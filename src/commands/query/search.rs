//! Indexed-symbol lexical discovery; intentionally not semantic identity.
use crate::commands::{jout, PathFilter};
use crate::{db, indexer, search};
use anyhow::{bail, Result};
use rusqlite::Connection;
use serde_json::json;
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::path::Path;
use std::rc::Rc;

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
    // Shared only by retained hits from this file (and the active scan).
    lengths: Rc<Vec<usize>>,
    ordinal: usize,
}

/// The original full-sort order. An ordinal preserves stable encounter order
/// even for symbols whose complete ranking keys are identical.
fn rank<'a>(
    score: usize,
    file: &'a str,
    symbol: &'a Symbol,
    ordinal: usize,
) -> (Reverse<usize>, &'a str, usize, &'a str, usize, usize) {
    (
        Reverse(score),
        file,
        symbol.start,
        &symbol.qualified,
        symbol.end,
        ordinal,
    )
}

impl Ord for Hit {
    fn cmp(&self, other: &Self) -> Ordering {
        rank(self.matched.score, &self.file, &self.symbol, self.ordinal).cmp(&rank(
            other.matched.score,
            &other.file,
            &other.symbol,
            other.ordinal,
        ))
    }
}

impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Hit {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Hit {}

/// Max-heap: the worst retained candidate is the first to be displaced.
struct TopHits {
    heap: BinaryHeap<Hit>,
    limit: usize,
    seen: usize,
}

impl TopHits {
    fn new(limit: usize) -> Self {
        Self {
            heap: BinaryHeap::new(),
            limit: limit.min(100),
            seen: 0,
        }
    }

    /// Count every match, including at capacity zero, before building display
    /// evidence or retaining file lengths. The returned ordinal is scan order.
    fn consider(&mut self, score: usize, file: &str, symbol: &Symbol) -> Option<usize> {
        let ordinal = self.seen;
        self.seen += 1;
        if self.limit == 0 {
            return None;
        }
        if self.heap.len() < self.limit
            || self.heap.peek().is_some_and(|worst| {
                rank(score, file, symbol, ordinal)
                    < rank(
                        worst.matched.score,
                        &worst.file,
                        &worst.symbol,
                        worst.ordinal,
                    )
            })
        {
            Some(ordinal)
        } else {
            None
        }
    }

    fn push(&mut self, hit: Hit) {
        if self.heap.len() == self.limit {
            self.heap.pop();
        }
        self.heap.push(hit);
    }

    fn finish(self) -> (Vec<Hit>, bool) {
        let truncated = self.seen > self.limit;
        (self.heap.into_sorted_vec(), truncated)
    }
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
    let mut hits = TopHits::new(limit);
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
            let mut lengths = None;
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
                let Some(ordinal) = hits.consider(matched.score, &file, &sym) else {
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
                    lengths: Rc::clone(
                        lengths.get_or_insert_with(|| {
                            Rc::new(lines.iter().map(|l| l.len()).collect())
                        }),
                    ),
                    ordinal,
                });
            }
        }
    }
    let (mut hits, mut truncated) = hits.finish();
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
    let mut sites: BTreeMap<&str, (&[usize], Vec<usize>)> = BTreeMap::new();
    for hit in &hits {
        sites
            .entry(&hit.file)
            .or_insert_with(|| (&hit.lengths, Vec::new()))
            .1
            .push(hit.line);
    }
    let baseline = sites
        .values()
        .map(|(lengths, lines)| db::baseline_tokens(lengths, lines))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(n: usize, ordinal: usize, lengths: Rc<Vec<usize>>) -> Hit {
        Hit {
            file: format!("f{:02}.rs", n % 11),
            selector: format!("candidate-{n}"),
            symbol: Symbol {
                name: "candidate".into(),
                qualified: ["café", "alpha", "λ", "z"][n % 4].into(),
                kind: "function".into(),
                start: n % 7 + 1,
                end: n % 7 + 2,
                sig: String::new(),
            },
            matched: search::Match {
                score: n % 5,
                reasons: vec![],
            },
            line: n % lengths.len() + 1,
            excerpt: String::new(),
            lengths,
            ordinal,
        }
    }

    /// Full stable sort is deliberately retained as the ordering oracle, not
    /// used in production. Zero ordinal compares only the original keys.
    fn full_sort(mut hits: Vec<Hit>, limit: usize) -> (Vec<Hit>, bool) {
        hits.sort_by(|a, b| {
            rank(a.matched.score, &a.file, &a.symbol, 0).cmp(&rank(
                b.matched.score,
                &b.file,
                &b.symbol,
                0,
            ))
        });
        let truncated = hits.len() > limit.min(100);
        hits.truncate(limit.min(100));
        (hits, truncated)
    }

    #[test]
    fn search_bounded_selection_matches_stable_full_sort() {
        // Repeated n values have identical complete ranking keys, but distinct
        // encounter ordinals. Reverse insertion also exercises replacement.
        for reverse in [false, true] {
            for count in [0, 1, 10, 100, 800] {
                for limit in [0, 1, 10, 100, 101, usize::MAX] {
                    let lengths = Rc::new(vec![3; 20]);
                    let numbers: Vec<_> = (0..count)
                        .map(|i| if reverse { count - i - 1 } else { i })
                        .map(|i| i % 133)
                        .collect();
                    let (expected, truncated) = full_sort(
                        numbers
                            .iter()
                            .enumerate()
                            .map(|(i, &n)| hit(n, i, Rc::clone(&lengths)))
                            .collect(),
                        limit,
                    );
                    let mut bounded = TopHits::new(limit);
                    for n in numbers {
                        let mut h = hit(n, 0, Rc::clone(&lengths));
                        if let Some(ordinal) = bounded.consider(h.matched.score, &h.file, &h.symbol)
                        {
                            h.ordinal = ordinal;
                            bounded.push(h);
                        }
                        assert!(bounded.heap.len() <= limit.min(100));
                    }
                    let (actual, actual_truncated) = bounded.finish();
                    assert_eq!(actual_truncated, truncated);
                    assert_eq!(
                        actual.iter().map(|h| h.ordinal).collect::<Vec<_>>(),
                        expected.iter().map(|h| h.ordinal).collect::<Vec<_>>(),
                        "reverse={reverse} count={count} limit={limit}"
                    );
                }
            }
        }
    }

    #[test]
    fn search_lengths_are_shared_and_evicted_with_last_hit() {
        for limit in [0, 1, 10, 100, 200] {
            let mut bounded = TopHits::new(limit);
            let mut caches = Vec::new();
            for file in 0..400 {
                let file_name = format!("{file}.rs");
                let lengths = Rc::new(vec![file + 1; 120]);
                caches.push(Rc::downgrade(&lengths));
                for n in 0..5 {
                    let mut h = hit(n, 0, Rc::clone(&lengths));
                    h.file = file_name.clone();
                    h.matched.score = file; // Later files displace earlier ones.
                    if let Some(ordinal) = bounded.consider(h.matched.score, &h.file, &h.symbol) {
                        h.ordinal = ordinal;
                        bounded.push(h);
                    }
                    assert!(bounded.heap.len() <= limit.min(100));
                    let represented: std::collections::BTreeSet<_> =
                        bounded.heap.iter().map(|h| &h.file).collect();
                    assert_eq!(
                        caches.iter().filter(|c| c.strong_count() > 0).count(),
                        represented.len() + usize::from(!represented.contains(&file_name)),
                    );
                }
            }
            let (hits, _) = bounded.finish();
            let mut sites: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
            for h in &hits {
                sites.entry(&h.file).or_default().push(h.line);
            }
            // Same baseline as the old all-files length cache, including
            // overlapping windows and multiple results in one file.
            let old: i64 = sites
                .iter()
                .map(|(file, lines)| {
                    let n: usize = file.trim_end_matches(".rs").parse().unwrap();
                    db::baseline_tokens(&vec![n + 1; 120], lines)
                })
                .sum();
            let new: i64 = sites
                .iter()
                .map(|(file, lines)| {
                    let h = hits.iter().find(|h| h.file == *file).unwrap();
                    db::baseline_tokens(&h.lengths, lines)
                })
                .sum();
            assert_eq!(old, new);
            drop(hits);
            assert!(caches.iter().all(|c| c.strong_count() == 0));
        }
    }
}
