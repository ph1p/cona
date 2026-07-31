//! Binary install / upgrade / uninstall + the ≤1×/day background
//! auto-update check (source rebuild, GitHub release binary, cargo fallback).

use super::{
    agents::agent_exe, cmd_agents, fetch_release_archive, release_target, Change, HELPER_EXE,
    USER_AGENT,
};
use crate::{db, ui};
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

/// `cona hooks install|uninstall` — project git hooks that re-index after
/// commit/merge/checkout (the self-upgrade variant is refresh_upgrade_hooks).
pub fn cmd_hooks(root: &Path, action: &str) -> Result<()> {
    // Every other user-facing command prints its own title; this one does too,
    // so a caller can never forget it. `cona hooks` adds a banner above it.
    println!("{}", ui::heading("git hooks"));
    let hooks_dir = root.join(".git").join("hooks");
    if !hooks_dir.exists() {
        bail!("no .git/hooks directory — is this a git repository?");
    }
    let names = ["post-commit", "post-merge", "post-checkout"];
    match action {
        "install" => {
            // absolute path so hooks work even when cona isn't on PATH
            let line = format!("{} index --quiet 2>/dev/null &", agent_exe());
            for n in names {
                append_hook_line(&hooks_dir.join(n), &line, "index --quiet")?;
            }
            println!(
                "{}",
                ui::ok(&format!(
                    "installed {} — index stays fresh automatically",
                    names.join(", ")
                ))
            );
        }
        "uninstall" => {
            strip_git_hook_lines(&hooks_dir, &["index --quiet", "installed by cona"]);
            println!("cona hooks removed");
        }
        _ => unreachable!(),
    }
    Ok(())
}

/// Strip lines containing any needle from the named git hooks; a hook reduced
/// to its shebang is deleted. Returns true when anything changed.
/// Needles identifying cona's index git-hook lines in a project repo.
const CONA_HOOK_NEEDLES: &[&str] = &["cona index", "installed by cona"];

/// Read-only twin of `strip_git_hook_lines`: does any hook file contain one of
/// the needles? Used to decide whether a project is worth announcing.
fn git_hooks_have(hooks_dir: &Path, needles: &[&str]) -> bool {
    ["post-commit", "post-merge", "post-checkout"]
        .iter()
        .any(|n| {
            std::fs::read_to_string(hooks_dir.join(n))
                .is_ok_and(|c| c.lines().any(|l| needles.iter().any(|nd| l.contains(nd))))
        })
}

fn strip_git_hook_lines(hooks_dir: &Path, needles: &[&str]) -> bool {
    let mut changed = false;
    for n in ["post-commit", "post-merge", "post-checkout"] {
        let p = hooks_dir.join(n);
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        let cleaned: String = content
            .lines()
            .filter(|l| !needles.iter().any(|needle| l.contains(needle)))
            .map(|l| format!("{l}\n"))
            .collect();
        if cleaned != content {
            if cleaned.trim() == "#!/bin/sh" || cleaned.trim().is_empty() {
                let _ = std::fs::remove_file(&p);
            } else {
                let _ = std::fs::write(&p, cleaned);
            }
            changed = true;
        }
    }
    changed
}

/// Byte-for-byte equality of two files (false if either can't be read).
fn files_identical(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn default_bin_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    Ok(home.join(".local").join("bin"))
}

/// Is `dir` a cona source checkout?
fn is_source_dir(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("Cargo.toml"))
        .map(|t| t.contains("name = \"cona\""))
        .unwrap_or(false)
}

/// Newest mtime (secs) of anything that affects the build.
fn source_mtime(dir: &Path) -> i64 {
    let mut newest = 0i64;
    let mut stamp = |p: &Path| {
        if let Ok(md) = std::fs::metadata(p) {
            if let Ok(m) = md.modified() {
                if let Ok(d) = m.duration_since(std::time::UNIX_EPOCH) {
                    newest = newest.max(d.as_secs() as i64);
                }
            }
        }
    };
    stamp(&dir.join("Cargo.toml"));
    stamp(&dir.join("Cargo.lock"));
    if let Ok(rd) = std::fs::read_dir(dir.join("src")) {
        for entry in rd.flatten() {
            stamp(&entry.path());
        }
    }
    newest
}

