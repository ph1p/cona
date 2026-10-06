//! Binary install / upgrade / uninstall + the ≤1×/day background
//! auto-update check (source rebuild, GitHub release binary, cargo fallback).

use super::{Change, GITHUB_REPO, USER_AGENT};
use crate::{db, ui};
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

mod binary;
mod hooks;
mod install;
#[cfg(test)]
mod tests;
mod uninstall;

pub(crate) use binary::*;
pub use hooks::*;
pub use install::*;
pub use uninstall::*;

/// `cona upgrade [--quiet]` — rebuild from the source checkout if newer than
/// the binary, else install a newer release if one exists.
pub fn cmd_upgrade(quiet: bool) -> Result<()> {
    if !quiet {
        println!("{}", ui::banner("cona upgrade"));
    }

    // Prefer the recorded install path only if it still exists. A stale row
    // (e.g. a `curl|sh` temp dir) would copy every upgrade into a dead path
    // while the on-PATH binary never moves. Fall back to `current_exe()` and
    // heal the meta.
    let recorded = db::meta_get("install_path")?.map(PathBuf::from);
    let dst = match recorded {
        Some(p) if p.exists() => p,
        stale => {
            let live = std::env::current_exe()
                .map_err(|_| anyhow!("no install path recorded — run `cona install` first"))?;
            if let Some(missing) = &stale {
                if !quiet {
                    println!(
                        "{}",
                        ui::warn(&format!(
                            "recorded install path missing ({}) — using {}",
                            crate::install::short_path(missing),
                            crate::install::short_path(&live)
                        ))
                    );
                }
            }
            db::meta_set("install_path", &live.to_string_lossy())?;
            live
        }
    };

    // 1. Local dev workflow: source checkout newer than the binary → rebuild.
    if let Some(src) = db::meta_get("source_dir")?.map(PathBuf::from) {
        if is_source_dir(&src) && source_mtime(&src) > mtime_secs(&dst) {
            if !quiet {
                println!("{}", ui::dim("sources changed — rebuilding …"));
            }
            cargo_build(&src)?;
            let ch = replace_binary(&src.join("target/release/cona"), &dst)?;
            if !quiet {
                match ch {
                    Change::Unchanged => println!(
                        "{}",
                        ui::ok(&format!(
                            "rebuilt — binary unchanged ({})",
                            crate::install::short_path(&dst)
                        ))
                    ),
                    _ => println!(
                        "{}",
                        ui::ok(&format!("updated → {}", crate::install::short_path(&dst)))
                    ),
                }
            }
            // keep the sibling resolve helper in step (best-effort, optional)
            if let Some(bin_dir) = dst.parent() {
                if let Err(e) = install_resolve_helper(&src, bin_dir) {
                    if !quiet {
                        println!("{}", ui::warn(&format!("resolve helper not rebuilt ({e})")));
                    }
                }
            }
            if ch != Change::Unchanged {
                refresh_config(quiet);
            }
            if !quiet {
                println!("\n{}", ui::ok(&ui::bold("up to date")));
            }
            return Ok(());
        }
    }

    // 2. Remote release newer → update. A source checkout stays the source of
    //    truth (git pull + rebuild, never a release binary over a dev build).
    let current = env!("CARGO_PKG_VERSION");
    let latest = latest_remote_version();
    if latest.is_some() {
        // Only a check that ANSWERED counts toward the daily gate — a failed
        // one (offline, timeout) is retried by the next session, not tomorrow.
        let _ = db::meta_set("last_remote_check", &db::now().to_string());
    }
    match latest {
        Some((remote, _)) if remote_is_newer(&remote, current) => {
            if !quiet {
                println!(
                    "{}",
                    ui::heading(&format!("new release v{remote} (installed v{current})"))
                );
            }
            let src = db::meta_get("source_dir")?
                .map(PathBuf::from)
                .filter(|s| is_source_dir(s));
            if let Some(src) = src {
                if !quiet {
                    println!(
                        "  {}",
                        ui::dim(&format!(
                            "updating source checkout {} …",
                            crate::install::short_path(&src)
                        ))
                    );
                }
                update_source_checkout(&src, &remote, &dst, quiet)?;
            } else {
                install_release_binary(&remote, &dst)?;
            }
            if !quiet {
                println!(
                    "  {}",
                    ui::ok(&format!("{} → v{remote}", crate::install::short_path(&dst)))
                );
            }
            refresh_config(quiet);
            if !quiet {
                println!("\n{}", ui::ok(&ui::bold("upgrade complete")));
            }
        }
        Some((remote, source)) => {
            if !quiet {
                println!(
                    "{}",
                    ui::ok(&format!(
                        "already up to date — v{current} ({}; latest on {source}: v{remote})",
                        crate::install::short_path(&dst)
                    ))
                );
            }
        }
        None => {
            if !quiet {
                println!(
                    "{}",
                    ui::warn(&format!(
                        "v{current} ({}) — remote version check unavailable",
                        crate::install::short_path(&dst)
                    ))
                );
            }
        }
    }
    Ok(())
}

