//! `cona doctor` — diagnose install + agent integration.

use crate::{db, resolve, ui};
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

/// Which cona hooks are present in a settings.json: (index hook, read-guard hook).
fn settings_cona_hooks(path: &Path) -> (bool, bool) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (false, false);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (false, false);
    };
    let mut has_index = false;
    let mut has_read = false;
    if let Some(events) = v["hooks"].as_object() {
        for entries in events.values() {
            for entry in entries.as_array().into_iter().flatten() {
                for h in entry["hooks"].as_array().into_iter().flatten() {
                    if let Some(c) = h["command"].as_str() {
                        if c.contains("cona") && c.contains("index --quiet") {
                            has_index = true;
                        }
                        if c.contains("cona") && c.contains("hook PreToolUse") {
                            has_read = true;
                        }
                    }
                }
            }
        }
    }
    (has_index, has_read)
}

/// First `cona` executable found on PATH, if any.
fn cona_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("cona"))
        .find(|p| p.is_file())
}

/// `cona doctor` — report install + agent-integration health so the user can
/// see exactly why Claude Code may or may not be picking cona up.
pub fn cmd_doctor(project_root: &Path) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    let mut issues = 0usize;

    println!("{}\n", ui::bold("cona doctor"));

    // --- binary ---
    println!("{}", ui::heading("binary"));
    match db::meta_get("install_path").ok().flatten() {
        Some(p) if Path::new(&p).exists() => println!("  {}", ui::ok(&format!("installed: {p}"))),
        Some(p) => {
            issues += 1;
            println!(
                "  {}",
                ui::warn(&format!("recorded but missing: {p}  → run `cona install`"))
            );
        }
        None => {
            issues += 1;
            println!(
                "  {}",
                ui::warn("no recorded install  → run `cona install` from the source checkout")
            );
        }
    }
    match cona_on_path() {
        Some(p) => println!("  {}", ui::ok(&format!("on PATH: {}", p.display()))),
        None => println!(
            "  {}",
            ui::warn(
                "`cona` is not on PATH (agents use an absolute path, so this is only cosmetic)"
            )
        ),
    }

    // --- agent integration (global + project) ---
    let current_ver = env!("CARGO_PKG_VERSION");
    for (label, root) in [
        ("global  (~/.claude)", home.clone()),
        ("project (./.claude)", project_root.to_path_buf()),
    ] {
        let dir = root.join(".claude");
        let settings = dir.join("settings.json");
        let skill = dir.join("skills/cona/SKILL.md");
        let (idx, read) = settings_cona_hooks(&settings);
        let mut tag = |b: bool, s: &str| {
            if b {
                ui::ok(s)
            } else {
                issues += 1;
                ui::warn(s)
            }
        };
        println!("\n{}", ui::heading(&format!("claude {label}")));
        println!("  {}", tag(idx, "index hook (PostToolUse/SessionStart)"));
        println!(
            "  {}",
            tag(read, "read-guard hook (PreToolUse read/grep/shell)")
        );
        println!(
            "  {}",
            tag(skill.exists(), &format!("skill: {}", skill.display()))
        );
        // freshness: does the config here match the running binary's baked
        // SKILL/guide/hooks? (recorded by the auto-refresh / `upgrade` paths)
        if skill.exists() {
            match db::meta_get(&super::upgrade::config_ver_key(&root))
                .ok()
                .flatten()
            {
                Some(v) if v == current_ver => {
                    println!("  {}", ui::ok(&format!("config current (v{v})")))
                }
                Some(v) => {
                    issues += 1;
                    println!(
                        "  {}",
                        ui::warn(&format!(
                            "config written by v{v}, binary is v{current_ver}  → run `cona upgrade`"
                        ))
                    );
                }
                None => {
                    issues += 1;
                    println!(
                        "  {}",
                        ui::warn("config version unknown  → run `cona upgrade` to re-sync")
                    );
                }
            }
        }
    }

    // --- MCP registration ---------------------------------------------------
    // Informational, never an "issue": the MCP server is an optional second
    // surface (the CLI + skill work without it), and most harnesses only get
    // it once their config directory exists.
    println!("\n{}", ui::heading("mcp server (cona mcp)"));
    let mcp = super::agents::mcp_registrations(project_root, &home);
    let mut any = false;
    for (a, global, path, _) in mcp.iter().filter(|(.., on)| *on) {
        any = true;
        let scope = if *global { "global" } else { "project" };
        println!(
            "  {}",
            ui::ok(&format!(
                "{} ({scope}): {}",
                a.slug(),
                super::short_path(path)
            ))
        );
    }
    if !any {
        println!(
            "  {}",
            ui::dim("not registered anywhere — `cona agents install` adds it where a harness config exists")
        );
    }

    // --- index --- (diagnostics must not CREATE a project DB as a side effect
    // — a stray empty DB would flip the hook from Nudge to Redirect here)
    let (files, symbols) = if db::project_db_path(project_root).exists() {
        let conn = db::open_project_db(project_root)?;
        (
            conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                .unwrap_or(0),
            conn.query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
                .unwrap_or(0),
        )
    } else {
        (0i64, 0i64)
    };
    println!(
        "\n{}",
        ui::heading(&format!("index ({})", project_root.display()))
    );
    if files == 0 {
        issues += 1;
        println!("  {}", ui::warn("empty — run `cona index`"));
    } else {
        println!(
            "  {}",
            ui::ok(&format!(
                "{files} files, {symbols} symbols · db {}",
                db::human_bytes(db::project_db_size(project_root))
            ))
        );
    }

    // --- storage / database ---
    let g = db::open_global_db()?;
    let s = db::storage_summary(&g, project_root)?;
    println!("\n{}", ui::heading("storage (config + databases)"));
    println!(
        "  data dir   {}  ({} total)",
        s.data_dir.display(),
        db::human_bytes(s.total)
    );
    println!(
        "  global db  {}  ({}, {} usage rows, {} projects)",
        s.global_db.display(),
        db::human_bytes(s.global_db_size),
        s.usage_rows,
        s.projects
    );
    println!(
        "  project db {}  ({})",
        s.project_db.display(),
        db::human_bytes(s.project_db_size)
    );
    println!(
        "  auto-tidy  last {} · keeps usage ≤90d / ≤200k rows (env: CONA_USAGE_RETENTION_DAYS, CONA_MAX_USAGE_ROWS)",
        s.last_tidy
    );
    if s.over_limit {
        println!(
            "  {}",
            ui::warn("~/.cona is over 100 MB — run `cona tidy --orphans` to reclaim space")
        );
    }

    // --- semantic resolve helper (optional) ---
    println!("\n{}", ui::heading("semantic resolve helper (optional)"));
    match resolve::helper_status() {
        Some((path, how)) => {
            println!(
                "  {}",
                ui::ok(&format!("found via {how}: {}", path.display()))
            );
            println!(
                "  resolves same-file & cross-file same-arity ambiguity for {}",
                resolve::SUPPORTED_LANGS
            );
        }
        None => {
            println!(
                "  {}",
                ui::warn("not installed — cona uses its name-based + arity heuristics only")
            );
            println!(
                "  ships in the release tarball beside cona; `cargo install` users get it\n  \
                 auto-fetched from the GitHub release on first use (disable: CONA_NO_FETCH_HELPER)"
            );
        }
    }

    println!(
        "\n{}",
        ui::summary(
            issues,
            "thing",
            "need attention — see above",
            "all checks passed"
        )
    );
    println!(
        "{}",
        ui::dim(
            "if Claude Code isn't using cona: it snapshots hooks + skills at startup, so \
             RESTART Claude Code (or run /hooks) after installing — verify with /hooks \
             (should list cona index + hook PreToolUse) and by asking it to use the cona skill"
        )
    );
    Ok(())
}
