//! Git-history commands: blame, hot, coupling.

use super::{jout, locate_fresh};
use crate::{db, gitmap};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

pub fn cmd_blame(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    limit: usize,
    json: bool,
) -> Result<(String, i64)> {
    let (path, s, e, q) = locate_fresh(root, conn, symbol, None)?;
    let (commits, raw_len) = gitmap::log_symbol_range(root, &path, s, e, limit)?;
    let baseline = db::est_tokens(raw_len);
    if json {
        let items: Vec<_> = commits
            .iter()
            .map(|(h, a, ts, sub)| serde_json::json!({"hash": h, "author": a, "ts": ts, "subject": sub}))
            .collect();
        let obj =
            serde_json::json!({"symbol": q, "file": path, "start": s, "end": e, "commits": items});
        return jout(&obj, baseline);
    }
    let mut out = format!("history of {q}  {path}:{s}-{e}\n");
    if commits.is_empty() {
        out.push_str("  no commits touch these lines (new/uncommitted code?)\n");
    }
    // git already capped the list via -n
    for (h, a, ts, sub) in &commits {
        let short: String = h.chars().take(7).collect();
        out.push_str(&format!("  {short}  {:<4} {a}: {sub}\n", db::ago(*ts)));
    }
    Ok((out, baseline))
}

pub fn cmd_hot(
    root: &Path,
    conn: &Connection,
    since: &str,
    limit: usize,
    json: bool,
) -> Result<(String, i64)> {
    let mut stmt = conn.prepare("SELECT path FROM files")?;
    let indexed: HashSet<String> = stmt.query_map([], |r| r.get(0))?.flatten().collect();
    let commits = gitmap::log_numstat(root, since)?;
    let churn: Vec<_> = gitmap::churn(&commits)
        .into_iter()
        .filter(|(p, ..)| indexed.contains(p))
        .take(limit)
        .collect();
    if json {
        let items: Vec<_> = churn
            .iter()
            .map(|(p, n, l, a, ts)| serde_json::json!({"file": p, "commits": n, "lines": l, "last_author": a, "last_ts": ts}))
            .collect();
        return jout(&items, 0);
    }
    let mut out = format!(
        "churn hotspots since '{since}' ({} commits):\n",
        commits.len()
    );
    if churn.is_empty() {
        out.push_str("  none — no commits in window touch indexed files\n");
    }
    for (p, n, l, a, ts) in &churn {
        out.push_str(&format!(
            "  {n:>3}×  ~{l:<5} lines  {p}  (last: {a}, {})\n",
            db::ago(*ts)
        ));
    }
    Ok((out, 0))
}

pub fn cmd_coupling(
    root: &Path,
    conn: &Connection,
    file: &str,
    since: &str,
    limit: usize,
    json: bool,
) -> Result<(String, i64)> {
    let target = file.trim_start_matches("./").to_string();
    let mut stmt = conn.prepare("SELECT count(*) FROM files WHERE path = ?1")?;
    let known: i64 = stmt.query_row([&target], |r| r.get(0))?;
    if known == 0 {
        eprintln!("note: '{target}' is not in the index — matching raw git paths anyway");
    }
    // full history (not `-- <target>`): co_change needs each commit's complete file list
    let commits = gitmap::log_numstat(root, since)?;
    let (total, riders) = gitmap::co_change(&commits, &target);
    let top: Vec<_> = riders
        .into_iter()
        .filter(|(_, n)| *n >= 2)
        .take(limit)
        .collect();
    if json {
        let items: Vec<_> = top
            .iter()
            .map(|(p, n)| serde_json::json!({"file": p, "together": n, "of": total}))
            .collect();
        let obj = serde_json::json!({"file": target, "commits": total, "coupled": items});
        return jout(&obj, 0);
    }
    let mut out = format!("files co-changing with {target} ({total} commits since '{since}'):\n");
    if total == 0 {
        out.push_str("  no commits touch this file in the window\n");
    } else if top.is_empty() {
        out.push_str("  none ≥2 co-changes — this file moves alone\n");
    }
    for (p, n) in &top {
        let pct = if total > 0 { n * 100 / total } else { 0 };
        out.push_str(&format!("  {n:>3}/{total} ({pct:>2}%)  {p}\n"));
    }
    Ok((out, 0))
}
