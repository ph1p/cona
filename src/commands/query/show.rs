//! `show`: render one symbol (or every candidate with `--all`).

use super::outline::cmd_outline;
use crate::commands::{defaults, jout, locate_fresh, push_numbered_lines, ShowOpts};
use crate::{db, indexer};
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// `show` for one symbol. With `all` and an ambiguous name, every candidate is
/// rendered instead of erroring, saving the agent a round-trip.
pub fn cmd_show(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    opts: ShowOpts<'_>,
    json: bool,
) -> Result<(String, i64)> {
    // `context` is not read here — it is `show_one`'s to apply, per candidate.
    let ShowOpts { kind, sig, all, .. } = opts;
    // A path means "map this file" → outline (directories reach its
    // `tree --path` redirect). The filesystem decides; `split_locator` only
    // keeps a `file.rs:Name` locator from being mistaken for a path.
    if crate::commands::split_locator(symbol).is_none() && root.join(symbol).exists() {
        return cmd_outline(root, conn, symbol, sig, json);
    }
    // Without --all, a SMALL ambiguity pool is auto-expanded: 2–3 short
    // definitions beat a retry round-trip. Big pools (or bodies) still raise
    // `locate_symbol`'s guided error via `show_one` — printing them all is the
    // token sink this tool exists to avoid. `locate_all` only reports >1 when
    // locate erred on ambiguity.
    // candidates from deleted files must not fake an ambiguity (one stat each)
    crate::commands::prune_vanished(root, conn, symbol, kind);
    let cands = if all {
        Some(crate::commands::locate_all(conn, symbol, kind)?)
    } else {
        // a lookup error here falls through to show_one's guided error
        crate::commands::locate_all(conn, symbol, kind)
            .ok()
            .filter(|cands| {
                cands.len() > 1
                    && cands.len() <= defaults::AUTO_ALL_MAX_CANDIDATES
                    && cands.iter().map(|c| c.2 - c.1 + 1).sum::<i64>()
                        <= defaults::AUTO_ALL_MAX_LINES
            })
    };
    let auto_all = !all && cands.is_some();
    if let Some(cands) = cands {
        if cands.len() > 1 {
            // Render from the rows locate_all resolved — re-resolving by
            // `file:Name` re-hits the ambiguity when candidates share a file
            // (enum + impl). Refresh stale candidate files first (invariant 2)
            // and re-run the ONE lookup if anything moved.
            let refreshed =
                indexer::refresh_files(root, conn, cands.iter().map(|(p, ..)| p.as_str()));
            let cands = if refreshed.any_refreshed {
                crate::commands::locate_all(conn, symbol, kind)?
            } else {
                cands
            };
            let mut out = String::new();
            if auto_all {
                out.push_str(&format!(
                    "· '{symbol}' is ambiguous — showing all {} (narrow with \
                     Parent.Name, file.rs:Name, or --kind)\n",
                    cands.len()
                ));
            }
            let mut baseline = 0;
            for c in &cands {
                match show_located(root, conn, c, opts, false, false) {
                    Ok((body, b)) => {
                        out.push_str(&body);
                        out.push('\n');
                        baseline += b;
                    }
                    Err(e) => out.push_str(&format!("{}:{}: {e}\n", c.0, db::name_tail(&c.3))),
                }
            }
            if json {
                return jout(
                    &serde_json::json!({"symbol": symbol, "matches": cands.len(), "text": out}),
                    baseline,
                );
            }
            return Ok((out, baseline));
        }
    }
    show_one(root, conn, symbol, opts, json, true)
}

fn show_one(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    opts: ShowOpts<'_>,
    json: bool,
    disclose_others: bool,
) -> Result<(String, i64)> {
    let located = locate_fresh(root, conn, symbol, opts.kind)?;
    show_located(root, conn, &located, opts, json, disclose_others)
}

/// Render one already-located symbol, so `--all` can print candidates without
/// re-resolving (and re-raising the ambiguity) by name. Callers own freshness:
/// the line range is used as-is (invariant 2).
fn show_located(
    root: &Path,
    conn: &Connection,
    located: &crate::commands::Located,
    opts: ShowOpts<'_>,
    json: bool,
    disclose_others: bool,
) -> Result<(String, i64)> {
    let ShowOpts { context, sig, .. } = opts;
    let (path, s, e, q) = located.clone();
    // --sig: print the indexed signature without reading the file body
    if sig {
        let signature: String = conn
            .query_row(
                "SELECT s.signature FROM symbols s JOIN files f ON f.id = s.file_id
                 WHERE f.path = ?1 AND s.qualified = ?2 AND s.start_line = ?3",
                rusqlite::params![path, q, s],
                |r| r.get(0),
            )
            .unwrap_or_default();
        // Baseline: the whole span, at a per-line width estimated from the
        // index (file size / last symbol end line) — no file read.
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
    // baseline: Read windows anchored on the symbol's first and last line
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
    // them in one trailer line (skipped under `--all`, which prints them all)
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
