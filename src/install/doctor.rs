//! `cona doctor` — diagnose install + agent integration.
//!
//! Split gather/render: `gather()` collects every fact into a `DoctorReport`,
//! then one renderer prints prose and the other JSON (`cona doctor --json`).
//! New checks belong in `gather()` so both surfaces see them.

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

/// One `.claude` scope (global or project) integration snapshot.
struct ScopeCheck {
    label: &'static str,
    index_hook: bool,
    read_hook: bool,
    skill_path: PathBuf,
    skill: bool,
    /// Recorded config version, only meaningful when the skill exists.
    config_ver: Option<String>,
}

/// Everything `doctor` knows, gathered once, rendered twice (text / JSON).
struct DoctorReport {
    current_ver: &'static str,
    install_path: Option<String>,
    install_exists: bool,
    on_path: Option<PathBuf>,
    scopes: Vec<ScopeCheck>,
    /// (agent slug, scope, config path) for every registration found.
    mcp: Vec<(String, &'static str, PathBuf)>,
    index_files: i64,
    index_symbols: i64,
    index_db_size: i64,
    storage: db::StorageSummary,
    helper: Option<(PathBuf, &'static str)>,
    project_root: PathBuf,
    issues: usize,
}

fn gather(project_root: &Path) -> Result<DoctorReport> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    let current_ver = env!("CARGO_PKG_VERSION");
    let mut issues = 0usize;

    let install_path = db::meta_get("install_path").ok().flatten();
    let install_exists = install_path.as_deref().is_some_and(|p| Path::new(p).exists());
    if !install_exists {
        issues += 1;
    }
    let on_path = cona_on_path();

    let mut scopes = Vec::new();
    for (label, root) in [("global", home.clone()), ("project", project_root.to_path_buf())] {
        let dir = root.join(".claude");
        let skill_path = dir.join("skills/cona/SKILL.md");
        let (index_hook, read_hook) = settings_cona_hooks(&dir.join("settings.json"));
        let skill = skill_path.exists();
        issues += [index_hook, read_hook, skill].iter().filter(|b| !**b).count();
        let config_ver = if skill {
            let v = db::meta_get(&super::upgrade::config_ver_key(&root))
                .ok()
                .flatten();
            if v.as_deref() != Some(current_ver) {
                issues += 1;
            }
            v
        } else {
            None
        };
        scopes.push(ScopeCheck {
            label,
            index_hook,
            read_hook,
            skill_path,
            skill,
            config_ver,
        });
    }

    let mcp = super::agents::mcp_registrations(project_root, &home)
        .into_iter()
        .filter(|(.., on)| *on)
        .map(|(a, global, path, _)| {
            (
                a.slug().to_string(),
                if global { "global" } else { "project" },
                path,
            )
        })
        .collect();

    // diagnostics must not CREATE a project DB as a side effect — a stray
    // empty DB would flip the hook from Nudge to Redirect here
    let (index_files, index_symbols) = if db::project_db_path(project_root).exists() {
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
    if index_files == 0 {
        issues += 1;
    }

    let g = db::open_global_db()?;
    let storage = db::storage_summary(&g, project_root)?;
    let helper = resolve::helper_status();

    Ok(DoctorReport {
        current_ver,
        install_path,
        install_exists,
        on_path,
        scopes,
        mcp,
        index_files,
        index_symbols,
        index_db_size: db::project_db_size(project_root),
        storage,
        helper,
        project_root: project_root.to_path_buf(),
        issues,
    })
}

fn render_json(r: &DoctorReport) -> serde_json::Value {
    serde_json::json!({
        "version": r.current_ver,
        "issues": r.issues,
        "binary": {
            "installed": r.install_path,
            "exists": r.install_exists,
            "on_path": r.on_path.as_ref().map(|p| p.display().to_string()),
        },
        "claude": r.scopes.iter().map(|s| serde_json::json!({
            "scope": s.label,
            "index_hook": s.index_hook,
            "read_hook": s.read_hook,
            "skill": s.skill,
            "skill_path": s.skill_path.display().to_string(),
            "config_version": s.config_ver,
            "config_current": s.skill.then_some(s.config_ver.as_deref() == Some(r.current_ver)),
        })).collect::<Vec<_>>(),
        "mcp": r.mcp.iter().map(|(agent, scope, path)| serde_json::json!({
            "agent": agent, "scope": scope, "path": path.display().to_string(),
        })).collect::<Vec<_>>(),
        "index": {
            "root": r.project_root.display().to_string(),
            "files": r.index_files,
            "symbols": r.index_symbols,
            "db_bytes": r.index_db_size,
        },
        "storage": {
            "data_dir": r.storage.data_dir.display().to_string(),
            "total_bytes": r.storage.total,
            "global_db_bytes": r.storage.global_db_size,
            "usage_rows": r.storage.usage_rows,
            "projects": r.storage.projects,
            "project_db_bytes": r.storage.project_db_size,
            "last_tidy": r.storage.last_tidy,
            "over_limit": r.storage.over_limit,
        },
        "resolve_helper": {
            "found": r.helper.is_some(),
            "path": r.helper.as_ref().map(|(p, _)| p.display().to_string()),
            "via": r.helper.as_ref().map(|(_, how)| *how),
        },
    })
}

fn render_text(r: &DoctorReport) {
    println!("{}\n", ui::bold("cona doctor"));

    println!("{}", ui::heading("binary"));
    match (&r.install_path, r.install_exists) {
        (Some(p), true) => println!("  {}", ui::ok(&format!("installed: {p}"))),
        (Some(p), false) => println!(
            "  {}",
            ui::warn(&format!("recorded but missing: {p}  → run `cona install`"))
        ),
        (None, _) => println!(
            "  {}",
            ui::warn("no recorded install  → run `cona install` from the source checkout")
        ),
    }
    match &r.on_path {
        Some(p) => println!("  {}", ui::ok(&format!("on PATH: {}", p.display()))),
        None => println!(
            "  {}",
            ui::warn(
                "`cona` is not on PATH (agents use an absolute path, so this is only cosmetic)"
            )
        ),
    }

    for s in &r.scopes {
        let tag = |b: bool, s: &str| if b { ui::ok(s) } else { ui::warn(s) };
        let label = match s.label {
            "global" => "global  (~/.claude)",
            _ => "project (./.claude)",
        };
        println!("\n{}", ui::heading(&format!("claude {label}")));
        println!("  {}", tag(s.index_hook, "index hook (PostToolUse/SessionStart)"));
        println!(
            "  {}",
            tag(s.read_hook, "read-guard hook (PreToolUse read/grep/shell)")
        );
        println!(
            "  {}",
            tag(s.skill, &format!("skill: {}", s.skill_path.display()))
        );
        if s.skill {
            match &s.config_ver {
                Some(v) if v == r.current_ver => {
                    println!("  {}", ui::ok(&format!("config current (v{v})")))
                }
                Some(v) => println!(
                    "  {}",
                    ui::warn(&format!(
                        "config written by v{v}, binary is v{}  → run `cona upgrade`",
                        r.current_ver
                    ))
                ),
                None => println!(
                    "  {}",
                    ui::warn("config version unknown  → run `cona upgrade` to re-sync")
                ),
            }
        }
    }

    // MCP is informational, never an "issue": an optional second surface (the
    // CLI + skill work without it), and most harnesses only get it once their
    // config directory exists.
    println!("\n{}", ui::heading("mcp server (cona mcp)"));
    if r.mcp.is_empty() {
        println!(
            "  {}",
            ui::dim("not registered anywhere — `cona agents install` adds it where a harness config exists")
        );
    }
    for (agent, scope, path) in &r.mcp {
        println!(
            "  {}",
            ui::ok(&format!("{agent} ({scope}): {}", super::short_path(path)))
        );
    }

    println!(
        "\n{}",
        ui::heading(&format!("index ({})", r.project_root.display()))
    );
    if r.index_files == 0 {
        println!("  {}", ui::warn("empty — run `cona index`"));
    } else {
        println!(
            "  {}",
            ui::ok(&format!(
                "{} files, {} symbols · db {}",
                r.index_files,
                r.index_symbols,
                db::human_bytes(r.index_db_size)
            ))
        );
    }

    let s = &r.storage;
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

    println!("\n{}", ui::heading("semantic resolve helper (optional)"));
    match &r.helper {
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
            r.issues,
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
}

/// `cona doctor` — report install + agent-integration health so the user can
/// see exactly why Claude Code may or may not be picking cona up.
pub fn cmd_doctor(project_root: &Path, json: bool) -> Result<()> {
    let report = gather(project_root)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&render_json(&report))?);
    } else {
        render_text(&report);
    }
    Ok(())
}
