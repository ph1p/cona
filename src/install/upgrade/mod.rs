//! Binary install / upgrade / uninstall + the ≤1×/day background
//! auto-update check (source rebuild, GitHub release binary, cargo fallback).

use super::{Change, USER_AGENT};
use crate::{db, ui};
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

mod binary;
mod hooks;
mod install;
#[cfg(test)]
mod tests;
mod uninstall;

pub use binary::*;
pub use hooks::*;
pub use install::*;
pub use uninstall::*;

/// `cona upgrade [--quiet]` — rebuild from the recorded source
/// checkout if it is newer than the installed binary, otherwise check
/// crates.io for a newer release and install the prebuilt binary.
pub fn cmd_upgrade(quiet: bool) -> Result<()> {
    if !quiet {
        println!("{}", ui::banner("cona upgrade"));
    }

    // Prefer the recorded install path, but only if it still exists. A stale
    // meta row (e.g. a temp extraction dir from a `curl|sh` install that ran
    // the binary out of `/var/folders/…tmp…/`) would otherwise make every
    // upgrade rebuild correctly then copy the new binary into a dead path —
    // the on-PATH binary never moves. Fall back to `current_exe()` and heal
    // the meta so the next run is clean.
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

    // 2. Remote release newer than this build → update. A source checkout
    //    stays the source of truth (git pull + rebuild — never overwrite a
    //    dev build with a release binary); otherwise prebuilt binary.
    let current = env!("CARGO_PKG_VERSION");
    match latest_remote_version() {
        Some(remote) if remote_is_newer(&remote, current) => {
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
        Some(_) => {
            if !quiet {
                println!(
                    "{}",
                    ui::ok(&format!(
                        "already up to date — v{current} ({})",
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

/// `X.Y.Z` (optional `v` prefix, pre-release/build suffix on the patch
/// component tolerated) → comparable triple.
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

/// Newest non-yanked version on crates.io, via the sparse index (built for
/// tooling, no rate-limited API). Fail-open: any error → None.
fn latest_remote_version() -> Option<String> {
    let out = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "5",
            "-A",
            USER_AGENT,
            "https://index.crates.io/co/na/cona",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout)
        .ok()?
        .lines()
        .rev()
        .find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v["yanked"].as_bool().unwrap_or(true) {
                return None;
            }
            Some(v["vers"].as_str()?.to_string())
        })
}

/// Rewrite the upgrade git hooks in the source repo: strip legacy lines
/// (older installs called `self-update`) and (re-)append the current
/// `upgrade --quiet` line. Returns false when the repo has no `.git`.
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

/// Per-scope meta key recording which binary version last wrote the config in
/// that path. Config only goes stale when the binary's baked-in `include_str!`
/// content changes — i.e. on a version change — so the version is the freshness
/// signal (self-limiting: one refresh per scope per version, catches downgrades
/// via `!=`, no wall-clock timer, no unbounded per-visit meta rows).
pub(crate) fn config_ver_key(path: &Path) -> String {
    format!("config_ver:{}", path.to_string_lossy())
}

/// After a binary swap the SKILL.md / guide / hook blocks baked into the *old*
/// binary may be stale (they're `include_str!`ed at build time). Re-run the
/// idempotent `agents install` in every scope that already carries cona
/// config so the installed integration matches the new binary, then stamp each
/// scope with the running version. Marker-based + `write_if_changed` → a no-op
/// wherever content is already current, so this is cheap to call after every
/// upgrade.
fn refresh_config(quiet: bool) {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return,
    };
    let mut refreshed = 0usize;
    // ONE loop over every scope: the global (~/.claude) scope, then every
    // registered project. `sync_scope_config` itself decides whether a scope
    // has anything installed — no pre-gate, that would double the fs scan.
    let projects = db::registered_project_paths();
    let scopes = std::iter::once((home.clone(), true))
        .chain(projects.into_iter().map(|p| (PathBuf::from(p), false)));
    for (root, global) in scopes {
        if !global && !root.is_dir() {
            continue;
        }
        if let Some(names) = sync_scope_config(&root, &home, global, quiet) {
            if !quiet {
                // heading prints lazily, only once something actually refreshes
                // — a run where every scope is empty stays silent
                if refreshed == 0 {
                    println!("\n{}", ui::heading("config refresh"));
                }
                let list: Vec<&str> = names.iter().map(|n| n.slug()).collect();
                println!(
                    "  {}",
                    ui::dim(&format!(
                        "{} — {}",
                        crate::install::short_path(&root),
                        list.join(", ")
                    ))
                );
            }
            refreshed += 1;
        }
    }

    if !quiet && refreshed > 0 {
        println!(
            "{}",
            ui::ok(&format!("config refreshed in {refreshed} scope(s)"))
        );
    }
}

/// Re-run the idempotent `agents install` in one scope and stamp it with the
/// running version. Targets ONLY the agents already installed there
/// (`installed_agents`) — a bare install would autodetect and add config for
/// agents the user never selected. Returns the refreshed agent set, `None`
/// when the scope had nothing installed or the install failed (surfaced only
/// when not `quiet`).
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
    // Always quiet: refresh can span dozens of registered projects, and the
    // per-scope install block ("· N already current" + "✓ agents installed")
    // would bury the upgrade result under one screen of noise per project.
    // `refresh_config` owns the display — one dim line per scope.
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

/// True if the checkout has uncommitted changes to tracked files (staged or
/// unstaged). A clean tree returns false. A missing/broken git or non-repo
/// makes `git diff` exit nonzero → reported dirty, but the follow-up
/// `git stash push` then fails, so `update_source_checkout` won't try to pop.
fn tree_dirty(src: &Path) -> bool {
    !git_ok(src, &["diff", "--quiet"]) || !git_ok(src, &["diff", "--cached", "--quiet"])
}

/// Source-install update: `git pull --ff-only` the checkout, rebuild. A dirty
/// tree (e.g. a release-plz version bump touching Cargo.lock/toml, or a prior
/// build) would otherwise abort the ff-only pull, so tracked changes are
/// stashed around the pull and popped back after. If the pop conflicts the
/// stash is LEFT in place (user's work is never lost) and we warn. If the pull
/// still can't fast-forward (local commits, no git) the checkout is left
/// untouched and we rebuild whatever is there — a dev build is never replaced
/// by a release binary.
fn update_source_checkout(src: &Path, remote: &str, dst: &Path, quiet: bool) -> Result<()> {
    // heal stale hook lines (e.g. legacy `self-update`) BEFORE the pull
    // fires post-merge, so the hook never invokes a removed subcommand
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

/// Called at the start of every normal command: if we ARE the installed
/// binary and the recorded source checkout is newer, kick off a background
/// rebuild. Cheap (one sqlite read + a few stats); never blocks the command.
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

    // Keep THIS project's config in step with the running binary: a globally
    // upgraded binary carries newer SKILL/guide/hooks than a project that was
    // installed against an older build. Idempotent + daily-gated so it's a
    // no-op once synced. Only touches a project that already has cona config.
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

/// At most one background remote version check per day; the timestamp is
/// bumped up-front so concurrent commands can't stampede.
/// Re-sync the current project's config to the running binary when its recorded
/// version differs (a globally-upgraded binary carries newer baked SKILL/guide/
/// hooks than a project last installed against an older build). Version-gated,
/// not timed: the cheap sqlite read runs first, so the hot path pays one meta
/// lookup + string compare on every command and only does the fs scan + write
/// on the rare version mismatch. Fully silent — auto-refresh never speaks on the
/// query path.
fn maybe_refresh_project_config(project_root: &Path) {
    // Heal the current project AND the global (~/.claude) scope. A binary swapped
    // outside `cona upgrade` (cargo-install, manual copy) never runs refresh_config,
    // so without a passive global heal ~/.claude stays pinned to the old version
    // until the user manually re-runs setup/upgrade.
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
    // Version differs (or never recorded): sync_scope_config scans the scope
    // itself (installed_agents) and touches only what's already installed —
    // an empty scope just gets stamped so the probe stays one sqlite read.
    let _ = sync_scope_config(root, home, global, true);
}

fn remote_check_due() -> bool {
    let now = db::now();
    let last = db::meta_get("last_remote_check")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    if now - last < 86_400 {
        return false;
    }
    db::meta_set("last_remote_check", &now.to_string()).is_ok()
}
