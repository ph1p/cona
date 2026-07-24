//! CLI command implementations. `main.rs` only parses arguments and
//! dispatches here; each submodule groups one concern. Shared plumbing
//! (DB open, usage logging, symbol lookup, output budgeting) lives in
//! this module root.

pub mod callgraph;
pub mod history;
pub mod insight;
pub mod mcp_server;
pub mod mutate;
pub mod query;
pub mod stats;

pub use callgraph::*;
pub use history::*;
pub use insight::*;
pub use mcp_server::*;
pub use mutate::*;
pub use query::*;
pub use stats::*;

use crate::{db, indexer, lang};
use anyhow::{anyhow, bail, Result};
use rusqlite::Connection;
use std::path::Path;
use std::time::Instant;

/// Innermost indexed symbol enclosing a (file, line) — the ONE definition of
/// "which symbol is this line in", shared by context/grep/tests. Prepare once
/// per command, query in the loop. Columns: qualified, kind.
pub(crate) const ENCLOSING_SYMBOL_SQL: &str =
    "SELECT s.qualified, s.kind FROM symbols s JOIN files f ON f.id = s.file_id
     WHERE f.path = ?1 AND s.start_line <= ?2 AND s.end_line >= ?2
     ORDER BY s.start_line DESC LIMIT 1";

pub fn open_indexed(root: &Path) -> Result<Connection> {
    let conn = db::open_project_db(root)?;
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
    if n == 0 {
        // never silently index the whole home dir / filesystem root
        if db::is_home_or_fs_root(root) {
            bail!(
                "refusing to auto-index {} (home/filesystem root) — cd into a project, \
                 or run `cona index` there explicitly",
                root.display()
            );
        }
        // auto-index on first use
        indexer::index_project(root, &conn)?;
        let indexed: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        if indexed == 0 {
            // nothing here — don't leave a junk DB registered globally
            drop(conn);
            let _ = db::remove_project_data(root, true);
            bail!(
                "nothing to index in {} — cd into a project with code files",
                root.display()
            );
        }
    }
    Ok(conn)
}

pub fn finish(root: &Path, cmd: &str, t0: Instant, out: &str, baseline_tokens: i64, detail: &str) {
    let ms = t0.elapsed().as_millis() as i64;
    let tokens_out = db::est_tokens(out.len());
    // Baseline = reading only the files this query's results live in, so a
    // query can never claim to have "saved" more than those files cost.
    let saved = (baseline_tokens - tokens_out).max(0);
    let results = out.lines().count() as i64;
    db::log_usage_detail(root, cmd, ms, results, tokens_out, saved, detail);
}

/// The `--json` return shape every query command shares: one JSON line + the
/// savings baseline.
pub(crate) fn jout<T: serde::Serialize>(value: &T, baseline: i64) -> Result<(String, i64)> {
    Ok((format!("{}\n", serde_json::to_string(value)?), baseline))
}

/// Token-budget accumulator shared by tree/context/shape: chunks are appended
/// while they fit, `finish` adds the standard truncation trailer.
pub(crate) struct BudgetOut {
    out: String,
    used: i64,
    budget: i64,
    truncated: bool,
}

impl BudgetOut {
    fn new(seed: String, budget: i64) -> Self {
        let used = db::est_tokens(seed.len());
        BudgetOut {
            out: seed,
            used,
            budget,
            truncated: false,
        }
    }
    /// Append if it fits; on overflow flips `truncated` and refuses.
    fn try_push(&mut self, chunk: &str) -> bool {
        let cost = db::est_tokens(chunk.len());
        if self.used + cost > self.budget {
            self.truncated = true;
            return false;
        }
        self.used += cost;
        self.out.push_str(chunk);
        true
    }
    /// Append regardless of budget (counted, so later chunks still compete) —
    /// for section headers/footers that must always show.
    fn push_always(&mut self, chunk: &str) {
        self.used += db::est_tokens(chunk.len());
        self.out.push_str(chunk);
    }
    fn finish(mut self, trailer: &str) -> String {
        if self.truncated {
            self.out.push_str(trailer);
        }
        self.out
    }
}

