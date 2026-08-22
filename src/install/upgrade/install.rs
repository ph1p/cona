//! `cona install` — first install from a source checkout.

use super::*;
use crate::install::Change;
use crate::{db, ui};
use anyhow::{bail, Result};
use std::path::PathBuf;

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
    println!(
        "  {}",
        ui::ok(&format!("{verb} → {}", crate::install::short_path(&dst)))
    );

    // Optional semantic-resolve helper: a separate crate (own tree-sitter 0.24
    // runtime — can't share cona's build). Build + install it beside cona
    // best-effort; failure is non-fatal (cona degrades to its heuristics).
    match install_resolve_helper(&src_root, &bin_dir) {
        Ok(Some(p)) => println!(
            "  {}",
            ui::ok(&format!(
                "resolve helper → {}",
                crate::install::short_path(&p)
            ))
        ),
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
                // The prose half shortens; the `export` line must stay
                // absolute — it is meant to be copied into a shell rc, where
                // `~` may not expand and `./` means something else entirely.
                "{} is not on your PATH — add it, e.g. `export PATH=\"{}:$PATH\"`",
                crate::install::short_path(&bin_dir),
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
pub(super) fn print_next_steps() {
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
