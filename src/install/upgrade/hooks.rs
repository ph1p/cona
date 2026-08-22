//! Project git hooks (index on commit/merge/checkout) + hook-line helpers.

use crate::install::agents::agent_exe;
use crate::ui;
use anyhow::{bail, Result};
use std::path::Path;

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
            let line = format!(
                "{} index --quiet 2>/dev/null &",
                crate::install::sh_quote(&agent_exe())
            );
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
pub(super) const CONA_HOOK_NEEDLES: &[&str] = &["cona index", "installed by cona"];

/// Read-only twin of `strip_git_hook_lines`: does any hook file contain one of
/// the needles? Used to decide whether a project is worth announcing.
pub(super) fn git_hooks_have(hooks_dir: &Path, needles: &[&str]) -> bool {
    ["post-commit", "post-merge", "post-checkout"]
        .iter()
        .any(|n| {
            std::fs::read_to_string(hooks_dir.join(n))
                .is_ok_and(|c| c.lines().any(|l| needles.iter().any(|nd| l.contains(nd))))
        })
}

pub(super) fn strip_git_hook_lines(hooks_dir: &Path, needles: &[&str]) -> bool {
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

/// Append a marked line to a git hook script (create if missing), idempotent.
pub(super) fn append_hook_line(hook: &Path, line: &str, marker: &str) -> Result<()> {
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
