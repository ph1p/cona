//! `tree` and `tree --rank`: project structure and fan-in ranking.

use crate::commands::{jout, BudgetOut, PathFilter};
use crate::{db, lang};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub fn cmd_tree(
    root: &Path,
    conn: &Connection,
    budget: i64,
    path_filter: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let pf = PathFilter::new(root, path_filter);
    let mut stmt = conn.prepare(
        "SELECT f.path, s.kind, s.qualified, s.start_line, s.end_line, f.size
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.parent IS NULL
         ORDER BY f.path, s.start_line",
    )?;
    let rows: Vec<(String, String, String, i64, i64, i64)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .flatten()
        .collect();

    // Baseline: sum each included file's size once (rows are path-ordered).
    let mut bytes: i64 = 0;
    if json {
        let mut current = "";
        let mut items = Vec::new();
        for (p, k, q, s, e, size) in &rows {
            if !pf.ok(p) {
                continue;
            }
            if p.as_str() != current {
                current = p;
                bytes += size;
            }
            items
                .push(serde_json::json!({"file": p, "kind": k, "symbol": q, "start": s, "end": e}));
        }
        let out = format!("{}\n", serde_json::to_string(&items)?);
        return Ok((out, db::est_tokens(bytes as usize)));
    }

    let mut bo = BudgetOut::new(String::new(), budget);
    let mut current = String::new();
    for (path, kind, name, s, e, size) in rows {
        if !pf.ok(&path) {
            continue;
        }
        let mut chunk = String::new();
        let new_file = path != current;
        if new_file {
            chunk.push_str(&format!("{path}\n"));
            current = path.clone();
        }
        chunk.push_str(&format!("  {kind} {name} :{s}-{e}\n"));
        if !bo.try_push(&chunk) {
            break;
        }
        if new_file {
            bytes += size;
        }
    }
    if bo.out.is_empty() {
        bo.push_always("no symbols indexed — run `cona index`\n");
    }
    let out = bo.finish("… truncated (raise --budget or filter with --path)\n");
    Ok((out, db::est_tokens(bytes as usize)))
}

/// Rank top-level symbols by reference fan-in: identifier occurrences of the
/// symbol's name in files OTHER than its defining one (semantic per file when
/// parseable, textual fallback — same rules as `refs`). The "what is
/// load-bearing here" view, à la Aider's repo map.
pub fn cmd_tree_rank(
    root: &Path,
    conn: &Connection,
    budget: i64,
    path_filter: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let pf = PathFilter::new(root, path_filter);
    struct RankSym {
        path: String,
        kind: String,
        qualified: String,
        name: String,
        start: i64,
        end: i64,
    }
    let mut stmt = conn.prepare(
        "SELECT f.path, s.kind, s.qualified, s.name, s.start_line, s.end_line
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.parent IS NULL ORDER BY f.path, s.start_line",
    )?;
    let syms: Vec<RankSym> = stmt
        .query_map([], |r| {
            Ok(RankSym {
                path: r.get(0)?,
                kind: r.get(1)?,
                qualified: r.get(2)?,
                name: r.get(3)?,
                start: r.get(4)?,
                end: r.get(5)?,
            })
        })?
        .flatten()
        .collect();
    let names: HashSet<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    let defining: HashSet<&str> = syms.iter().map(|s| s.path.as_str()).collect();

    // one pass over the repo: global occurrence totals per name, plus each
    // defining file's own counts so fan-in = total − self (O(symbols) ranking)
    let mut files_stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let all_files: Vec<String> = files_stmt.query_map([], |r| r.get(0))?.flatten().collect();
    let mut total: HashMap<String, i64> = HashMap::new();
    let mut self_counts: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let mut bytes: usize = 0;
    for rel in &all_files {
        let Ok(src) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        bytes += src.len();
        let counts = lang::ident_counts(lang::detect_lang(rel), &src, &names);
        for (n, c) in &counts {
            *total.entry(n.clone()).or_insert(0) += c;
        }
        if defining.contains(rel.as_str()) && !counts.is_empty() {
            self_counts.insert(rel.clone(), counts);
        }
    }

    let mut ranked: Vec<(i64, &RankSym)> = syms
        .iter()
        .filter(|sym| pf.ok(&sym.path))
        // one-line `mod x;` declarations soak up their module's whole fan-in
        // but point at no code — orientation noise, drop them from the ranking
        .filter(|sym| !(sym.kind == "mod" && sym.start == sym.end))
        .map(|sym| {
            let own = self_counts
                .get(&sym.path)
                .and_then(|c| c.get(&sym.name))
                .copied()
                .unwrap_or(0);
            (total.get(&sym.name).copied().unwrap_or(0) - own, sym)
        })
        .collect();
    // tie-break by name before path so same-named symbols (equal fan-in by
    // construction) form one contiguous run for the collapse below
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.name.cmp(&b.1.name))
            .then_with(|| a.1.path.cmp(&b.1.path))
    });

    // Baseline: ranking required reading every indexed file once.
    let baseline = db::est_tokens(bytes);
    if json {
        let items: Vec<_> = ranked
            .iter()
            .map(|(fan, sym)| {
                serde_json::json!({"fan_in": fan, "kind": sym.kind, "symbol": sym.qualified,
                    "file": sym.path, "start": sym.start, "end": sym.end})
            })
            .collect();
        return jout(&items, baseline);
    }
    let mut bo = BudgetOut::new(String::new(), budget);
    let mut i = 0;
    while i < ranked.len() {
        let (fan, sym) = &ranked[i];
        // fan-in is per NAME, so same-named symbols (16× `mod tests`) form
        // identical-count runs with zero orientation value — collapse to one row
        let run = ranked[i..]
            .iter()
            .take_while(|(f, s2)| f == fan && s2.name == sym.name && s2.kind == sym.kind)
            .count();
        let line = if run >= 3 {
            format!("{fan:>4}×  {} {}  (×{run} files)\n", sym.kind, sym.name)
        } else {
            let RankSym {
                path: p,
                kind: k,
                qualified: q,
                start: s,
                end: e,
                ..
            } = sym;
            format!("{fan:>4}×  {k} {q}  {p}:{s}-{e}\n")
        };
        if !bo.try_push(&line) {
            break;
        }
        i += if run >= 3 { run } else { 1 };
    }
    if bo.out.is_empty() {
        bo.push_always("no symbols indexed — run `cona index`\n");
    }
    let out = bo.finish("… truncated (raise --budget or filter with --path)\n");
    Ok((out, baseline))
}
