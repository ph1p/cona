//! Indexed-symbol lexical discovery; intentionally not semantic identity.
use crate::commands::{jout, PathFilter};
use crate::{db, indexer, search};
use anyhow::{bail, Result};
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

struct Symbol {
    name: String,
    qualified: String,
    kind: String,
    start: usize,
    end: usize,
    sig: String,
}

struct Hit {
    file: String,
    selector: String,
    symbol: Symbol,
    matched: search::Match,
    line: usize,
    excerpt: String,
}

/// Search only indexed files, refreshing before accessing stored line ranges.
/// Scores are lexical evidence, never confidence or resolved symbol identity.
pub fn cmd_search(
    conn: &Connection,
    root: &Path,
    query: &str,
    limit: usize,
    path: Option<&str>,
    json_output: bool,
) -> Result<(String, i64)> {
    let pf = PathFilter::new(root, path);
    let terms = search::query_tokens(query);
    let mut hits = Vec::new();
    let mut lengths = BTreeMap::new();
    let mut stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let files = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    // Even stopword-only input has a useful exact-name interpretation.
    if !query.trim().is_empty() {
        for file in files.into_iter().filter(|f| pf.ok(f)) {
            if !root.join(&file).is_file() {
                continue;
            }
            indexer::ensure_fresh(root, conn, &file);
            if indexer::is_stale(root, conn, &file) {
                bail!("{file} changed since it was indexed; run `cona index` before searching");
            }
            let source = std::fs::read_to_string(root.join(&file))?;
            let lines: Vec<_> = source.lines().collect();
            let mut stmt = conn.prepare(
                "SELECT s.name,s.qualified,s.kind,s.start_line,s.end_line,s.signature
                 FROM symbols s JOIN files f ON f.id=s.file_id WHERE f.path=?1
                 ORDER BY s.start_line,s.end_line,s.qualified",
            )?;
            let symbols = stmt
                .query_map([&file], |r| {
                    Ok(Symbol {
                        name: r.get(0)?,
                        qualified: r.get(1)?,
                        kind: r.get(2)?,
                        start: r.get::<_, u32>(3)? as usize,
                        end: r.get::<_, u32>(4)? as usize,
                        sig: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // A file-qualified name can still be ambiguous (overloads, or a
            // type and its impl). Count every indexed candidate, not just the
            // ranked hits, so filtering/limits cannot hide that ambiguity.
            let mut name_counts = BTreeMap::<String, usize>::new();
            for sym in &symbols {
                *name_counts.entry(sym.qualified.clone()).or_default() += 1;
                if sym.name != sym.qualified {
                    *name_counts.entry(sym.name.clone()).or_default() += 1;
                }
            }
            // Assign shared lines to the smallest span, preventing container
            // bodies from repeating evidence in nested methods/functions.
            let mut owner = vec![None; lines.len()];
            for (i, sym) in symbols.iter().enumerate() {
                for slot in owner
                    .iter_mut()
                    .take(sym.end)
                    .skip(sym.start.saturating_sub(1))
                {
                    if slot.is_none_or(|old: usize| {
                        sym.end.saturating_sub(sym.start)
                            < symbols[old].end.saturating_sub(symbols[old].start)
                    }) {
                        *slot = Some(i);
                    }
                }
            }
            for (i, sym) in symbols.into_iter().enumerate() {
                let start = sym.start.saturating_sub(1).min(lines.len());
                let mut doc_start = start;
                while doc_start > 0 && start - doc_start < 24 {
                    let prev = lines[doc_start - 1].trim();
                    if prev.starts_with("//")
                        || prev.starts_with('#')
                        || prev.starts_with('*')
                        || prev.starts_with("/*")
                        || prev.starts_with("--")
                    {
                        doc_start -= 1;
                    } else {
                        break;
                    }
                }
                let docs = lines[doc_start..start].join("\n");
                let owned: Vec<_> = lines
                    .iter()
                    .enumerate()
                    .take(sym.end)
                    .skip(start)
                    .filter(|(n, _)| owner[*n] == Some(i))
                    .collect();
                let body = owned
                    .iter()
                    .map(|(_, l)| **l)
                    .collect::<Vec<_>>()
                    .join("\n");
                let Some(matched) = search::score(
                    query,
                    &terms,
                    &search::Fields {
                        name: &sym.name,
                        qualified: &sym.qualified,
                        signature: &sym.sig,
                        docs: &docs,
                        body: &body,
                    },
                ) else {
                    continue;
                };
                // Prefer a line exhibiting the most query terms; ties retain
                // source order. Include leading docs in excerpt selection.
                let evidence = (doc_start..start)
                    .map(|n| (n, lines[n]))
                    .chain(owned.iter().map(|(n, l)| (*n, **l)));
                let mut best = (start, lines.get(start).copied().unwrap_or(""), 0);
                for (n, text) in evidence {
                    let count = terms.intersection(&search::tokens(text)).count();
                    if count > best.2 {
                        best = (n, text, count);
                    }
                }
                let selector = if name_counts[&sym.qualified] > 1 {
                    format!("{file}:{}-{}", sym.start, sym.end)
                } else {
                    format!("{file}:{}", sym.qualified)
                };
                hits.push(Hit {
                    file: file.clone(),
                    selector,
                    symbol: sym,
                    matched,
                    line: best.0 + 1,
                    excerpt: search::excerpt(best.1.trim(), 240),
                });
            }
            lengths.insert(file, lines.iter().map(|l| l.len()).collect::<Vec<_>>());
        }
    }
    hits.sort_by(|a, b| {
        b.matched
            .score
            .cmp(&a.matched.score)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.symbol.start.cmp(&b.symbol.start))
            .then_with(|| a.symbol.qualified.cmp(&b.symbol.qualified))
            .then_with(|| a.symbol.end.cmp(&b.symbol.end))
    });
    let limit = limit.min(100);
    let mut truncated = hits.len() > limit;
    hits.truncate(limit);
    // Keep selectors intact (truncating them makes the next show unusable).
    // Bound their aggregate size instead; fixed allowances cover escaped
    // excerpts/signatures/reasons and JSON keys, including control characters.
    let mut bytes = 0usize;
    let keep = hits
        .iter()
        .take_while(|h| {
            bytes = bytes.saturating_add(
                (h.file.len() + h.symbol.qualified.len() + h.symbol.kind.len())
                    .saturating_mul(12)
                    .saturating_add(16_000),
            );
            bytes <= 512_000
        })
        .count();
    truncated |= keep < hits.len();
    hits.truncate(keep);
    let mut sites: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for hit in &hits {
        sites.entry(&hit.file).or_default().push(hit.line);
    }
    let baseline = sites
        .iter()
        .map(|(f, lines)| db::baseline_tokens(&lengths[*f], lines))
        .sum();
    if json_output {
        let results: Vec<_> = hits
            .iter()
            .map(|h| {
                json!({
                    "file": h.file, "kind": h.symbol.kind, "symbol": h.symbol.qualified,
                    "selector": h.selector,
                    "start": h.symbol.start, "end": h.symbol.end,
                    "sig": search::excerpt(&h.symbol.sig, 240),
                    "score": h.matched.score, "reasons": h.matched.reasons.iter()
                        .map(|r| search::excerpt(r, 240)).collect::<Vec<_>>(),
                    "excerpt": {"line": h.line, "text": h.excerpt},
                })
            })
            .collect();
        return jout(
            &json!({"query": search::excerpt(query, 512), "lexical": true,
            "truncated": truncated, "results": results}),
            baseline,
        );
    }
    let mut out = String::from("Lexical matches only — not semantic identity.\n");
    for h in &hits {
        out.push_str(&format!(
            "{} {} ({})  {}-{}  score {}\n  {}\n  {}: {}\n",
            h.symbol.kind,
            h.selector,
            h.symbol.qualified,
            h.symbol.start,
            h.symbol.end,
            h.matched.score,
            h.matched
                .reasons
                .iter()
                .map(|r| search::excerpt(r, 240))
                .collect::<Vec<_>>()
                .join("; "),
            h.line,
            h.excerpt
        ));
    }
    if hits.is_empty() {
        out.push_str("No lexical matches.\n");
    }
    if truncated {
        out.push_str("(more matches; narrow --path or raise --limit, maximum 100)\n");
    }
    Ok((out, baseline))
}