// ---------- remote release check ----------

/// `X.Y.Z` (optional `v`, patch suffix tolerated) → comparable triple.
fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches('v').split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch: String = it
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    Some((major, minor, patch.parse().ok()?))
}

fn remote_is_newer(remote: &str, current: &str) -> bool {
    matches!(
        (parse_semver(remote), parse_semver(current)),
        (Some(r), Some(c)) if r > c
    )
}

/// Newest release and where it was seen. GitHub first: binaries come from
/// there and `cargo publish` lands only after them, so crates.io alone said
/// "up to date" during that gap. Sparse index is the fallback. Fail-open.
fn latest_remote_version() -> Option<(String, &'static str)> {
    let url = format!("https://github.com/{GITHUB_REPO}/releases/latest");
    if let Some(v) = curl(&["-fsSI", &url])
        .as_deref()
        .and_then(tag_from_redirect)
    {
        return Some((v, "GitHub"));
    }
    let index = curl(&["-fsSL", "https://index.crates.io/co/na/cona"])?;
    Some((newest_in_index(&index)?, "crates.io"))
}

/// stdout of a 5 s curl, None on any failure.
fn curl(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("curl")
        .args(["--max-time", "5", "-A", USER_AGENT])
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).ok())?
}

/// `releases/latest` answers with a redirect to `…/releases/tag/vX.Y.Z`;
/// the version is the tag in its `location` header.
fn tag_from_redirect(headers: &str) -> Option<String> {
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if !k.trim().eq_ignore_ascii_case("location") {
            return None;
        }
        let tag = v.trim().rsplit_once("/tag/")?.1;
        parse_semver(tag)?;
        Some(tag.trim_start_matches('v').to_string())
    })
}

/// Highest non-yanked, non-pre-release version in a sparse-index file — by
/// version, not line order (a late patch to an older series is the last line).
fn newest_in_index(index: &str) -> Option<String> {
    index
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v["yanked"].as_bool().unwrap_or(true) {
                return None;
            }
            let vers = v["vers"].as_str()?;
            if vers.contains('-') {
                return None;
            }
            Some((parse_semver(vers)?, vers.to_string()))
        })
        .max_by_key(|(key, _)| *key)
        .map(|(_, vers)| vers)
}

/// Rewrite the source repo's upgrade git hooks: strip legacy `self-update`
/// lines, (re-)append `upgrade --quiet`. False when there is no `.git`.
fn refresh_upgrade_hooks(src_root: &Path, dst: &Path) -> Result<bool> {
    let hooks_dir = src_root.join(".git/hooks");
    if !hooks_dir.exists() {
        return Ok(false);
    }
    strip_git_hook_lines(&hooks_dir, &["self-update"]);
    let line = format!(
        "{} upgrade --quiet &",
        crate::install::sh_quote(&dst.display().to_string())
    );
    for n in ["post-commit", "post-merge", "post-checkout"] {
        append_hook_line(&hooks_dir.join(n), &line, "upgrade --quiet")?;
    }
    Ok(true)
}

/// Per-scope meta key: which binary version last wrote that path's config.
/// Baked-in `include_str!` content only changes with the version, so the
/// version is the freshness signal (one refresh per scope per version,
/// downgrades caught via `!=`, no timer).
pub(crate) fn config_ver_key(path: &Path) -> String {
    format!("config_ver:{}", path.to_string_lossy())
}