/// Append a symbol body as `── header ──` + notes + numbered lines — the one
/// renderer behind context/shape.
pub(crate) fn render_symbol_body(
    out: &mut String,
    q: &str,
    path: &str,
    s: i64,
    e: i64,
    lines: &[&str],
    notes: &[(i64, String, i64)],
) {
    // clamp start too: a stale index can point past the live file's EOF, and
    // lines[start..end] with start > end panics
    let end = (e as usize).min(lines.len());
    let start = (s as usize).saturating_sub(1).min(end);
    out.push_str(&format!("── {q}  {path}:{s}-{e} ──\n"));
    for (_, n, _) in notes {
        out.push_str(&format!("  ⚑ {n}\n"));
    }
    push_numbered_lines(out, lines, start, end);
}

/// Numbered source lines with a gutter sized to the largest line number in
/// range, not a fixed 5 — small files (the common case) get 2–4 fewer leading
/// chars per line. THE one gutter policy (render_symbol_body AND cmd_show).
pub(crate) fn push_numbered_lines(out: &mut String, lines: &[&str], start: usize, end: usize) {
    let w = end.to_string().len();
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{:>w$} {}\n", start + i + 1, line, w = w));
    }
}

/// Walk every indexed file, find semantic references to `name` and hand each
/// site (with its innermost enclosing symbol) to `visit(rel, line, enclosing,
/// kind, file_len)` — the one scanner behind context's callers and `tests`.
/// Each file's index rows are refreshed before its lines are mapped
/// (invariant 2). `preloaded` short-circuits the defining file, which the
/// caller already holds in memory and has already refreshed. `visit` returns
/// false to stop the whole scan.
/// Per-command default limits/budgets — the single source for both the clap
/// `default_value_t`s (main.rs) and the MCP dispatch fallbacks (mcp_server.rs),
/// so the two surfaces can't drift.
pub mod defaults {
    pub const TREE_BUDGET: i64 = 2000;
    pub const FIND_LIMIT: i64 = 25;
    pub const SHOW_CONTEXT: usize = 0;
    pub const REFS_LIMIT: usize = 100;
    pub const CONTEXT_BUDGET: i64 = 3000;
    pub const GREP_LIMIT: usize = 50;
    pub const CALLS_DEPTH: usize = 2;
    pub const SHAPE_BUDGET: i64 = 2000;
    pub const ENTRIES_LIMIT: usize = 40;
    pub const BLAME_LIMIT: usize = 10;
    pub const HOT_LIMIT: usize = 20;
    pub const COUPLING_LIMIT: usize = 15;
    pub const PATH_DEPTH: usize = 8;
}

