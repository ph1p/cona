//! `find`: exact / LIKE / fuzzy symbol lookup.

use crate::commands::{clip, fuzzy_symbols, jout, PathFilter, LIMIT_TRAILER};
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

pub fn cmd_find(
    root: &Path,
    conn: &Connection,
    name: &str,
    kind: Option<&str>,
    limit: usize,
    path_filter: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let pf = PathFilter::new(root, path_filter);
    let kind_clause = kind.map(|_| "AND s.kind = ?4").unwrap_or("");
    let sql = format!(
        "SELECT f.path, s.kind, s.qualified, s.start_line, s.end_line, s.signature,
                CASE WHEN s.name = ?1 OR s.qualified = ?1 THEN 0 ELSE 1 END AS rank
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE (s.name = ?1 OR s.qualified = ?1 OR s.name LIKE ?2 OR s.qualified LIKE ?2)
         {kind_clause}
         ORDER BY rank, length(s.qualified), f.path LIMIT ?3"
    );
    let like = format!("%{name}%");
    // `--path` is applied in Rust below, so the SQL LIMIT must not clip
    // in-scope rows first: over-fetch when scoped, truncate afterwards. Clamped
    // both ways — a floor so a small --limit sees enough rows, a ceiling so a
    // large one can't grow the fetch without bound.
    let sql_limit = if pf.is_scoped() {
        limit.saturating_mul(20).clamp(1000, 5000)
    } else {
        // one extra row so clip() can tell "exactly limit" from "more exist"
        limit.saturating_add(1)
    } as i64;
    let mut stmt = conn.prepare(&sql)?;
    let mapper =
        |r: &rusqlite::Row| -> rusqlite::Result<(String, String, String, i64, i64, String)> {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        };
    let mut rows: Vec<(String, String, String, i64, i64, String)> = if let Some(k) = kind {
        stmt.query_map(rusqlite::params![name, like, sql_limit, k], mapper)?
            .flatten()
            .collect()
    } else {
        stmt.query_map(rusqlite::params![name, like, sql_limit], mapper)?
            .flatten()
            .collect()
    };
    if pf.is_scoped() {
        let had_rows = !rows.is_empty();
        rows.retain(|(p, ..)| pf.ok(p));
        // Distinguish "name exists, just not here" from "no such name" — the
        // fuzzy fallback would misleadingly answer the second question.
        if rows.is_empty() && had_rows {
            let scope = pf.as_str();
            return Ok((
                format!(
                    "no '{name}' under '{scope}' — it exists elsewhere; retry without --path\n"
                ),
                0,
            ));
        }
    }
    if rows.is_empty() {
        return cmd_find_fuzzy(conn, name, kind, json);
    }
    let truncated = clip(&mut rows, limit);
    // Baseline 0: a find replaces only the grep pass (≈ free in the
    // grep-then-Read model); the Read it avoids is credited to the follow-up
    // show, so counting it here too would double-count.
    let baseline = 0;
    if json {
        let items: Vec<_> = rows
            .iter()
            .map(|(p, k, q, s, e, sig)| {
                serde_json::json!({"file": p, "kind": k, "symbol": q, "start": s, "end": e, "sig": sig})
            })
            .collect();
        return jout(&items, baseline);
    }
    let mut out = String::new();
    for (path, kind, q, s, e, sig) in rows {
        out.push_str(&format!("{kind} {q}  {path}:{s}-{e}  {sig}\n"));
    }
    if truncated {
        out.push_str(LIMIT_TRAILER);
    }
    Ok((out, baseline))
}

/// Fallback when exact + LIKE found nothing: rank every symbol by fuzzy
/// score and show the closest few, so a half-remembered name still lands.
pub fn cmd_find_fuzzy(
    conn: &Connection,
    name: &str,
    kind: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let scored = fuzzy_symbols(conn, name, kind, 10)?;
    if scored.is_empty() {
        return Ok((format!("no match for '{name}'\n"), 0));
    }
    let baseline = 0; // same as cmd_find: the follow-up show earns the saving
    if json {
        let items: Vec<_> = scored
            .iter()
            .map(|h| {
                serde_json::json!({"file": h.path, "kind": h.kind, "symbol": h.qualified,
                    "start": h.start, "end": h.end, "sig": h.sig, "fuzzy": true})
            })
            .collect();
        return jout(&items, baseline);
    }
    let mut out = format!("no exact match for '{name}' — closest:\n");
    for h in scored {
        out.push_str(&format!(
            "{} {}  {}:{}-{}  {}\n",
            h.kind, h.qualified, h.path, h.start, h.end, h.sig
        ));
    }
    Ok((out, baseline))
}