/// Whole-second mtime — deliberately coarser than `indexer::file_mtime`
/// (nanoseconds, needed for sub-second staleness); comparing binary vs source
/// checkout ages never needs that precision. Keep the two separate.
fn mtime_secs(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Atomically place `src` at `dst` (copy to temp sibling, rename over).
fn replace_binary(src: &Path, dst: &Path) -> Result<Change> {
    let existed = dst.exists();
    if existed && files_identical(src, dst) {
        return Ok(Change::Unchanged);
    }
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // per-process temp name: auto-update can run concurrently (background
    // `upgrade --quiet` + git-hook upgrade); a shared name lets two copiers
    // interleave into the same inode and rename a truncated binary into place
    let tmp = dst.with_extension(format!("tmp-update.{}", std::process::id()));
    std::fs::copy(src, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, dst)?;
    Ok(if existed {
        Change::Updated
    } else {
        Change::Created
    })
}

/// `cona install [--bin-dir DIR]`
/// Run inside the cona source checkout: builds (if needed), installs the
/// binary, records source/install paths, and wires git hooks in the source
/// repo so every commit/merge rebuilds the installed binary.
pub fn cmd_install(bin_dir: Option<&str>) -> Result<()> {
    println!("{}", ui::banner("cona install"));

    let cwd = std::env::current_dir()?;
    let src_root = {
        let mut d = cwd.as_path();
        loop {
            if is_source_dir(d) {
                break d.to_path_buf();
            }
            match d.parent() {
                Some(p) => d = p,
                None => bail!(
                    "not inside a cona source checkout — run `cona install` from the repo, \
                     or copy the binary manually"
                ),
            }
        }
    };

    let bin_dir = match bin_dir {
        Some(d) => PathBuf::from(shellexpand_home(d)?),
        None => default_bin_dir()?,
    };
    let dst = bin_dir.join("cona");
    let mut warnings = 0usize;

    println!("{}", ui::heading("binary"));
    let built = src_root.join("target/release/cona");
    if !built.exists() || mtime_secs(&built) < source_mtime(&src_root) {
        println!("  {}", ui::dim("building release binary …"));
        cargo_build(&src_root)?;
    }
    let verb = match replace_binary(&built, &dst)? {
        Change::Unchanged => "already current",
        Change::Created => "installed",
        Change::Updated => "updated",
    };
    println!("  {}", ui::ok(&format!("{verb} → {}", dst.display())));

    // Optional semantic-resolve helper: a separate crate (own tree-sitter 0.24
    // runtime — can't share cona's build). Build + install it beside cona
    // best-effort; failure is non-fatal (cona degrades to its heuristics).
    match install_resolve_helper(&src_root, &bin_dir) {
        Ok(Some(p)) => println!("  {}", ui::ok(&format!("resolve helper → {}", p.display()))),
        Ok(None) => {}
        Err(e) => {
            warnings += 1;
            println!(
                "  {}",
                ui::warn(&format!(
                    "resolve helper not built ({e}) — cona will use name-based + arity heuristics"
                ))
            );
        }
    }

    db::meta_set("source_dir", &src_root.to_string_lossy())?;
    db::meta_set("install_path", &dst.to_string_lossy())?;

    // git hooks in the SOURCE repo: rebuild+reinstall on every code change
    println!("\n{}", ui::heading("auto-rebuild"));
    if refresh_upgrade_hooks(&src_root, &dst)? {
        println!(
            "  {}",
            ui::ok("git hooks installed (post-commit/-merge/-checkout) — every commit rebuilds")
        );
    } else {
        warnings += 1;
        println!(
            "  {}",
            ui::warn("source repo has no .git — run `cona upgrade` manually after changes")
        );
    }

    if !on_path(&bin_dir) {
        warnings += 1;
        println!(
            "  {}",
            ui::warn(&format!(
                "{} is not on your PATH — add it, e.g. `export PATH=\"{}:$PATH\"`",
                bin_dir.display(),
                bin_dir.display()
            ))
        );
    }

    println!(
        "\n{}",
        ui::summary(warnings, "thing", "need attention", "install complete")
    );
    print_next_steps();
    Ok(())
}

/// Post-install guidance: what to run next and what it does.
fn print_next_steps() {
    println!("\n{}", ui::heading("next steps"));
    print!(
        "{}",
        ui::cmd_table(&[
            (
                "cona setup",
                "interactive setup — index this project + wire agent integration",
            ),
            (
                "cona setup project",
                "project only (git hooks, .claude/, CLAUDE.md, AGENTS.md, …)",
            ),
            (
                "cona setup global",
                "global only (~/.claude, ~/.codex, … home configs)",
            ),
            (
                "cona doctor",
                "verify the installation (binary, PATH, hooks, skill, index)",
            ),
        ])
    );
    println!(
        "\n{}",
        ui::dim(
            "run setup inside each project you want indexed — then agents pick it up automatically"
        )
    );
}

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
                            missing.display(),
                            live.display()
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
                        ui::ok(&format!("rebuilt — binary unchanged ({})", dst.display()))
                    ),
                    _ => println!("{}", ui::ok(&format!("updated → {}", dst.display()))),
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
                        ui::dim(&format!("updating source checkout {} …", src.display()))
                    );
                }
                update_source_checkout(&src, &remote, &dst, quiet)?;
            } else {
                install_release_binary(&remote, &dst)?;
            }
            if !quiet {
                println!("  {}", ui::ok(&format!("{} → v{remote}", dst.display())));
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
                        dst.display()
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
                        dst.display()
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
    let line = format!("{} upgrade --quiet &", dst.display());
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

    // global scope (~/.claude): refresh only if it already has cona config
    if crate::install::agents::project_has_cona(&home) && sync_scope_config(&home, true, quiet) {
        refreshed += 1;
    }

    // every registered project that carries cona config
    for p in db::registered_project_paths() {
        let root = Path::new(&p);
        if root.is_dir()
            && crate::install::agents::project_has_cona(root)
            && sync_scope_config(root, false, quiet)
        {
            refreshed += 1;
        }
    }

    if !quiet && refreshed > 0 {
        println!(
            "{}",
            ui::ok(&format!("config re-synced in {refreshed} scope(s)"))
        );
    }
}

