//! `find`: exact / LIKE / fuzzy symbol lookup.

use crate::commands::{clip, jout, PathFilter, LIMIT_TRAILER};
use crate::{db, fuzzy};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashSet;
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
        "SELECT f.path, s.kind, s.qualified, s.start_line, s.end_line, s.signature, f.size,
                CASE WHEN s.name = ?1 OR s.qualified = ?1 THEN 0 ELSE 1 END AS rank
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE (s.name = ?1 OR s.qualified = ?1 OR s.name LIKE ?2 OR s.qualified LIKE ?2)
         {kind_clause}
         ORDER BY rank, length(s.qualified), f.path LIMIT ?3"
    );
    let like = format!("%{name}%");
    // `--path` is applied in Rust (below), so the SQL LIMIT must not clip
    // in-scope rows before that filter runs — over-fetch when scoped, then
    // truncate to `limit` afterwards.
    // Clamped both ways: a floor so a small --limit still sees enough rows to
    // filter, a ceiling so a large one can't scale the fetch without bound.
    let sql_limit = if pf.is_scoped() {
        limit.saturating_mul(20).clamp(1000, 5000)
    } else {
        // one extra row so clip() can tell "exactly limit" from "more exist"
        limit.saturating_add(1)
    } as i64;
    let mut stmt = conn.prepare(&sql)?;
    let mapper =
        |r: &rusqlite::Row| -> rusqlite::Result<(String, String, String, i64, i64, String, i64)> {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        };
    let mut rows: Vec<(String, String, String, i64, i64, String, i64)> = if let Some(k) = kind {
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
    // Baseline: sum each hit file's size once (rows are rank-ordered, not
    // path-ordered, so dedup with a set).
    let mut seen: HashSet<&str> = HashSet::new();
    let mut bytes: i64 = 0;
    for (p, .., size) in &rows {
        if seen.insert(p) {
            bytes += size;
        }
    }
    let baseline = db::est_tokens(bytes as usize);
    if json {
        let items: Vec<_> = rows
            .iter()
            .map(|(p, k, q, s, e, sig, _)| {
                serde_json::json!({"file": p, "kind": k, "symbol": q, "start": s, "end": e, "sig": sig})
            })
            .collect();
        return jout(&items, baseline);
    }
    let mut out = String::new();
    for (path, kind, q, s, e, sig, _) in rows {
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
    struct Hit {
        path: String,
        kind: String,
        qualified: String,
        start: i64,
        end: i64,
        sig: String,
        size: i64,
    }
    let mut stmt = conn.prepare(
        "SELECT f.path, s.kind, s.qualified, s.start_line, s.end_line, s.signature, f.size, s.name
         FROM symbols s JOIN files f ON f.id = s.file_id",
    )?;
    let candidates: Vec<(Hit, String)> = stmt
        .query_map([], |r| {
            let hit = Hit {
                path: r.get(0)?,
                kind: r.get(1)?,
                qualified: r.get(2)?,
                start: r.get(3)?,
                end: r.get(4)?,
                sig: r.get(5)?,
                size: r.get(6)?,
            };
            let bare: String = r.get(7)?;
            Ok((hit, bare))
        })?
        .flatten()
        .filter(|(hit, _)| kind.map(|k| hit.kind == k).unwrap_or(true))
        .collect();
    let scored: Vec<(i64, &Hit)> = fuzzy::rank(
        name,
        candidates
            .iter()
            .enumerate()
            .map(|(i, (h, bare))| (i, bare.as_str(), h.qualified.as_str())),
        10,
    )
    .into_iter()
    .map(|(s, i)| (s, &candidates[i].0))
    .collect();
    if scored.is_empty() {
        return Ok((format!("no match for '{name}'\n"), 0));
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut bytes: i64 = 0;
    for (_, hit) in &scored {
        if seen.insert(&hit.path) {
            bytes += hit.size;
        }
    }
    let baseline = db::est_tokens(bytes as usize);
    if json {
        let items: Vec<_> = scored
            .iter()
            .map(|(_, h)| {
                serde_json::json!({"file": h.path, "kind": h.kind, "symbol": h.qualified,
                    "start": h.start, "end": h.end, "sig": h.sig, "fuzzy": true})
            })
            .collect();
        return jout(&items, baseline);
    }
    let mut out = format!("no exact match for '{name}' — closest:\n");
    for (_, h) in scored {
        out.push_str(&format!(
            "{} {}  {}:{}-{}  {}\n",
            h.kind, h.qualified, h.path, h.start, h.end, h.sig
        ));
    }
    Ok((out, baseline))
}
