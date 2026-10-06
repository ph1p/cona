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

/// The one INSERT. `outcome` is `""` for a query that answered, else one of
/// `outcome_of_output`/`outcome_of_error` — the failure signal `cona learn`
/// and the per-command miss rate read back.
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

/// Outcome of a query that returned Ok: the empty-result renders every query
/// command prints instead of hits. `""` = it answered.
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

/// Rough token estimate (chars / 4).
/// Symbol annotations — the knowledge layer. Keyed by qualified name (or a
/// bare name/file path); lookups match both the qualified name and its last
/// segment so `Foo.bar` notes surface when someone shows `bar`.
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

/// Notes attached to `symbol`: exact key match, or matching last `.`-segment
/// so qualified and bare forms find each other. Notes stay few — filtering in
/// Rust beats a clever SQL expression.
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

pub fn est_tokens(chars: usize) -> i64 {
    (chars as i64 + 3) / 4
}

/// Ceiling on the modeled baseline of an orientation query (`tree`). The raw
/// model — "ranking needs every indexed file read once" — grows with the repo,
/// but no agent reads a 500-file repo to orient: it lists the tree and opens a
/// handful of files. ~8 mid-size files. `stats` applies the same cap to rows
/// logged before it existed (`db::stats::SAVED_SUM`).
pub const ORIENT_BASELINE_CAP: i64 = 20_000;

/// Baseline for reading `bytes` of source to orient, capped.
pub fn orient_baseline(bytes: usize) -> i64 {
    est_tokens(bytes).min(ORIENT_BASELINE_CAP)
}

/// Lines of context a disciplined agent Reads around a hit before/after — the
/// padding in the grep-then-Read baseline model (see `baseline_tokens`).
pub const READ_PAD_LINES: usize = 40;

/// Honest savings baseline: what the SAME lookup would have cost an agent
/// WITHOUT cona — a grep pass (≈free, it only returns line numbers) plus a
/// targeted `Read offset/limit` window around each hit, NOT the whole file.
///
/// `line_lens` is every line length (chars) of the file the hits live in;
/// `hits` are 1-based line numbers the query landed on. Windows of
/// `±READ_PAD_LINES` around each hit are merged where they overlap and summed,
/// capped at the whole file — so a query can never claim to have saved more
/// than a naive whole-file read, and on a symbol buried in a huge file it
/// claims only the realistic window, not the file. Empty `hits` ⇒ whole file
/// (the agent had to scan all of it to find nothing to anchor on).
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

/// Maintenance rows (index/edit/hook:*) never carry savings — they are kept
/// out of the savings table and shown as a compact one-liner instead.
pub fn is_maintenance_cmd(cmd: &str) -> bool {
    matches!(cmd, "index" | "edit" | "rename" | "note") || cmd.starts_with("hook:")
}