/// Re-run the idempotent `agents install` in one scope and stamp it with the
/// running version. Returns whether the scope was targeted (regardless of
/// whether files actually moved) so callers can count. Errors are surfaced only
/// when not `quiet`.
fn sync_scope_config(root: &Path, global: bool, quiet: bool) -> bool {
    match crate::install::agents::cmd_agents_q(root, "install", &[], false, global, quiet) {
        Ok(_) => {
            let _ = db::meta_set(&config_ver_key(root), env!("CARGO_PKG_VERSION"));
            true
        }
        Err(e) => {
            if !quiet {
                println!(
                    "{}",
                    ui::warn(&format!(
                        "config not refreshed for {} ({e})",
                        root.display()
                    ))
                );
            }
            false
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
                src.display()
            );
        }
    }

    if !pulled && !quiet {
        println!(
            "note: git pull --ff-only failed in {} — rebuilding local state (wanted v{remote})",
            src.display()
        );
    }
    cargo_build(src)?;
    replace_binary(&src.join("target/release/cona"), dst)?;
    Ok(())
}

/// Update `dst` to release `ver`: prebuilt GitHub-release binary first,
/// `cargo install` from crates.io as fallback (private repo, exotic platform).
fn install_release_binary(ver: &str, dst: &Path) -> Result<Change> {
    match download_release_binary(ver, dst) {
        Ok(ch) => Ok(ch),
        Err(e) => install_via_cargo(ver, dst)
            .map_err(|e2| anyhow!("binary download failed ({e}); cargo fallback failed ({e2})")),
    }
}