pub(crate) fn scan_ref_sites(
    root: &Path,
    conn: &Connection,
    name: &str,
    preloaded: Option<(&str, &str)>,
    mut visit: impl FnMut(&str, i64, &str, &str, &str) -> bool,
) -> Result<()> {
    let mut files_stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let mut files: Vec<String> = files_stmt.query_map([], |r| r.get(0))?.flatten().collect();
    // Prefilter to files that literally contain the name (fail-open); a name
    // absent as a substring can't be a semantic ref. Always keep the preloaded
    // file — it may hold the definition/refs the caller already read.
    if let Some(candidates) = query::grep_prefilter(root, name, false) {
        files.retain(|f| candidates.contains(f) || preloaded.map(|(p, _)| p == f).unwrap_or(false));
    }
    let mut enclosing = conn.prepare(ENCLOSING_SYMBOL_SQL)?;
    'files: for rel in &files {
        let owned;
        let fsrc: &str = match preloaded {
            Some((p, s)) if p == rel => s,
            _ => match std::fs::read_to_string(root.join(rel)) {
                Ok(f) => {
                    owned = f;
                    &owned
                }
                Err(_) => continue,
            },
        };
        let ref_lns = lang::ref_lines(lang::detect_lang(rel), fsrc, name);
        if ref_lns.is_empty() {
            continue;
        }
        if preloaded.map(|(p, _)| p != rel).unwrap_or(true) {
            indexer::ensure_fresh(root, conn, rel);
        }
        for ln in ref_lns {
            let (encl, kind): (String, String) = enclosing
                .query_row(rusqlite::params![rel, ln as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap_or_default();
            if !visit(rel, ln as i64, &encl, &kind, fsrc) {
                break 'files;
            }
        }
    }
    Ok(())
}

pub(crate) fn locate_symbol(conn: &Connection, symbol: &str) -> Result<(String, i64, i64, String)> {
    locate_symbol_kind(conn, symbol, None)
}

/// Locate + freshness in one step — the ONLY correct way to get line numbers
/// you are about to read from disk (invariant 2). Reindexes the defining
/// file when stale and re-locates, so the returned range matches the live file.
pub(crate) fn locate_fresh(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    kind: Option<&str>,
) -> Result<(String, i64, i64, String)> {
    let located = locate_symbol_kind(conn, symbol, kind)?;
    if indexer::is_stale(root, conn, &located.0) {
        indexer::reindex_file(root, conn, &located.0)?;
        return locate_symbol_kind(conn, symbol, kind);
    }
    Ok(located)
}

/// Like `locate_symbol`, optionally narrowed to a kind (`--kind struct`
/// resolves the classic same-name struct/impl ambiguity). Still errors on
/// ambiguity WITHIN the narrowed pool — invariant 4 stands.
fn locate_symbol_kind(
    conn: &Connection,
    symbol: &str,
    kind: Option<&str>,
) -> Result<(String, i64, i64, String)> {
    // `path:Name` narrows to symbols in that file (exact path or `/`-guarded
    // suffix) — the escape hatch for same-named top-level symbols, and exactly
    // the shape the ambiguity listing below prints.
    let (file_filter, symbol) = match symbol.rsplit_once(':') {
        Some((f, n)) if f.contains('.') && !n.is_empty() && !n.contains('/') => {
            (Some(f.to_string()), n)
        }
        _ => (None, symbol),
    };
    let mut stmt = conn.prepare(
        "SELECT f.path, s.start_line, s.end_line, s.qualified
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE (s.qualified = ?1 OR s.name = ?1) AND (?2 IS NULL OR s.kind = ?2)
         ORDER BY CASE WHEN s.qualified = ?1 THEN 0 ELSE 1 END, length(s.qualified)",
    )?;
    let mut rows: Vec<(String, i64, i64, String)> = stmt
        .query_map(rusqlite::params![symbol, kind], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .flatten()
        .collect();
    if let Some(f) = &file_filter {
        // An exact project-relative path wins outright — otherwise a filter like
        // `src/main.rs` also suffix-matches `src/resolve-helper/src/main.rs` and
        // stays ambiguous, defeating the escape hatch (invariant 4).
        if rows.iter().any(|(p, ..)| p == f) {
            rows.retain(|(p, ..)| p == f);
        } else {
            rows.retain(|(p, ..)| p.ends_with(&format!("/{f}")));
        }
    }
    if rows.is_empty() {
        let hint = kind
            .map(|k| format!(" with kind '{k}'"))
            .unwrap_or_default();
        bail!("symbol '{symbol}'{hint} not found — try `cona find {symbol}`");
    }
    // exact qualified matches take priority; only unambiguous if exactly one
    let exact: Vec<&(String, i64, i64, String)> = rows.iter().filter(|r| r.3 == symbol).collect();
    let pool: Vec<&(String, i64, i64, String)> = if exact.is_empty() {
        rows.iter().collect()
    } else {
        exact
    };
    if pool.len() == 1 {
        return Ok(pool[0].clone());
    }
    let opts: Vec<String> = pool
        .iter()
        .take(8)
        .map(|(p, s, _, q)| format!("  {q}  {p}:{s}"))
        .collect();
    let example = pool
        .first()
        .map(|(p, _, _, q)| format!("{p}:{}", db::name_tail(q)))
        .unwrap_or_default();
    bail!(
        "ambiguous '{symbol}' ({} matches) — qualify with Parent.Name, file (`{example}`) or --kind:\n{}",
        pool.len(),
        opts.join("\n")
    )
}