/// After a binary swap the SKILL.md / guide / hook blocks baked into the old
/// binary may be stale. Re-run the idempotent `agents install` in every scope
/// that carries cona config and stamp it with the running version. A no-op
/// where content is current, so cheap after every upgrade.
fn refresh_config(quiet: bool) {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return,
    };
    let (mut home_refreshed, mut projects_refreshed) = (false, 0usize);
    // ONE loop: global scope, then every registered project.
    // `sync_scope_config` decides if a scope has anything — a pre-gate would
    // double the fs scan.
    let projects = db::registered_project_paths();
    let scopes = std::iter::once((home.clone(), true))
        .chain(projects.into_iter().map(|p| (PathBuf::from(p), false)));
    for (root, global) in scopes {
        if !global && !root.is_dir() {
            continue;
        }
        if let Some(names) = sync_scope_config(&root, &home, global, quiet) {
            if !quiet {
                // heading prints lazily, so an all-empty run stays silent
                if !home_refreshed && projects_refreshed == 0 {
                    println!("\n{}", ui::heading("config refresh"));
                }
                // Say WHICH scope in words: a bare `~` or `.` reads as noise.
                let scope = if global {
                    "home configs".to_string()
                } else {
                    format!("project {}", crate::install::short_path(&root))
                };
                let list: Vec<&str> = names.iter().map(|n| n.slug()).collect();
                println!("  {:<22} {}", scope, ui::dim(&list.join(", ")));
            }
            if global {
                home_refreshed = true;
            } else {
                projects_refreshed += 1;
            }
        }
    }

    if !quiet && (home_refreshed || projects_refreshed > 0) {
        let mut parts = Vec::new();
        if home_refreshed {
            parts.push("home".to_string());
        }
        if projects_refreshed > 0 {
            parts.push(ui::plural(projects_refreshed, "project"));
        }
        println!(
            "{}",
            ui::ok(&format!("agent configs refreshed ({})", parts.join(" + ")))
        );
    }
}

/// Re-run `agents install` in one scope and stamp the running version. Targets
/// ONLY already-installed agents — a bare install would autodetect and add
/// agents the user never chose. `None` when nothing was installed or the
/// install failed.
fn sync_scope_config(
    root: &Path,
    home: &Path,
    global: bool,
    quiet: bool,
) -> Option<Vec<crate::install::agents::AgentName>> {
    let names = crate::install::agents::installed_agents(root, home, global);
    if names.is_empty() {
        // nothing installed here — stamp so the version-gated probe stays cheap
        let _ = db::meta_set(&config_ver_key(root), env!("CARGO_PKG_VERSION"));
        return None;
    }
    // Always quiet: across dozens of projects the per-scope install block
    // would bury the upgrade result. `refresh_config` prints one line per scope.
    match crate::install::agents::cmd_agents_q(root, "install", &names, false, global, true) {
        Ok(_) => {
            let _ = db::meta_set(&config_ver_key(root), env!("CARGO_PKG_VERSION"));
            Some(names)
        }
        Err(e) => {
            if !quiet {
                println!(
                    "{}",
                    ui::warn(&format!(
                        "config not refreshed for {} ({e})",
                        crate::install::short_path(root)
                    ))
                );
            }
            None
        }
    }
}

