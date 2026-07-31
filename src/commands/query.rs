//! Read-only navigation commands: tree, outline, find, show, refs,
//! context, diff, grep.

use super::{
    jout, locate_fresh, push_numbered_lines, render_symbol_body, scan_ref_sites, BudgetOut,
    PathFilter, ENCLOSING_SYMBOL_SQL,
};
use crate::{db, diffmap, entries, fuzzy, gitmap, graph, indexer, lang, resolve};
use anyhow::{bail, Result};
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

pub fn cmd_outline(
    root: &Path,
    conn: &Connection,
    file: &str,
    show_sig: bool,
    json: bool,
) -> Result<(String, i64)> {
    // suffix match only at a path-separator boundary (`db.rs` must not pull in
    // `gitdb.rs`), with LIKE metacharacters escaped so `_`/`%` in names stay literal
    let escaped = file
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let mut stmt = conn.prepare(
        "SELECT f.path, s.kind, s.qualified, s.start_line, s.end_line, s.signature, f.size
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE f.path = ?1 OR f.path LIKE ?2 ESCAPE '\\'
         ORDER BY f.path, s.start_line",
    )?;
    let rows: Vec<(String, String, String, i64, i64, String, i64)> = stmt
        .query_map(rusqlite::params![file, format!("%/{escaped}")], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .flatten()
        .collect();
    if rows.is_empty() {
        // A directory is a natural thing to hand `outline`; answer it with the
        // command that does cover directories instead of erroring out.
        // On disk normally; the index probe still answers for a directory that
        // was indexed but has since been removed from the working tree.
        let is_dir = root.join(file).is_dir()
            || conn
                .query_row(
                    "SELECT 1 FROM files WHERE path LIKE ?1 ESCAPE '\\' LIMIT 1",
                    [format!("{escaped}/%")],
                    |_| Ok(()),
                )
                .is_ok();
        if is_dir {
            bail!("'{file}' is a directory — try `cona tree --path {file}`");
        }
        bail!("no symbols for '{file}' — file not indexed or has none");
    }
    // Baseline: sum each matched file's size once (rows are path-ordered).
    let mut bytes: i64 = 0;
    let mut current = "";
    for (p, .., size) in &rows {
        if p.as_str() != current {
            current = p;
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
    let mut current = String::new();
    for (path, kind, name, s, e, sig, _) in rows {
        if path != current {
            out.push_str(&format!("{path}\n"));
            current = path;
        }
        let depth = name.matches('.').count();
        let indent = "  ".repeat(depth + 1);
        if show_sig {
            out.push_str(&format!("{indent}{kind} {name} :{s}-{e}  {sig}\n"));
        } else {
            out.push_str(&format!("{indent}{kind} {name} :{s}-{e}\n"));
        }
    }
    Ok((out, baseline))
}

pub fn cmd_find(
    root: &Path,
    conn: &Connection,
    name: &str,
    kind: Option<&str>,
    limit: i64,
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
        limit
    };
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
        rows.truncate(limit.max(0) as usize);
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

/// `show` for one symbol. When `all` is set and the name is ambiguous, every
/// candidate is rendered in turn instead of erroring — the ambiguity is
/// answered rather than bounced back for another round-trip.
pub fn cmd_show(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    context: usize,
    kind: Option<&str>,
    sig: bool,
    json: bool,
    all: bool,
) -> Result<(String, i64)> {
    // A path handed to `show` means "map this file" — answer with the outline
    // instead of failing on a symbol name that was never a symbol. Directories
    // route here too, so they reach cmd_outline's `tree --path` redirect rather
    // than dead-ending in the symbol resolver. The filesystem is the authority;
    // `split_locator` only keeps a `file.rs:Name` locator (which addresses a
    // symbol) from being mistaken for a path.
    if super::split_locator(symbol).is_none() && root.join(symbol).exists() {
        return cmd_outline(root, conn, symbol, sig, json);
    }
    if all {
        let cands = super::locate_all(conn, symbol, kind)?;
        if cands.len() > 1 {
            let mut out = String::new();
            let mut baseline = 0;
            for (p, _, _, q) in &cands {
                // address each candidate unambiguously by file:Name
                let addr = format!("{p}:{}", db::name_tail(q));
                match show_one(root, conn, &addr, context, kind, sig, false, false) {
                    Ok((body, b)) => {
                        out.push_str(&body);
                        out.push('\n');
                        baseline += b;
                    }
                    Err(e) => out.push_str(&format!("{addr}: {e}\n")),
                }
            }
            if json {
                return jout(&serde_json::json!({"symbol": symbol, "matches": cands.len(), "text": out}), baseline);
            }
            return Ok((out, baseline));
        }
    }
    show_one(root, conn, symbol, context, kind, sig, json, true)
}

fn show_one(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    context: usize,
    kind: Option<&str>,
    sig: bool,
    json: bool,
    disclose_others: bool,
) -> Result<(String, i64)> {
    let (path, s, e, q) = locate_fresh(root, conn, symbol, kind)?;
    // --sig: the signature is already in the index — print it without ever
    // reading the file body. The leanest possible peek (one line, not the span).
    if sig {
        let signature: String = conn
            .query_row(
                "SELECT s.signature FROM symbols s JOIN files f ON f.id = s.file_id
                 WHERE f.path = ?1 AND s.qualified = ?2 AND s.start_line = ?3",
                rusqlite::params![path, q, s],
                |r| r.get(0),
            )
            .unwrap_or_default();
        // Baseline: without --sig the agent would `show` the whole span. Honest
        // per-line width from the index (file size / its last symbol's end line)
        // instead of a fabricated 80-char constant — no file read either way.
        let span_lines = ((e - s + 1).max(1)) as usize;
        let avg_len: usize = conn
            .query_row(
                "SELECT f.size, MAX(s.end_line) FROM files f
                 JOIN symbols s ON s.file_id = f.id WHERE f.path = ?1",
                [&path],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .map(|(size, max_line)| (size / max_line.max(1)) as usize)
            .unwrap_or(40);
        let baseline = db::baseline_tokens(&vec![avg_len; span_lines], &[1, span_lines]);
        if json {
            let obj = serde_json::json!({"file": path, "symbol": q, "start": s, "end": e, "sig": signature});
            return jout(&obj, baseline);
        }
        let sig_txt = if signature.is_empty() {
            q.clone()
        } else {
            signature
        };
        return Ok((format!("{path}:{s}-{e}  {sig_txt}\n"), baseline));
    }
    let abs = root.join(&path);
    let src = std::fs::read_to_string(&abs)?;
    let lines: Vec<&str> = src.lines().collect();
    // Honest baseline: a targeted Read window around the symbol, not the whole
    // file — anchor windows on the symbol's first and last line.
    let line_lens: Vec<usize> = lines.iter().map(|l| l.len()).collect();
    let baseline = db::baseline_tokens(&line_lens, &[s as usize, e as usize]);
    let end = ((e as usize) + context).min(lines.len());
    // clamp start to end: a stale index can point past EOF (panic guard)
    let start = (s as usize)
        .saturating_sub(1)
        .saturating_sub(context)
        .min(end);
    let notes = db::notes_for(conn, &q).unwrap_or_default();
    if json {
        let code = lines[start..end].join("\n");
        let obj = serde_json::json!({
            "file": path, "symbol": q, "start": start + 1, "end": end, "code": code,
            "notes": notes.iter().map(|(_, n, _)| n.clone()).collect::<Vec<_>>(),
        });
        return jout(&obj, baseline);
    }
    let mut out = format!("{path}:{s}-{e}  ({q})\n");
    for (_, n, _) in &notes {
        out.push_str(&format!("  ⚑ {n}\n"));
    }
    push_numbered_lines(&mut out, &lines, start, end);
    // exact-name preference may have hidden same-named candidates — disclose
    // them in one trailer line so the agent never gets a confidently wrong body.
    // Skipped under `--all`, which is already printing every candidate.
    if !disclose_others {
        return Ok((out, baseline));
    }
    let others: Vec<String> = conn
        .prepare(
            "SELECT s.qualified, f.path, s.start_line
             FROM symbols s JOIN files f ON f.id = s.file_id
             WHERE s.name = ?1 AND NOT (f.path = ?2 AND s.start_line = ?3)
             LIMIT 3",
        )?
        .query_map(rusqlite::params![db::name_tail(&q), path, s], |r| {
            Ok(format!(
                "{} {}:{}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?
            ))
        })?
        .flatten()
        .collect();
    if !others.is_empty() {
        out.push_str(&format!("also matched: {}\n", others.join(", ")));
    }
    Ok((out, baseline))
}

pub fn cmd_refs(
    root: &Path,
    conn: &Connection,
    name: &str,
    limit: usize,
    path_filter: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let mut hits: Vec<(String, usize, String)> = Vec::new();
    // Honest baseline: per hit file, a grep pass + a Read window around each
    // ref line (not the whole file) — what the same lookup costs without cona.
    let mut baseline: i64 = 0;
    let mut truncated = false;
    let mut cur_file = String::new();
    let mut cur_lens: Vec<usize> = Vec::new();
    // line-start byte offsets — O(1) hit-text access instead of re-scanning
    // the file prefix per hit (offsets don't borrow fsrc, so they can live
    // across visit calls, unlike a Vec<&str> of the lines)
    let mut cur_offsets: Vec<usize> = Vec::new();
    let mut cur_lns: Vec<usize> = Vec::new();
    scan_ref_sites(root, conn, name, None, path_filter, |rel, ln, _, _, fsrc| {
        if rel != cur_file {
            if !cur_lns.is_empty() {
                baseline += db::baseline_tokens(&cur_lens, &cur_lns);
            }
            cur_file = rel.to_string();
            cur_lens = fsrc.lines().map(|l| l.len()).collect();
            cur_offsets = std::iter::once(0)
                .chain(fsrc.match_indices('\n').map(|(i, _)| i + 1))
                .collect();
            cur_lns.clear();
        }
        let ln = ln as usize;
        cur_lns.push(ln);
        let text = cur_offsets
            .get(ln - 1)
            .and_then(|&st| fsrc[st..].lines().next())
            .unwrap_or("")
            .trim();
        hits.push((rel.to_string(), ln, text.to_string()));
        if hits.len() >= limit {
            truncated = true;
            return false;
        }
        true
    })?;
    if !cur_lns.is_empty() {
        baseline += db::baseline_tokens(&cur_lens, &cur_lns);
    }
    if json {
        let items: Vec<_> = hits
            .iter()
            .map(|(f, l, t)| serde_json::json!({"file": f, "line": l, "text": t}))
            .collect();
        return jout(&items, baseline);
    }
    let mut out = String::new();
    let mut current = "";
    for (f, l, t) in &hits {
        if f.as_str() != current {
            out.push_str(&format!("{f}\n"));
            current = f;
        }
        out.push_str(&format!("  {l}: {t}\n"));
    }
    if truncated {
        out.push_str("… truncated (raise --limit)\n");
    }
    if hits.is_empty() {
        // An empty result is a dead end unless it names the next move: in-scope
        // misses are usually a too-narrow --path, global misses a wrong name.
        out.push_str(&match path_filter {
            Some(pf) => format!(
                "no references to '{name}' under '{pf}' — try without --path, or `cona find {name}`\n"
            ),
            None => format!("no references to '{name}' — try `cona find {name}` or `cona grep {name}`\n"),
        });
    }
    Ok((out, baseline))
}

/// One budgeted context pack for a symbol: its full source, the signatures of
/// indexed symbols its body references (callees), and the sites that call it
/// (callers, deduped per enclosing symbol). Replaces the show → refs → N×show
/// round-trip chain with a single command.
pub fn cmd_context(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    budget: i64,
    no_tests: bool,
    json: bool,
) -> Result<(String, i64)> {
    let (path, s, e, q) = locate_fresh(root, conn, symbol, None)?;
    let src = std::fs::read_to_string(root.join(&path))?;
    let lines: Vec<&str> = src.lines().collect();
    let body_end = (e as usize).min(lines.len());
    let body_start = (s as usize).saturating_sub(1).min(body_end);
    let body = lines[body_start..body_end].join("\n");
    let name = db::name_tail(&q).to_string();

    // callees: identifiers in the body that resolve to indexed symbols
    // (semantic with textual fail-open fallback — policy lives in lang.rs).
    // Ordered-unique by name, carrying the call-site arg count (arity signal)
    // and the line of the first occurrence (for the semantic-resolve tier).
    let mut idents: Vec<(String, Option<usize>, usize)> = Vec::new();
    {
        let mut seen = std::collections::HashSet::new();
        for (n, ln, is_call, argc) in
            lang::ident_occurrences_failopen(lang::detect_lang(&path), &src)
        {
            if !is_call || ln < s as usize || ln > e as usize || n.len() < 2 {
                continue;
            }
            if seen.insert(n.clone()) {
                idents.push((n, argc, ln));
            }
        }
    }
    // kind, qualified, path, start, end, sig, ambiguous
    type Callee = (String, String, String, i64, i64, String, bool);
    let mut by_name = conn.prepare(
        "SELECT s.kind, s.qualified, f.path, s.start_line, s.end_line, s.signature
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.name = ?1 ORDER BY f.path, s.start_line LIMIT 6",
    )?;
    let my_scope = graph::scope_parent(&q).map(str::to_string);
    let mut callees: Vec<Callee> = Vec::new();
    let mut callees_capped = false;
    // names still ambiguous after the heuristics, with their call-site line —
    // handed to the semantic-resolve tier in one batch below
    let mut ambiguous_refs: Vec<resolve::Ref> = Vec::new();
    for (ident, argc, ln) in idents {
        if ident == name {
            continue;
        }
        let rows: Vec<Callee> = by_name
            .query_map([&ident], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    false,
                ))
            })?
            .flatten()
            // a call site never points back into the symbol itself; module
            // declarations carry no useful signature
            .filter(|(k, cq, cp, ..)| k != "mod" && !(*cp == path && *cq == q))
            .collect();
        let mut rows = graph::narrow_by_scope(
            my_scope.as_deref(),
            &path,
            argc,
            rows,
            |(_, cq, cp, _, _, sig, _)| graph::Candidate {
                scope: graph::scope_parent(cq).map(String::from),
                file: cp.clone(),
                params: lang::param_count(sig),
                is_method: lang::first_param_is_receiver(sig),
            },
        );
        // several definitions still share this name → mark, never silently pick
        if rows.len() > 1 {
            rows.truncate(2);
            for r in &mut rows {
                r.6 = true;
            }
            ambiguous_refs.push(resolve::Ref {
                line: ln,
                name: ident.clone(),
            });
        }
        callees.extend(rows);
        if callees.len() >= 24 {
            callees_capped = true;
            break;
        }
    }

    // Semantic-resolve tier (fail-open, opt-in on the helper binary): for names
    // still ambiguous after scope/file/dir/arity, ask the out-of-process
    // stack-graphs helper. The candidate definitions for an ambiguous name may
    // live in OTHER files, so we feed those files to the helper as deps and let
    // it resolve cross-file. When it points at exactly ONE of the ambiguous
    // rows (matched by (file, line)), we collapse the rest and clear the mark.
    if !ambiguous_refs.is_empty() {
        // candidate rows = every ambiguous callee, keyed by (bare name, file,
        // line); deps = the distinct non-primary files they live in, so a ref
        // can resolve into them cross-file.
        let candidates: Vec<resolve::Candidate> = callees
            .iter()
            .filter(|c| c.6)
            .map(|c| resolve::Candidate {
                name: db::name_tail(&c.1).to_string(),
                file: c.2.clone(),
                line: c.3,
            })
            .collect();
        let mut dep_files: Vec<String> = candidates
            .iter()
            .filter(|c| c.file != path)
            .map(|c| c.file.clone())
            .collect();
        dep_files.sort();
        dep_files.dedup();
        let deps: Vec<resolve::DepFile> = dep_files
            .into_iter()
            .filter_map(|p| {
                std::fs::read_to_string(root.join(&p))
                    .ok()
                    .map(|source| resolve::DepFile { path: p, source })
            })
            .collect();

        let resolved = resolve::disambiguate(
            lang::detect_lang(&path).unwrap_or(""),
            &path,
            &src,
            &ambiguous_refs,
            &candidates,
            &deps,
        );
        for (r, target) in ambiguous_refs.iter().zip(resolved) {
            let Some((target_file, target_line)) = target else {
                continue; // no clean resolution → leave ambiguous
            };
            let rname = &r.name;
            let keep: Vec<usize> = callees
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.6 && db::name_tail(&c.1) == *rname && c.2 == target_file && c.3 == target_line
                })
                .map(|(i, _)| i)
                .collect();
            if keep.len() != 1 {
                continue;
            }
            // unmark the keeper FIRST (index still valid), then drop the other
            // ambiguous rows for this name. After this the keeper is
            // c.6 == false, so the retain predicate spares it.
            callees[keep[0]].6 = false;
            callees.retain(|c| !(c.6 && db::name_tail(&c.1) == *rname));
        }
    }

    // callers: identifier occurrences of `name` outside the symbol's own
    // range, deduped by enclosing symbol
    type Caller = (String, String, String, i64); // kind, enclosing qualified, path, line
    let mut callers: Vec<Caller> = Vec::new();
    let mut callers_capped = false;
    let mut tests_hidden = 0usize;
    let mut seen_callers: HashSet<(String, String)> = HashSet::new();
    scan_ref_sites(
        root,
        conn,
        &name,
        Some((&path, &src)),
        None,
        |rel, ln1, encl, kind, _| {
            if rel == path && ln1 >= s && ln1 <= e {
                return true; // inside the symbol itself
            }
            if !seen_callers.insert((rel.to_string(), encl.to_string())) {
                return true; // one entry per enclosing symbol and file
            }
            // Well-tested symbols have far more test callers than real ones, and
            // the cap below is first-come — unfiltered, tests crowd out the
            // production call sites that actually answer "who uses this?".
            if no_tests && (entries::is_test_symbol(encl) || entries::is_test_path(rel)) {
                tests_hidden += 1;
                return true;
            }
            callers.push((kind.to_string(), encl.to_string(), rel.to_string(), ln1));
            if callers.len() >= 20 {
                callers_capped = true;
                return false;
            }
            true
        },
    )?;

    // baseline: the files an agent would have opened for the same picture
    let mut involved: HashSet<&str> = HashSet::new();
    involved.insert(&path);
    involved.extend(callees.iter().map(|(_, _, p, ..)| p.as_str()));
    involved.extend(callers.iter().map(|(_, _, p, _)| p.as_str()));
    let mut size_stmt = conn.prepare("SELECT size FROM files WHERE path = ?1")?;
    let mut bytes: i64 = 0;
    for p in involved {
        bytes += size_stmt
            .query_row([p], |r| r.get::<_, i64>(0))
            .unwrap_or(0);
    }
    let baseline = db::est_tokens(bytes as usize);

    let notes = db::notes_for(conn, &q).unwrap_or_default();
    if json {
        let obj = serde_json::json!({
            "symbol": {"file": path, "symbol": q, "start": s, "end": e, "code": body},
            "notes": notes.iter().map(|(_, n, _)| n.clone()).collect::<Vec<_>>(),
            "calls": callees.iter().map(|(k, cq, p, cs, ce, sig, amb)| {
                serde_json::json!({"kind": k, "symbol": cq, "file": p, "start": cs, "end": ce, "sig": sig, "ambiguous": amb})
            }).collect::<Vec<_>>(),
            "calls_capped": callees_capped,
            "called_by": callers.iter().map(|(k, cq, p, l)| {
                serde_json::json!({"kind": k, "symbol": cq, "file": p, "line": l})
            }).collect::<Vec<_>>(),
            "called_by_capped": callers_capped,
            "test_callers_hidden": tests_hidden,
        });
        return jout(&obj, baseline);
    }

    // assemble within budget: source always in full (context without the
    // body is useless) — --budget bounds only the calls/called-by sections
    let mut seed = String::new();
    render_symbol_body(&mut seed, &q, &path, s, e, &lines, &notes);
    let mut bo = BudgetOut::new(seed, budget);
    for (title, capped, entries) in [
        (
            "calls",
            callees_capped,
            callees
                .iter()
                .map(|(k, cq, p, cs, ce, sig, amb)| {
                    let mark = if *amb { "  ·ambiguous" } else { "" };
                    format!("  {k} {cq}  {p}:{cs}-{ce}  {sig}{mark}\n")
                })
                .collect::<Vec<_>>(),
        ),
        (
            "called by",
            callers_capped,
            callers
                .iter()
                .map(|(k, cq, p, l)| {
                    if cq.is_empty() {
                        format!("  {p}:{l}\n")
                    } else {
                        format!("  {k} {cq}  {p}:{l}\n")
                    }
                })
                .collect::<Vec<_>>(),
        ),
    ] {
        if entries.is_empty() {
            continue;
        }
        bo.push_always(&format!(
            "── {title} ({}{}) ──\n",
            entries.len(),
            if capped { "+" } else { "" }
        ));
        for entry in entries {
            if !bo.try_push(&entry) {
                break;
            }
        }
        if capped {
            bo.push_always("  … more (cap hit — use `cona refs` for the full list)\n");
        }
    }
    // never let a filter read as an absence — say what was withheld
    if tests_hidden > 0 {
        bo.push_always(&format!(
            "  ({tests_hidden} test caller{} hidden — drop --no-tests to include)\n",
            if tests_hidden == 1 { "" } else { "s" }
        ));
    }
    let out = bo.finish("… truncated (raise --budget)\n");
    Ok((out, baseline))
}

