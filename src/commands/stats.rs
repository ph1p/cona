//! Usage statistics: stats (text + JSON) and storage introspection.

use crate::{db, indexer};
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

pub fn cmd_stats(root: &Path, project_only: bool) -> Result<String> {
    let g = db::open_global_db()?;
    let mut out = String::new();
    stats_section(&mut out, &g, root, "this project", Some(root))?;
    if !project_only {
        out.push('\n');
        stats_section(&mut out, &g, root, "all projects", None)?;
    }
    out.push('\n');
    append_storage(&mut out, &g, root)?;
    Ok(out)
}

/// Where config/data lives + database sizes + retention info.
/// Machine-readable stats (`stats --json`): totals, per-command breakdown and
/// top targets for the project scope (and globally unless --project).
pub fn cmd_stats_json(root: &Path, project_only: bool) -> Result<String> {
    let g = db::open_global_db()?;
    let scope_json = |scope: Option<&str>| -> Result<serde_json::Value> {
        let t = db::totals(&g, scope)?;
        let cmds: Vec<_> = db::per_command(&g, scope)?
            .into_iter()
            .map(|(cmd, calls, avg_ms, out, saved)| {
                serde_json::json!({"cmd": cmd, "calls": calls, "avg_ms": avg_ms,
                    "tokens_out": out, "tokens_saved": saved})
            })
            .collect();
        let targets: Vec<_> = db::top_targets(&g, scope, 10)?
            .into_iter()
            .map(|(detail, calls, saved)| {
                serde_json::json!({"target": detail, "calls": calls, "tokens_saved": saved})
            })
            .collect();
        Ok(serde_json::json!({
            "calls": t.calls, "tokens_out": t.tokens_out, "tokens_saved": t.tokens_saved,
            "reads_blocked": t.reads_blocked, "total_ms": t.total_ms,
            "per_command": cmds, "top_targets": targets,
        }))
    };
    let scope_str = root.to_string_lossy().to_string();
    let mut obj = serde_json::json!({
        "project": scope_json(Some(&scope_str))?,
        "storage": {
            "total_bytes": db::total_storage_bytes(),
            "global_db_bytes": db::global_db_size(),
            "project_db_bytes": db::project_db_size(root),
        },
    });
    if !project_only {
        obj["global"] = scope_json(None)?;
    }
    Ok(format!("{}\n", serde_json::to_string(&obj)?))
}

fn append_storage(out: &mut String, g: &Connection, root: &Path) -> Result<()> {
    let s = db::storage_summary(g, root)?;
    out.push_str("── storage ──\n");
    out.push_str(&format!(
        "  data dir   {}  ({} total)\n",
        s.data_dir.display(),
        db::human_bytes(s.total)
    ));
    out.push_str(&format!(
        "  global db  {}  ({}, {} usage rows, {} projects)\n",
        s.global_db.display(),
        db::human_bytes(s.global_db_size),
        s.usage_rows,
        s.projects
    ));
    out.push_str(&format!(
        "  project db {}  ({})\n",
        s.project_db.display(),
        db::human_bytes(s.project_db_size)
    ));
    out.push_str(&format!(
        "  auto-tidy  last {} · keeps ≤90d / ≤200k usage rows\n",
        s.last_tidy
    ));
    if s.over_limit {
        out.push_str("  note: >100 MB — run `cona tidy --orphans` to reclaim space\n");
    }
    Ok(())
}

fn stats_section(
    out: &mut String,
    g: &Connection,
    root: &Path,
    label: &str,
    scope: Option<&Path>,
) -> Result<()> {
    let scope_str = scope.map(|p| p.to_string_lossy().to_string());
    let scope_ref = scope_str.as_deref();
    let t = db::totals(g, scope_ref)?;

    out.push_str(&format!("── stats · {label} ──\n"));

    // index state
    match scope {
        Some(_) => {
            let conn = db::open_project_db(root)?;
            let files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
            let symbols: i64 = conn.query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?;
            let paths: Vec<String> = {
                let mut s = conn.prepare("SELECT path FROM files")?;
                let v: Vec<String> = s.query_map([], |r| r.get(0))?.flatten().collect();
                v
            };
            let stale = paths
                .iter()
                .filter(|p| indexer::is_stale(root, &conn, p))
                .count();
            let last = g
                .query_row(
                    "SELECT last_indexed FROM projects WHERE hash = ?1",
                    [db::project_hash(root)],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .ok()
                .flatten()
                .map(db::ago)
                .unwrap_or_else(|| "never".into());
            out.push_str(&format!(
                "  index   {files} files · {symbols} symbols · db {} · {} · indexed {last}\n",
                db::human_bytes(db::project_db_size(root)),
                if stale == 0 {
                    "fresh".to_string()
                } else {
                    format!("{stale} stale")
                },
            ));
        }
        None => {
            let (n, f, s): (i64, i64, i64) = g.query_row(
                "SELECT COUNT(*), COALESCE(SUM(files),0), COALESCE(SUM(symbols),0) FROM projects",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            out.push_str(&format!(
                "  index   {n} projects · {f} files · {s} symbols\n"
            ));
        }
    }

    // savings headline
    out.push_str(&format!(
        "  savings {} tokens saved · {} used · {} would-read · {:.0}% avoided · {} big reads intercepted\n",
        t.tokens_saved,
        t.tokens_out,
        t.baseline(),
        t.pct_saved(),
        t.reads_blocked,
    ));

    // per-command table — queries only; maintenance rows (index/edit/hook:*)
    // never carry savings and are folded into one line below.
    let rows = db::per_command(g, scope_ref)?;
    if rows.is_empty() {
        out.push_str("  (no queries recorded yet)\n");
        return Ok(());
    }
    let (queries, maint): (Vec<_>, Vec<_>) = rows
        .iter()
        .partition(|(cmd, ..)| !db::is_maintenance_cmd(cmd));
    if queries.is_empty() {
        out.push_str("  (no queries recorded yet)\n");
    } else {
        out.push_str(&format!(
            "  {:<16} {:>6} {:>8} {:>11} {:>12}\n",
            "cmd", "calls", "avg ms", "tokens out", "tokens saved"
        ));
        for (cmd, n, ms, tout, tsav) in &queries {
            out.push_str(&format!(
                "  {:<16} {:>6} {:>8.0} {:>11} {:>12}\n",
                cmd, n, ms, tout, tsav
            ));
        }
    }
    if !maint.is_empty() {
        let parts: Vec<String> = maint
            .iter()
            .map(|(cmd, n, ms, ..)| format!("{cmd} {n}× (avg {ms:.0}ms)"))
            .collect();
        out.push_str(&format!("  maintenance {}\n", parts.join(" · ")));
    }

    // top targets
    let top = db::top_targets(g, scope_ref, 8)?;
    if !top.is_empty() {
        out.push_str("  top targets:\n");
        for (d, n, saved) in top {
            out.push_str(&format!("    {n:>3}×  {d}  ({saved} saved)\n"));
        }
    }

    // recent activity — real queries only (maintenance has no savings)
    let recent = db::recent(g, scope_ref, 10, true)?;
    if !recent.is_empty() {
        out.push_str("  recent:\n");
        for (ts, cmd, detail, saved, ms) in recent {
            out.push_str(&format!(
                "    {:<10} {:<16} {:<24} +{saved} tok  {ms}ms\n",
                db::ago(ts),
                cmd,
                detail,
            ));
        }
    }
    Ok(())
}
