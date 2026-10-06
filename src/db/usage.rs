//! Usage logging, notes, and the honest savings baseline.

use super::*;
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

pub fn log_usage(
    root: &Path,
    cmd: &str,
    ms: i64,
    results: i64,
    tokens_out: i64,
    tokens_saved: i64,
) {
    log_usage_detail(root, cmd, ms, results, tokens_out, tokens_saved, "");
}

/// Like `log_usage` but records the query target (symbol/file/pattern).
pub fn log_usage_detail(
    root: &Path,
    cmd: &str,
    ms: i64,
    results: i64,
    tokens_out: i64,
    tokens_saved: i64,
    detail: &str,
) {
    log_usage_outcome(root, cmd, ms, results, tokens_out, tokens_saved, detail, "");
}

/// The one INSERT. `outcome` is `""` when the query answered, else from
/// `outcome_of_output`/`outcome_of_error` — read back by `cona learn`.
#[allow(clippy::too_many_arguments)]
pub fn log_usage_outcome(
    root: &Path,
    cmd: &str,
    ms: i64,
    results: i64,
    tokens_out: i64,
    tokens_saved: i64,
    detail: &str,
    outcome: &str,
) {
    if let Ok(g) = open_global_db() {
        let _ = g.execute(
            "INSERT INTO usage(ts, project, cmd, ms, results, tokens_out, tokens_saved, detail, outcome)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                now(),
                root.to_string_lossy(),
                cmd,
                ms,
                results,
                tokens_out,
                tokens_saved.max(0),
                detail,
                outcome
            ],
        );
    }
}

/// Outcome of an Ok query, keyed on the empty-result renders. `""` = answered.
pub fn outcome_of_output(out: &str) -> &'static str {
    let first = out.trim_start();
    let empty = ["no match", "no references to", "no symbols indexed"];
    if empty.iter().any(|p| first.starts_with(p)) {
        "empty"
    } else {
        ""
    }
}

/// Outcome of a query that failed — keyed on the messages `locate_rows` /
/// `locate_symbol_kind` raise, so a miss and an ambiguity stay separable.
pub fn outcome_of_error(msg: &str) -> &'static str {
    if msg.contains("ambiguous '") {
        "ambiguous"
    } else if msg.contains("not found") {
        "miss"
    } else {
        "error"
    }
}

/// Symbol annotations — the knowledge layer. Keyed by qualified name (or bare
/// name/file path); lookups also match the last segment, so `Foo.bar` notes
/// surface for `bar`.
pub fn note_add(conn: &Connection, symbol: &str, note: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO notes(symbol, note, ts) VALUES (?1, ?2, ?3)",
        rusqlite::params![symbol, note, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn note_rm(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM notes WHERE id = ?1", [id])? > 0)
}

/// Last `.`-segment of a qualified name — THE convention for matching
/// qualified and bare symbol forms (notes, tests, shape, rename all use it).
pub fn name_tail(sym: &str) -> &str {
    sym.rsplit('.').next().unwrap_or(sym)
}

/// Notes attached to `symbol`: exact key or matching last `.`-segment. Notes
/// stay few, so filtering in Rust beats clever SQL.
pub fn notes_for(conn: &Connection, symbol: &str) -> Result<Vec<(i64, String, i64)>> {
    let tail = name_tail(symbol);
    let mut out = Vec::new();
    for (id, key, note, ts) in notes_all(conn)? {
        if key == symbol || name_tail(&key) == tail {
            out.push((id, note, ts));
        }
    }
    Ok(out)
}

pub fn notes_all(conn: &Connection) -> Result<Vec<(i64, String, String, i64)>> {
    let mut stmt = conn.prepare("SELECT id, symbol, note, ts FROM notes ORDER BY symbol, ts")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .flatten()
        .collect();
    Ok(rows)
}

/// Rough token estimate (chars / 4).
pub fn est_tokens(chars: usize) -> i64 {
    (chars as i64 + 3) / 4
}

/// Ceiling (~8 mid-size files) on the modeled baseline of an orientation query
/// (`tree`). The raw "every file read once" model grows with the repo, but no
/// agent reads 500 files to orient. `stats` applies the same cap to older rows
/// (`db::stats::SAVED_SUM`).
pub const ORIENT_BASELINE_CAP: i64 = 20_000;

/// Baseline for reading `bytes` of source to orient, capped.
pub fn orient_baseline(bytes: usize) -> i64 {
    est_tokens(bytes).min(ORIENT_BASELINE_CAP)
}

/// Lines of context a disciplined agent Reads around a hit before/after — the
/// padding in the grep-then-Read baseline model (see `baseline_tokens`).
pub const READ_PAD_LINES: usize = 40;

/// Honest savings baseline: what the SAME lookup costs WITHOUT cona — a grep
/// pass (≈free) plus a targeted `Read offset/limit` window per hit, NOT the
/// whole file.
///
/// `line_lens` = every line length (chars) of the hits' file; `hits` = 1-based
/// hit lines. `±READ_PAD_LINES` windows are merged and summed, capped at the
/// whole file — never more than a naive whole-file read, and only the realistic
/// window for a symbol in a huge file. Empty `hits` ⇒ whole file (nothing to
/// anchor on, so the agent scans it all).
pub fn baseline_tokens(line_lens: &[usize], hits: &[usize]) -> i64 {
    let n = line_lens.len();
    if n == 0 {
        return 0;
    }
    if hits.is_empty() {
        return est_tokens(line_lens.iter().sum::<usize>() + n); // +n ≈ newlines
    }
    // Merge ±pad windows (1-based, clamped to [1, n]) into disjoint ranges.
    let mut wins: Vec<(usize, usize)> = hits
        .iter()
        .filter(|&&h| h >= 1 && h <= n)
        .map(|&h| {
            let lo = h.saturating_sub(READ_PAD_LINES).max(1);
            let hi = (h + READ_PAD_LINES).min(n);
            (lo, hi)
        })
        .collect();
    if wins.is_empty() {
        return est_tokens(line_lens.iter().sum::<usize>() + n);
    }
    wins.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(wins.len());
    for (lo, hi) in wins {
        match merged.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    let chars: usize = merged
        .iter()
        .map(|&(lo, hi)| line_lens[lo - 1..hi].iter().sum::<usize>() + (hi - lo + 1))
        .sum();
    est_tokens(chars)
}

/// Maintenance rows (index/edit/hook:*) carry no savings — kept out of the
/// savings table, shown as a compact one-liner.
pub fn is_maintenance_cmd(cmd: &str) -> bool {
    matches!(cmd, "index" | "edit" | "rename" | "note") || cmd.starts_with("hook:")
}