/// Changed SYMBOLS instead of changed lines: parses `git diff --unified=0`
/// against a ref, maps new-side line ranges onto the (fresh) symbol index and
/// reports the innermost symbols they touch. Untracked code files appear as
/// whole new files. Review context for a fraction of the raw-diff tokens.
pub fn cmd_diff(root: &Path, conn: &Connection, gitref: &str, json: bool) -> Result<(String, i64)> {
    let diff_out = gitmap::run_git(root, &["diff", "--unified=0", gitref])?;
    let mut changes = diffmap::parse_unified(&diff_out);

    // untracked (but not ignored) code files = entirely new
    let status = gitmap::run_git(root, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    let mut untracked: HashSet<String> = HashSet::new();
    for line in status.lines() {
        if let Some(p) = line.strip_prefix("?? ") {
            untracked.insert(p.to_string());
        }
    }
    changes.retain(|c| lang::detect_lang(&c.path).is_some());
    let untracked_code: Vec<String> = {
        let mut v: Vec<String> = untracked
            .into_iter()
            .filter(|p| lang::detect_lang(p).is_some())
            .collect();
        v.sort();
        v
    };

    // (file, status, [symbols]) — symbols as (kind, qualified, start, end)
    type SymRow = (String, String, i64, i64);
    let mut report: Vec<(String, &'static str, Vec<SymRow>)> = Vec::new();
    let mut bytes: i64 = 0;
    let mut sym_stmt = conn.prepare(
        "SELECT s.kind, s.qualified, s.start_line, s.end_line
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE f.path = ?1 ORDER BY s.start_line, s.end_line DESC",
    )?;
    let mut size_stmt = conn.prepare("SELECT size FROM files WHERE path = ?1")?;

    let mut handle_file = |path: &str,
                           ranges: Option<&[(i64, i64)]>,
                           sym_stmt: &mut rusqlite::Statement,
                           size_stmt: &mut rusqlite::Statement|
     -> Result<Vec<SymRow>> {
        // symbol ranges come from the index — refresh before mapping
        indexer::ensure_fresh(root, conn, path);
        bytes += size_stmt
            .query_row([path], |r| r.get::<_, i64>(0))
            .unwrap_or(0);
        let all: Vec<SymRow> = sym_stmt
            .query_map([path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .flatten()
            .collect();
        let hit: Vec<SymRow> = match ranges {
            // new file: top-level symbols are the summary
            None => all.iter().filter(|s| !s.1.contains('.')).cloned().collect(),
            Some(ranges) => {
                let touched: Vec<SymRow> = all
                    .iter()
                    .filter(|(_, _, s, e)| diffmap::overlaps(*s, *e, ranges))
                    .cloned()
                    .collect();
                // drop a container only when ALL of its changed lines lie
                // inside nested hit symbols (else the container itself
                // changed and must stay)
                touched
                    .iter()
                    .filter(|(_, q, s, e)| {
                        let nested: Vec<(i64, i64)> = touched
                            .iter()
                            .filter(|(_, q2, s2, e2)| {
                                q2 != q && *s2 >= *s && *e2 <= *e && (*s2 > *s || *e2 < *e)
                            })
                            .map(|(_, _, s2, e2)| (*s2, *e2))
                            .collect();
                        nested.is_empty() || diffmap::has_uncovered(*s, *e, ranges, &nested)
                    })
                    .cloned()
                    .collect()
            }
        };
        Ok(hit)
    };

    for c in &changes {
        if c.deleted {
            report.push((c.path.clone(), "deleted", Vec::new()));
            continue;
        }
        let syms = handle_file(&c.path, Some(&c.ranges), &mut sym_stmt, &mut size_stmt)?;
        report.push((c.path.clone(), "modified", syms));
    }
    for p in &untracked_code {
        let syms = handle_file(p, None, &mut sym_stmt, &mut size_stmt)?;
        report.push((p.clone(), "new", syms));
    }
    let baseline = db::est_tokens(bytes as usize);

    if json {
        let items: Vec<_> = report
            .iter()
            .flat_map(|(file, status, syms)| {
                if syms.is_empty() {
                    vec![serde_json::json!({"file": file, "status": status})]
                } else {
                    syms.iter()
                        .map(|(k, q, s, e)| {
                            serde_json::json!({"file": file, "status": status, "kind": k, "symbol": q, "start": s, "end": e})
                        })
                        .collect()
                }
            })
            .collect();
        return jout(&items, baseline);
    }
    if report.is_empty() {
        return Ok((format!("no code changes vs {gitref}\n"), baseline));
    }
    let mut out = String::new();
    for (file, status, syms) in &report {
        match *status {
            "modified" => out.push_str(&format!("{file}\n")),
            s => out.push_str(&format!("{file} ({s})\n")),
        }
        if syms.is_empty() && *status == "modified" {
            out.push_str("  (changes outside indexed symbols)\n");
        }
        for (k, q, s, e) in syms {
            out.push_str(&format!("  {k} {q} :{s}-{e}\n"));
        }
    }
    Ok((out, baseline))
}

/// Substring search over indexed code files only. Each hit is mapped to its
/// enclosing symbol so the agent can jump straight to `show <Symbol>` instead
/// of reading around the line.
pub fn cmd_grep(
    root: &Path,
    conn: &Connection,
    pattern: &str,
    ignore_case: bool,
    limit: usize,
    path_filter: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let pf = PathFilter::new(root, path_filter);
    let mut stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let mut files: Vec<String> = stmt.query_map([], |r| r.get(0))?.flatten().collect();
    // rg (or grep) prefilters the candidate files far faster than reading
    // everything in-process; on any failure we fall back to the full scan.
    // A directory scope is handed to rg as its search root, so a scoped query
    // walks that subtree instead of the whole repo.
    if let Some(candidates) = grep_prefilter(root, pattern, ignore_case, pf.search_root()) {
        files.retain(|f| candidates.contains(f));
    }
    files.retain(|f| pf.ok(f));
    let mut enclosing = conn.prepare(ENCLOSING_SYMBOL_SQL)?;
    let needle = if ignore_case {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };
    let mut hits: Vec<(String, usize, String, String)> = Vec::new();
    // Honest baseline: per hit file, a grep pass + a Read window around each
    // match line — what the same search costs an agent without cona, NOT
    // the whole file.
    let mut baseline: i64 = 0;
    let mut truncated = false;
    'outer: for rel in files {
        let Ok(src) = std::fs::read_to_string(root.join(&rel)) else {
            continue;
        };
        let mut match_lines: Vec<usize> = Vec::new();
        // Full per-line lengths up front so the ±READ_PAD_LINES baseline window
        // isn't clamped short when a match sits near the file's end or when the
        // limit truncates this file mid-scan.
        let line_lens: Vec<usize> = src.lines().map(str::len).collect();
        for (ln, line) in src.lines().enumerate() {
            let matched = if ignore_case {
                line.to_lowercase().contains(&needle)
            } else {
                line.contains(&needle)
            };
            if !matched {
                continue;
            }
            if match_lines.is_empty() {
                // symbol ranges come from the index — refresh before labeling
                indexer::ensure_fresh(root, conn, &rel);
            }
            match_lines.push(ln + 1);
            let sym: String = enclosing
                .query_row(rusqlite::params![rel, (ln + 1) as i64], |r| r.get(0))
                .unwrap_or_default();
            hits.push((rel.clone(), ln + 1, sym, line.trim().to_string()));
            if hits.len() >= limit {
                truncated = true;
                baseline += db::baseline_tokens(&line_lens, &match_lines);
                break 'outer;
            }
        }
        if !match_lines.is_empty() {
            baseline += db::baseline_tokens(&line_lens, &match_lines);
        }
    }
    if json {
        let items: Vec<_> = hits
            .iter()
            .map(|(f, l, sym, t)| {
                serde_json::json!({"file": f, "line": l, "symbol": sym, "text": t})
            })
            .collect();
        return jout(&items, baseline);
    }
    let mut out = String::new();
    for (f, l, sym, t) in &hits {
        if sym.is_empty() {
            out.push_str(&format!("{f}:{l}: {t}\n"));
        } else {
            out.push_str(&format!("{f}:{l} (in {sym}): {t}\n"));
        }
    }
    if truncated {
        out.push_str("… truncated (raise --limit)\n");
    }
    if hits.is_empty() {
        out.push_str(&format!("no matches for '{pattern}'"));
        // `grep` is deliberately fixed-string. A pattern that *looks* like a
        // regex returning zero hits is the worst failure mode — the agent
        // concludes the code doesn't exist. Say why instead of staying silent.
        if let Some(literal) = regexish_literal(pattern) {
            out.push_str(&format!(
                "\n  note: matching is literal (no regex) — '{}' was searched verbatim.",
                pattern
            ));
            if !literal.is_empty() {
                out.push_str(&format!("\n  try `cona grep {literal}`"));
            }
            out.push_str(" — or `rg` for a real regex");
        } else if path_filter.is_some() {
            out.push_str(" — try without --path");
        }
        out.push('\n');
    }
    Ok((out, baseline))
}

/// Regex metacharacters that make a pattern *look* like a regex. Used only to
/// explain a zero-hit fixed-string search — never to change matching.
fn regexish_literal(pattern: &str) -> Option<String> {
    const META: [char; 11] = ['(', ')', '[', ']', '|', '+', '*', '?', '^', '$', '\\'];
    if !pattern.contains(|c| META.contains(&c)) {
        return None;
    }
    // The longest run of plain characters is the best literal fallback to
    // suggest (`tokens_(out|saved)` → `tokens_`).
    Some(
        pattern
            .split(|c| META.contains(&c) || c == '.' || c == '{' || c == '}')
            .max_by_key(|s| s.len())
            .unwrap_or("")
            .to_string(),
    )
}

/// Fixed-string list of files containing `pattern`, via ripgrep when
/// installed, system grep as fallback. `None` = no prefilter available
/// (tool missing or errored) — caller scans everything, fail-open.
/// `scope` (a repo-relative directory) becomes the search root, so a scoped
/// query makes rg walk only that subtree. Hits come back relative to it, so the
/// scope is prefixed again to keep every path repo-relative.
pub(super) fn grep_prefilter(
    root: &Path,
    pattern: &str,
    ignore_case: bool,
    scope: Option<&str>,
) -> Option<HashSet<String>> {
    let attempts: [(&str, Vec<&str>); 2] = [
        (
            "rg",
            vec!["--files-with-matches", "--fixed-strings", "--no-messages"],
        ),
        ("grep", vec!["-r", "-l", "-I", "-F", "-s"]),
    ];
    for (bin, mut args) in attempts {
        if ignore_case {
            args.push("-i");
        }
        let search_dir = scope.unwrap_or(".");
        args.extend(["--", pattern, search_dir]);
        let out = match std::process::Command::new(bin)
            .args(&args)
            .current_dir(root)
            .output()
        {
            Ok(o) => o,
            Err(_) => continue, // not installed
        };
        // 0 = matches, 1 = no matches; anything else = error → next attempt
        match out.status.code() {
            Some(0) | Some(1) => {}
            _ => continue,
        }
        return Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim_start_matches("./").to_string())
                .collect(),
        );
    }
    None
}