/// Download the GitHub-release archive for `ver`, extract, replace `dst`.
fn download_release_binary(ver: &str, dst: &Path) -> Result<Change> {
    let target = release_target().ok_or_else(|| anyhow!("no prebuilt binary for this platform"))?;
    let tmp = std::env::temp_dir().join(format!("cona-update-{ver}"));
    fetch_release_archive(ver, target, &tmp)?;
    let bin = tmp.join(if cfg!(windows) { "cona.exe" } else { "cona" });
    let ch = replace_binary(&bin, dst)?;
    // the tarball bundles the optional resolve helper on targets where it
    // builds — install it beside cona too (best-effort, absence is fine)
    let helper = tmp.join(HELPER_EXE);
    if helper.is_file() {
        if let Some(dir) = dst.parent() {
            let _ = replace_binary(&helper, &dir.join(HELPER_EXE));
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(ch)
}

/// `cargo install cona@ver` into a temp root, then move the binary into
/// place — keeps the recorded install path instead of ~/.cargo/bin.
fn install_via_cargo(ver: &str, dst: &Path) -> Result<Change> {
    let root = std::env::temp_dir().join(format!("cona-cargo-{ver}"));
    let ok = std::process::Command::new("cargo")
        .args(["install", "--locked", "--root"])
        .arg(&root)
        .arg(format!("cona@{ver}"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        bail!("cargo install cona@{ver} failed");
    }
    let bin = root
        .join("bin")
        .join(if cfg!(windows) { "cona.exe" } else { "cona" });
    let ch = replace_binary(&bin, dst)?;
    let _ = std::fs::remove_dir_all(&root);
    Ok(ch)
}

/// `cona uninstall [--purge]`
/// Reverses `install`: removes upgrade git hooks from the source repo,
/// global agent files, the installed binary and the recorded paths.
/// `--purge` additionally deletes ~/.cona (all indexes + stats).
/// Which parts of a cona install to tear down. Built either from the
/// interactive checklist or (non-interactive) from flags + safe defaults.
struct UninstallPlan {
    agents: bool, // per-project + global agent configs & git hooks
    binary: bool, // the installed binary
    purge: bool,  // delete ~/.cona (indexes + stats)
}

pub fn cmd_uninstall(purge: bool, yes: bool) -> Result<()> {
    use std::io::IsTerminal;
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;

    let interactive = !yes && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    println!("{}", ui::banner("cona uninstall"));

    let plan = if interactive {
        // Only offer what's actually present, so the checklist reflects reality.
        let has_agents = db::registered_project_paths()
            .iter()
            .any(|p| super::agents::project_has_cona(Path::new(p)))
            || super::agents::project_has_cona(&home);
        let has_binary = matches!(db::meta_get("install_path")?, Some(d) if Path::new(&d).exists());
        let cona_dir = home.join(".cona");
        let rows = vec![
            ui::Row::Item(
                "agents",
                "cona from all agent configs + git hooks",
                has_agents,
            ),
            ui::Row::Item("binary", "the installed cona executable", has_binary),
            ui::Row::Item(
                "data",
                "delete ~/.cona (indexes + stats, irreversible)",
                false,
            ),
        ];
        match ui::multiselect("what should cona remove?", &rows)? {
            None => {
                println!("{}", ui::dim("cancelled — nothing removed"));
                return Ok(());
            }
            Some(picked) => {
                // rows are, in order: 0 agents, 1 binary, 2 data
                let plan = UninstallPlan {
                    agents: picked.contains(&0),
                    binary: picked.contains(&1),
                    purge: picked.contains(&2),
                };
                // Guard the irreversible one behind an explicit confirm.
                if plan.purge
                    && cona_dir.exists()
                    && !ui::confirm(&format!("delete {} for good?", cona_dir.display()))
                {
                    println!("{}", ui::warn("aborted — nothing removed"));
                    return Ok(());
                }
                plan
            }
        }
    } else {
        // Non-interactive: full teardown; ~/.cona only with explicit --purge.
        UninstallPlan {
            agents: true,
            binary: true,
            purge,
        }
    };

    let mut removed = 0usize;
    if plan.agents {
        removed += remove_all_agents(&home)?;
    }
    if plan.binary {
        println!("\n{}", ui::heading("binary"));
        removed += remove_binary()?;
    }
    // Drop the recorded paths whenever we tore down the install proper.
    if plan.agents || plan.binary {
        db::meta_del("source_dir")?;
        db::meta_del("install_path")?;
    }
    if plan.purge {
        println!("\n{}", ui::heading("data"));
        let d = home.join(".cona");
        if d.exists() {
            std::fs::remove_dir_all(&d)?;
            println!("{}", ui::ok(&format!("purged  {}", d.display())));
            removed += 1;
        } else {
            println!("{}", ui::item("~/.cona already gone"));
        }
    } else if plan.agents || plan.binary {
        println!(
            "\n{}",
            ui::dim("kept ~/.cona (indexes + stats) — pass --purge to delete")
        );
    }

    println!(
        "\n{}",
        ui::ok(&format!(
            "uninstall complete — {removed} item{} removed",
            if removed == 1 { "" } else { "s" }
        ))
    );
    Ok(())
}

/// Strip cona from every registered project (agent files + git hooks) and the
/// global home configs. Returns how many targets were actually touched.
fn remove_all_agents(home: &Path) -> Result<usize> {
    let mut removed = 0usize;

    // upgrade hooks in the source repo (incl. legacy `self-update` lines)
    if let Ok(Some(src)) = db::meta_get("source_dir") {
        if strip_git_hook_lines(
            &Path::new(&src).join(".git/hooks"),
            &["self-update", "upgrade --quiet", "installed by cona"],
        ) {
            println!("{}", ui::item(&format!("upgrade git hooks   {src}")));
            removed += 1;
        }
    }

    // every registered project: agent files + git hooks
    let mut touched = 0usize;
    for p in db::registered_project_paths() {
        let root = Path::new(&p);
        if !root.is_dir() {
            continue;
        }
        // Skip registered-but-clean projects entirely — otherwise every one
        // floods the output with an empty heading + "nothing to do".
        if !super::agents::project_has_cona(root)
            && !git_hooks_have(&root.join(".git/hooks"), CONA_HOOK_NEEDLES)
        {
            continue;
        }
        println!("\n{}", ui::heading(&format!("project {p}")));
        match cmd_agents(root, "uninstall", &[], false, false) {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(e) => println!("{}", ui::warn(&e.to_string())),
        }
        if strip_git_hook_lines(&root.join(".git/hooks"), CONA_HOOK_NEEDLES) {
            println!("{}", ui::item("git hooks removed"));
            removed += 1;
        }
        touched += 1;
    }
    if touched == 0 {
        println!("\n{}", ui::dim("no per-project integration found"));
    }

    // global agent integration
    println!("\n{}", ui::heading("global"));
    if cmd_agents(home, "uninstall", &[], false, true)? {
        removed += 1;
    }
    Ok(removed)
}

/// Remove the recorded installed binary. Returns 1 if a file was deleted.
fn remove_binary() -> Result<usize> {
    match db::meta_get("install_path")? {
        // unlinking a running binary is fine on unix
        Some(dst) if Path::new(&dst).exists() => {
            if std::fs::remove_file(&dst).is_ok() {
                println!("{}", ui::ok(&format!("removed  {dst}")));
                Ok(1)
            } else {
                println!("{}", ui::warn(&format!("could not remove {dst}")));
                Ok(0)
            }
        }
        Some(dst) => {
            println!("{}", ui::item(&format!("already gone  {dst}")));
            Ok(0)
        }
        None => {
            println!("{}", ui::dim("no installed binary recorded"));
            Ok(0)
        }
    }
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
    maybe_refresh_scope(project_root, false);
    if let Some(home) = dirs::home_dir() {
        if home != project_root {
            maybe_refresh_scope(&home, true);
        }
    }
}

/// Version-gated passive re-sync of ONE scope's config to the running binary.
/// Cheap sqlite read + string compare on the hot path; fs scan + write only on
/// the rare version mismatch. Fully silent (query path never speaks).
fn maybe_refresh_scope(root: &Path, global: bool) {
    let recorded = db::meta_get(&config_ver_key(root)).ok().flatten();
    if recorded.as_deref() == Some(env!("CARGO_PKG_VERSION")) {
        return; // already in sync with this binary — one sqlite read, done
    }
    // Version differs (or never recorded): only touch a scope the user opted
    // into. project_has_cona's fs scan runs at most once per version change.
    if !crate::install::agents::project_has_cona(root) {
        return;
    }
    let _ = sync_scope_config(root, global, true);
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

fn cargo_build(dir: &Path) -> Result<()> {
    let status = std::process::Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(dir)
        .status()
        .context("failed to run cargo — is Rust installed?")?;
    if !status.success() {
        bail!("cargo build failed");
    }
    Ok(())
}

/// Build the standalone `resolve-helper` crate and install its binary beside
/// cona. Returns the installed path, or `Ok(None)` if the crate isn't in the
/// checkout. Errors (build failed, no cargo) are surfaced to the caller as
/// non-fatal — the helper is optional. Only rebuilds when the binary is stale.
fn install_resolve_helper(src_root: &Path, bin_dir: &Path) -> Result<Option<PathBuf>> {
    let crate_dir = src_root.join("src/resolve-helper");
    if !crate_dir.join("Cargo.toml").is_file() {
        return Ok(None); // not present in this checkout
    }
    let built = crate_dir.join("target/release").join(HELPER_EXE);
    if !built.exists() || mtime_secs(&built) < source_mtime(&crate_dir) {
        println!("building resolve helper (semantic resolution) …");
        cargo_build(&crate_dir)?;
    }
    if !built.exists() {
        bail!("helper binary not produced");
    }
    let dst = bin_dir.join(HELPER_EXE);
    replace_binary(&built, &dst)?;
    Ok(Some(dst))
}

pub(crate) fn shellexpand_home(p: &str) -> Result<String> {
    if let Some(rest) = p.strip_prefix("~/") {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
        return Ok(home.join(rest).to_string_lossy().to_string());
    }
    Ok(p.to_string())
}

pub(crate) fn on_path(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == dir))
        .unwrap_or(false)
}

/// Append a marked line to a git hook script (create if missing), idempotent.
fn append_hook_line(hook: &Path, line: &str, marker: &str) -> Result<()> {
    if hook.exists() {
        let existing = std::fs::read_to_string(hook).unwrap_or_default();
        if existing.contains(marker) {
            return Ok(());
        }
        std::fs::write(hook, format!("{existing}\n{line}\n"))?;
    } else {
        std::fs::write(hook, format!("#!/bin/sh\n# installed by cona\n{line}\n"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parse_and_compare() {
        assert_eq!(parse_semver("0.1.1"), Some((0, 1, 1)));
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_semver("nope"), None);
        assert!(remote_is_newer("0.1.2", "0.1.1"));
        assert!(remote_is_newer("0.2.0", "0.1.9"));
        assert!(!remote_is_newer("0.1.1", "0.1.1"));
        assert!(!remote_is_newer("0.1.0", "0.1.1"));
        assert!(!remote_is_newer("garbage", "0.1.1"));
    }

    #[test]
    fn git_hooks_have_detects_cona_lines() {
        let dir = std::env::temp_dir().join("cona-hookhave-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!git_hooks_have(&dir, CONA_HOOK_NEEDLES));
        std::fs::write(
            dir.join("post-commit"),
            "#!/bin/sh\nexec cona index --quiet\n",
        )
        .unwrap();
        assert!(git_hooks_have(&dir, CONA_HOOK_NEEDLES));
        // a foreign hook must not match
        std::fs::write(dir.join("post-commit"), "#!/bin/sh\nmake lint\n").unwrap();
        assert!(!git_hooks_have(&dir, CONA_HOOK_NEEDLES));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tree_dirty_tracks_working_changes() {
        let repo = std::env::temp_dir().join("cona-dirtytest-repo");
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        if !git_ok(&repo, &["init", "--quiet"]) {
            return; // no usable git in this environment — skip
        }
        git_ok(&repo, &["config", "user.email", "t@t"]);
        git_ok(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "one\n").unwrap();
        git_ok(&repo, &["add", "f.txt"]);
        git_ok(&repo, &["commit", "--quiet", "-m", "init"]);

        // Clean tree after commit.
        assert!(!tree_dirty(&repo));
        // Modify a tracked file → dirty.
        std::fs::write(repo.join("f.txt"), "two\n").unwrap();
        assert!(tree_dirty(&repo));

        let _ = std::fs::remove_dir_all(&repo);
    }
}
