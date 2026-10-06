//! `cona uninstall` — interactive/flagged teardown.

use super::*;
use crate::install::{cmd_agents, short_path, HELPER_EXE};
use crate::{db, ui};
use anyhow::{anyhow, Result};
use std::path::Path;

/// Which parts of a cona install to tear down: from the interactive checklist,
/// or from flags + safe defaults.
pub(super) struct UninstallPlan {
    agents: bool, // per-project + global agent configs & git hooks
    binary: bool, // the installed binary
    purge: bool,  // delete ~/.cona (indexes + stats)
}

/// `cona uninstall [--purge]` — reverses `install`: upgrade git hooks, agent
/// files, binary, recorded paths. `--purge` also deletes ~/.cona.
pub fn cmd_uninstall(purge: bool, yes: bool) -> Result<()> {
    use std::io::IsTerminal;
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;

    let interactive = !yes && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    // print!, not println!: every section below opens with its own "\n▸"
    print!("{}", ui::banner("cona uninstall"));

    let plan = if interactive {
        // Only offer what's actually present, so the checklist reflects reality.
        let has_agents = db::registered_project_paths()
            .iter()
            .any(|p| crate::install::agents::project_has_cona(Path::new(p)))
            || crate::install::agents::project_has_cona(&home);
        let has_binary = matches!(db::meta_get("install_path")?, Some(d) if Path::new(&d).exists());
        let cona_dir = home.join(".cona");
        let rows = vec![
            ui::Row::Item(
                "agents",
                "cona from all agent configs + git hooks".into(),
                has_agents,
            ),
            ui::Row::Item("binary", "the installed cona executable".into(), has_binary),
            ui::Row::Item(
                "data",
                "delete ~/.cona (indexes + stats, irreversible)".into(),
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

    // What was cleaned, named — a bare count mixes projects, scopes and files.
    let mut removed: Vec<String> = Vec::new();
    if plan.agents {
        removed.extend(remove_all_agents(&home)?);
    }
    if plan.binary {
        println!("\n{}", ui::heading("binary"));
        if remove_binary()? > 0 {
            removed.push("binary".into());
        }
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
            println!("{}", ui::ok(&format!("purged  {}", short_path(&d))));
            removed.push("~/.cona".into());
        } else {
            println!("{}", ui::item("~/.cona already gone"));
        }
    } else if plan.agents || plan.binary {
        println!(
            "\n{}",
            ui::dim("kept ~/.cona (indexes + stats) — pass --purge to delete")
        );
    }

    let tail = if removed.is_empty() {
        "nothing was installed".to_string()
    } else {
        format!("removed {}", removed.join(", "))
    };
    println!("\n{}", ui::ok(&format!("uninstall complete — {tail}")));
    Ok(())
}

/// Strip cona from every registered project and the home configs. Returns
/// what was cleaned, for the summary.
pub(super) fn remove_all_agents(home: &Path) -> Result<Vec<String>> {
    let mut removed = Vec::new();

    // upgrade hooks in the source repo (incl. legacy `self-update` lines)
    if let Ok(Some(src)) = db::meta_get("source_dir") {
        if strip_git_hook_lines(
            &Path::new(&src).join(".git/hooks"),
            &["self-update", "upgrade --quiet", "installed by cona"],
        ) {
            let src = short_path(Path::new(&src));
            println!("{}", ui::item(&format!("upgrade git hooks   {src}")));
            removed.push("upgrade git hooks".into());
        }
    }

    // every registered project: agent files + git hooks
    let (mut touched, mut cleaned) = (0usize, 0usize);
    for p in db::registered_project_paths() {
        let root = Path::new(&p);
        if !root.is_dir() {
            continue;
        }
        // Skip clean projects — each would print an empty heading.
        if !crate::install::agents::project_has_cona(root)
            && !git_hooks_have(&root.join(".git/hooks"), CONA_HOOK_NEEDLES)
        {
            continue;
        }
        println!(
            "\n{}",
            ui::heading(&format!("project {}", short_path(root)))
        );
        let mut any = match cmd_agents(root, "uninstall", &[], false, false) {
            Ok(b) => b,
            Err(e) => {
                println!("{}", ui::warn(&e.to_string()));
                false
            }
        };
        if strip_git_hook_lines(&root.join(".git/hooks"), CONA_HOOK_NEEDLES) {
            println!("{}", ui::item("git hooks removed"));
            any = true;
        }
        touched += 1;
        cleaned += usize::from(any);
    }
    if cleaned > 0 {
        removed.push(ui::plural(cleaned, "project"));
    }
    if touched == 0 {
        println!("\n{}", ui::dim("no per-project integration found"));
    }

    // global agent integration
    println!("\n{}", ui::heading("global"));
    if cmd_agents(home, "uninstall", &[], false, true)? {
        removed.push("home configs".into());
    }
    Ok(removed)
}

/// Remove the recorded installed binary. Returns 1 if a file was deleted.
pub(super) fn remove_binary() -> Result<usize> {
    match db::meta_get("install_path")? {
        // unlinking a running binary is fine on unix
        Some(dst) if Path::new(&dst).exists() => {
            if std::fs::remove_file(&dst).is_ok() {
                println!(
                    "{}",
                    ui::ok(&format!("removed  {}", short_path(Path::new(&dst))))
                );
                // take the sibling resolve helper too (as uninstall.sh does)
                if let Some(dir) = Path::new(&dst).parent() {
                    let helper = dir.join(HELPER_EXE);
                    if helper.exists() && std::fs::remove_file(&helper).is_ok() {
                        println!("{}", ui::ok(&format!("removed  {}", short_path(&helper))));
                    }
                }
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