/// Run a git subcommand in `src`, returning whether it exited 0. Never panics.
fn git_ok(src: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(src)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// True if tracked files have staged or unstaged changes. A broken git or
/// non-repo reports dirty, but then `git stash push` fails, so
/// `update_source_checkout` won't try to pop.
fn tree_dirty(src: &Path) -> bool {
    !git_ok(src, &["diff", "--quiet"]) || !git_ok(src, &["diff", "--cached", "--quiet"])
}

/// Source-install update: `git pull --ff-only`, rebuild. Tracked changes
/// (e.g. a release-plz bump of Cargo.lock) would abort the pull, so they are
/// stashed around it; a conflicting pop LEAVES the stash and warns. If the pull
/// can't fast-forward, rebuild what is there — a dev build is never replaced
/// by a release binary.
fn update_source_checkout(src: &Path, remote: &str, dst: &Path, quiet: bool) -> Result<()> {
    // heal stale hook lines BEFORE the pull fires post-merge, so the hook
    // never invokes a removed subcommand
    refresh_upgrade_hooks(src, dst)?;

    // Stash tracked changes so an ff-only pull isn't blocked by a dirty tree.
    let stashed = tree_dirty(src)
        && git_ok(
            src,
            &["stash", "push", "--quiet", "-m", "cona-upgrade autostash"],
        );

    let pulled = git_ok(src, &["pull", "--ff-only", "--quiet"]);

    if stashed {
        // Restore the caller's changes. On conflict the stash entry survives.
        let popped = git_ok(src, &["stash", "pop", "--quiet"]);
        if !popped && !quiet {
            println!(
                "note: could not reapply autostash in {} — your changes are kept in `git stash` (run `git stash pop` after resolving)",
                crate::install::short_path(src)
            );
        }
    }

    if !pulled && !quiet {
        println!(
            "note: git pull --ff-only failed in {} — rebuilding local state (wanted v{remote})",
            crate::install::short_path(src)
        );
    }
    cargo_build(src)?;
    replace_binary(&src.join("target/release/cona"), dst)?;
    Ok(())
}

/// Start of every normal command: if we ARE the installed binary and the
/// source checkout is newer, start a background rebuild. Never blocks.
pub fn maybe_auto_update(project_root: &Path) {
    let Ok(Some(dst)) = db::meta_get("install_path") else {
        return;
    };
    let dst = PathBuf::from(dst);
    let me = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_default();
    if me != dst.canonicalize().unwrap_or_default() {
        return; // running some other build (e.g. cargo run) — don't touch it
    }

    // Keep THIS project's config in step with the running binary. Idempotent
    // and version-gated; only touches a project that already has cona config.
    maybe_refresh_project_config(project_root);
    let source_changed = match db::meta_get("source_dir") {
        Ok(Some(src)) => {
            let src = PathBuf::from(src);
            is_source_dir(&src) && source_mtime(&src) > mtime_secs(&dst)
        }
        _ => false,
    };
    if source_changed {
        use std::io::IsTerminal;
        if std::io::stderr().is_terminal() {
            eprintln!("cona: sources changed — upgrading in background");
        }
    } else if !remote_check_due() {
        return;
    }
    let _ = std::process::Command::new(&dst)
        .args(["upgrade", "--quiet"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Re-sync the current project's config to the running binary when its
/// recorded version differs. Fully silent — the query path never speaks.
fn maybe_refresh_project_config(project_root: &Path) {
    // Heal the global scope too: a binary swapped outside `cona upgrade`
    // (cargo install, manual copy) never runs refresh_config.
    let Some(home) = dirs::home_dir() else {
        return;
    };
    maybe_refresh_scope(project_root, &home, false);
    if home != project_root {
        maybe_refresh_scope(&home, &home, true);
    }
}

/// Version-gated passive re-sync of ONE scope's config to the running binary.
/// Cheap sqlite read + string compare on the hot path; fs scan + write only on
/// the rare version mismatch. Fully silent (query path never speaks).
fn maybe_refresh_scope(root: &Path, home: &Path, global: bool) {
    let recorded = db::meta_get(&config_ver_key(root)).ok().flatten();
    if recorded.as_deref() == Some(env!("CARGO_PKG_VERSION")) {
        return; // already in sync with this binary — one sqlite read, done
    }
    // sync_scope_config touches only what's installed; an empty scope just
    // gets stamped so the probe stays one sqlite read.
    let _ = sync_scope_config(root, home, global, true);
}

/// At most one background remote check per day; the attempt is stamped
/// up-front so concurrent commands can't stampede.
fn remote_check_due() -> bool {
    let read = |k| {
        db::meta_get(k)
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
    };
    let now = db::now();
    if !check_due(now, read("last_remote_check"), read("last_remote_attempt")) {
        return false;
    }
    db::meta_set("last_remote_attempt", &now.to_string()).is_ok()
}

/// Daily after a check that answered (`last_ok`, stamped by `cmd_upgrade`),
/// hourly while they fail (`last_attempt`), so an offline start costs no day.
fn check_due(now: i64, last_ok: i64, last_attempt: i64) -> bool {
    now - last_ok >= 86_400 && now - last_attempt >= 3_600
}
