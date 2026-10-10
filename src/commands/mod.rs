//! CLI command implementations; `main.rs` only parses and dispatches here.
//! Shared plumbing (DB open, usage logging, symbol lookup, output budgeting)
//! lives in this module root.

pub mod callgraph;
mod discover;
pub mod history;
pub mod insight;
mod learn;
pub mod mcp_server;
pub mod mutate;
pub mod query;
pub mod stats;

pub use callgraph::*;
pub use discover::cmd_discover;
pub use history::*;
pub use insight::*;
pub use learn::{cmd_learn, learned_hints, suggest_fixes};
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
/// "which symbol is this line in" (context/grep/tests). Columns: qualified, kind.
pub(crate) const ENCLOSING_SYMBOL_SQL: &str =
    "SELECT s.qualified, s.kind FROM symbols s JOIN files f ON f.id = s.file_id
     WHERE f.path = ?1 AND s.start_line <= ?2 AND s.end_line >= ?2
     ORDER BY s.start_line DESC LIMIT 1";

/// Shared remedy for both read-only dead ends (missing index / empty index).
const READ_ONLY_INDEX_HINT: &str = "in read-only mode — run `cona index` from a \
     writable environment first (if one exists elsewhere, point CONA_DATA_DIR \
     at the directory that holds it)";

pub fn open_indexed(root: &Path) -> Result<Connection> {
    let conn = if db::is_read_only() {
        db::open_existing_project_db(root).map_err(|_| {
            anyhow!(
                "no existing index for {} {READ_ONLY_INDEX_HINT}",
                root.display()
            )
        })?
    } else {
        db::open_project_db(root)?
    };
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
    if n == 0 {
        if db::is_read_only() {
            bail!(
                "the index for {} is empty {READ_ONLY_INDEX_HINT}",
                root.display()
            );
        }
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
    if db::is_read_only() {
        return;
    }
    let ms = t0.elapsed().as_millis() as i64;
    let tokens_out = db::est_tokens(out.len());
    // Baseline = reading only the files the results live in, so a query can
    // never claim to have "saved" more than those files cost.
    let saved = (baseline_tokens - tokens_out).max(0);
    let results = out.lines().count() as i64;
    let outcome = db::outcome_of_output(out);
    db::log_usage_outcome(root, cmd, ms, results, tokens_out, saved, detail, outcome);
}

/// Log a query that errored (unknown symbol, ambiguity, bad args). Without
/// it a failed lookup leaves no trace and `cona learn` has nothing to mine.
pub fn finish_err(root: &Path, cmd: &str, t0: Instant, detail: &str, err: &anyhow::Error) {
    if db::is_read_only() {
        return;
    }
    let ms = t0.elapsed().as_millis() as i64;
    let outcome = db::outcome_of_error(&err.to_string());
    db::log_usage_outcome(root, cmd, ms, 0, 0, 0, detail, outcome);
}

/// Shared `--json` return shape: one JSON line + the savings baseline.
pub(crate) fn jout<T: serde::Serialize>(value: &T, baseline: i64) -> Result<(String, i64)> {
    Ok((format!("{}\n", serde_json::to_string(value)?), baseline))
}

/// Trailer for a list clipped by `--limit` — ONE string so every clipped list
/// names the same escape hatch (budget clipping: [`BudgetOut::finish`]).
pub(crate) const LIMIT_TRAILER: &str = "… truncated (raise --limit)\n";

/// Clip `v` to `limit`; on `true` callers append [`LIMIT_TRAILER`], since
/// exactly `limit` rows can't otherwise be told apart from "there was more".
pub(crate) fn clip<T>(v: &mut Vec<T>, limit: usize) -> bool {
    let clipped = v.len() > limit;
    v.truncate(limit);
    clipped
}

/// Token-budget accumulator for tree/context/shape: appends chunks while they
/// fit; `finish` adds the truncation trailer.
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
    /// Append regardless of budget (still counted) — for headers/footers that
    /// must always show.
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
    // clamp start too: a stale index can point past EOF, and start > end panics
    let end = (e as usize).min(lines.len());
    let start = (s as usize).saturating_sub(1).min(end);
    out.push_str(&format!("── {q}  {path}:{s}-{e} ──\n"));
    for (_, n, _) in notes {
        out.push_str(&format!("  ⚑ {n}\n"));
    }
    push_numbered_lines(out, lines, start, end);
}

/// Numbered source lines, gutter sized to the largest line number in range
/// (not a fixed 5, saving chars on small files). THE one gutter policy
/// (render_symbol_body AND cmd_show).
pub(crate) fn push_numbered_lines(out: &mut String, lines: &[&str], start: usize, end: usize) {
    let w = end.to_string().len();
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{:>w$} {}\n", start + i + 1, line, w = w));
    }
}

/// Per-command default limits/budgets — the single source for the clap
/// `default_value_t`s AND the MCP dispatch fallbacks, so they can't drift.
pub mod defaults {
    pub const TREE_BUDGET: i64 = 2000;
    pub const FIND_LIMIT: usize = 25;
    pub const SHOW_CONTEXT: usize = 0;
    pub const REFS_LIMIT: usize = 100;
    pub const CONTEXT_BUDGET: i64 = 3000;
    pub const GREP_LIMIT: usize = 50;
    /// Hits shown from the rest of the repo when a `--path` grep finds none.
    pub const GREP_ELSEWHERE: usize = 3;
    pub const CALLS_DEPTH: usize = 2;
    pub const SHAPE_BUDGET: i64 = 2000;
    pub const ENTRIES_LIMIT: usize = 40;
    pub const BLAME_LIMIT: usize = 10;
    pub const HOT_LIMIT: usize = 20;
    pub const COUPLING_LIMIT: usize = 15;
    pub const PATH_DEPTH: usize = 8;
    /// `show` auto-expands an ambiguous name when the pool has at most this
    /// many candidates …
    pub const AUTO_ALL_MAX_CANDIDATES: usize = 3;
    /// … and their bodies sum to at most this many lines; past either, the
    /// guided ambiguity error is cheaper.
    pub const AUTO_ALL_MAX_LINES: i64 = 400;
}

/// How `show` renders a resolved symbol.
///
/// One value through every `show` path (CLI, MCP, `cmd_show` → `show_one`), so
/// a new knob is added here, not threaded through three signatures.
/// `disclose_others` is deliberately NOT here — `cmd_show` sets it per call.
#[derive(Clone, Copy)]
pub struct ShowOpts<'a> {
    /// Extra lines above and below the symbol body.
    pub context: usize,
    /// Narrow to a kind (`fn`, `struct`, …) — the same-name escape hatch.
    pub kind: Option<&'a str>,
    /// Signature only, no body read.
    pub sig: bool,
    /// On an ambiguous name, render every candidate instead of erroring.
    pub all: bool,
}

/// How `grep` matches lines — read by both `Matcher` and `grep_prefilter`, so
/// they never get different readings of one pattern (a disagreeing prefilter
/// silently drops files holding real matches).
#[derive(Clone, Copy)]
pub struct GrepOpts<'a> {
    /// Case-insensitive match.
    pub ignore_case: bool,
    /// Treat the pattern as a Rust regex instead of a literal.
    pub regex: bool,
    /// Max hits reported.
    pub limit: usize,
    /// Restrict the search to this path prefix or directory.
    pub path: Option<&'a str>,
    /// Also search dependency dirs (`node_modules`, `vendor`, `target`, …).
    /// Off by default: they aren't the agent's code and bury repo hits.
    pub include_deps: bool,
    /// Context lines before / after each hit (`-B`/`-A`; `-C` sets both).
    /// 0 = one line per hit.
    pub before: usize,
    pub after: usize,
    /// `-l`/`-c`: one line per matching file (with its hit count under `count`)
    /// instead of one per hit.
    pub files_only: bool,
    pub count: bool,
}

/// THE `--path` policy for every query command (tree/find/refs/grep/…).
///
/// A filter matches the file itself, a parent directory, or a literal prefix.
/// Directory and prefix readings conflict: `--path src/commands` must EXCLUDE
/// `src/commands_old.rs`, while `--path src/comm` must INCLUDE
/// `src/commands/query.rs`. No string rule separates them, so the caller says
/// via `dir_filter` whether the filter is a real directory: if so, only the
/// `/`-boundary reading applies; otherwise the prefix reading does.
pub(crate) fn path_matches_dir(rel: &str, filter: &str, dir_filter: bool) -> bool {
    let f = filter.trim_end_matches('/');
    if rel == f {
        return true;
    }
    // `/`-boundary reading: `rel` sits under the directory `f`.
    if rel
        .strip_prefix(f)
        .is_some_and(|rest| rest.starts_with('/'))
    {
        return true;
    }
    // A trailing slash or a real directory means directory-only.
    if dir_filter || filter.ends_with('/') {
        return false;
    }
    rel.starts_with(filter)
}

/// A `--path` filter with the directory question answered ONCE.
///
/// The dir-vs-prefix reading is loop-invariant, but the filter runs per row
/// (up to thousands); resolving it up front keeps the hot path a pure string
/// compare. A struct, not a closure, so it doesn't pin a caller's scope alive.
pub(crate) struct PathFilter<'a> {
    filter: Option<&'a str>,
    dir_filter: bool,
}

impl<'a> PathFilter<'a> {
    /// Resolve `filter` against the filesystem — THE one stat.
    pub(crate) fn new(root: &Path, filter: Option<&'a str>) -> Self {
        let dir_filter = filter.is_some_and(|f| root.join(f.trim_end_matches('/')).is_dir());
        PathFilter { filter, dir_filter }
    }
    /// Does this repo-relative path pass? A `None` filter matches everything.
    pub(crate) fn ok(&self, rel: &str) -> bool {
        self.filter
            .is_none_or(|f| path_matches_dir(rel, f, self.dir_filter))
    }
    /// True when a filter is set at all (scoped-vs-global messaging).
    pub(crate) fn is_scoped(&self) -> bool {
        self.filter.is_some()
    }
    /// The raw filter, for error messages that quote what the user typed.
    pub(crate) fn as_str(&self) -> &'a str {
        self.filter.unwrap_or("")
    }
    /// Search root for rg/grep, so a scoped query WALKS less, not just reports
    /// less. Only a real directory — a partial name is not a path.
    pub(crate) fn search_root(&self) -> Option<&'a str> {
        self.filter
            .filter(|_| self.dir_filter)
            .map(|f| f.trim_end_matches('/'))
    }
}

/// Walk every indexed file, find semantic references to `name` and hand each
/// site (with its innermost enclosing symbol) to `visit(rel, line, enclosing,
/// kind, file_src)` — the one scanner behind context's callers and `tests`.
/// Each file is refreshed before its lines are mapped (invariant 2).
/// `preloaded` is the defining file, already in memory and refreshed. `visit`
/// returns false to stop the scan.
pub(crate) fn scan_ref_sites(
    root: &Path,
    conn: &Connection,
    name: &str,
    preloaded: Option<(&str, &str)>,
    path_filter: Option<&str>,
    mut visit: impl FnMut(&str, i64, &str, &str, &str) -> bool,
) -> Result<()> {
    let pf = PathFilter::new(root, path_filter);
    let mut files_stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let mut files: Vec<String> = files_stmt.query_map([], |r| r.get(0))?.flatten().collect();
    // Prefilter to files literally containing the name (fail-open), always
    // keeping the preloaded file. A directory scope narrows the walk itself;
    // the scope filter below would drop an out-of-scope preloaded file anyway.
    let matcher = query::Matcher::literal(name);
    // include_deps=false: a hit inside a dependency has no symbol row.
    if let Some(candidates) =
        query::grep_prefilter(root, name, &matcher, false, pf.search_root(), false)
    {
        files.retain(|f| candidates.contains(f) || preloaded.map(|(p, _)| p == f).unwrap_or(false));
    }
    // `--path` applies AFTER the prefilter and overrides the preloaded
    // exemption: an explicit scope excludes even the defining file.
    files.retain(|f| pf.ok(f));
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

/// A located symbol: `(path, start_line, end_line, qualified)` — the ONE shape
/// every resolver here returns.
pub(crate) type Located = (String, i64, i64, String);

pub(crate) fn locate_symbol(conn: &Connection, symbol: &str) -> Result<Located> {
    locate_symbol_kind(conn, symbol, None)
}

/// Locate + freshness in one step — the ONLY correct way to get line numbers
/// you are about to read from disk (invariant 2): reindexes a stale defining
/// file and re-locates.
pub(crate) fn locate_fresh(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    kind: Option<&str>,
) -> Result<Located> {
    let located = match locate_symbol_kind(conn, symbol, kind) {
        Ok(l) => l,
        // a deleted file can still hold a candidate until the next full index;
        // drop those and retry before calling it ambiguous
        Err(_) if prune_vanished(root, conn, symbol, kind) => {
            locate_symbol_kind(conn, symbol, kind)?
        }
        Err(e) => return Err(e),
    };
    if indexer::is_stale(root, conn, &located.0) {
        indexer::reindex_file(root, conn, &located.0)?;
        return locate_symbol_kind(conn, symbol, kind);
    }
    Ok(located)
}

/// `locate_fresh` for WRITE paths (edit/insert): always reindex the defining
/// file, no staleness heuristic. A write splices by these lines, so "probably
/// fresh" is not enough — one reindex is cheap next to a mis-spliced edit.
pub(crate) fn locate_for_write(root: &Path, conn: &Connection, symbol: &str) -> Result<Located> {
    let (path0, ..) = locate_symbol(conn, symbol)?;
    indexer::reindex_file(root, conn, &path0)?;
    locate_symbol(conn, symbol)
}

/// Forget indexed files among `symbol`'s candidates that no longer exist on
/// disk. True when anything was dropped (the caller re-resolves).
pub(crate) fn prune_vanished(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    kind: Option<&str>,
) -> bool {
    let Ok(cands) = locate_candidates(conn, symbol, kind) else {
        return false;
    };
    let mut gone: Vec<&str> = cands
        .iter()
        .map(|(p, ..)| p.as_str())
        .filter(|p| !root.join(p).exists())
        .collect();
    gone.dedup();
    for p in &gone {
        let _ = conn.execute(
            "DELETE FROM symbols WHERE file_id IN (SELECT id FROM files WHERE path = ?1)",
            [p],
        );
        let _ = conn.execute("DELETE FROM files WHERE path = ?1", [p]);
    }
    !gone.is_empty()
}

/// Every candidate for `symbol`, in ambiguity-error order. Does NOT pick a
/// winner — `show --all` renders them all, so invariant 4 holds: the ambiguity
/// is answered in full instead of costing a round-trip.
pub(crate) fn locate_all(
    conn: &Connection,
    symbol: &str,
    kind: Option<&str>,
) -> Result<Vec<Located>> {
    match locate_symbol_kind(conn, symbol, kind) {
        Ok(one) => Ok(vec![one]),
        Err(e) => {
            let cands = locate_candidates(conn, symbol, kind)?;
            if cands.len() > 1 {
                Ok(cands)
            } else {
                Err(e) // genuinely not found — keep the original message
            }
        }
    }
}

/// The candidate pool for `symbol` in priority order (exact-qualified first),
/// after `path:Name` and `--kind` narrowing. Shared by the single-result
/// resolver and `locate_all` so they can't disagree about the candidates.
fn locate_candidates(conn: &Connection, symbol: &str, kind: Option<&str>) -> Result<Vec<Located>> {
    let (rows, symbol) = locate_rows(conn, symbol, kind)?;
    let exact: Vec<Located> = rows.iter().filter(|r| r.3 == symbol).cloned().collect();
    let mut pool = if exact.is_empty() { rows } else { exact };
    // Ambiguity stays an error (inv. 4), but real code lists before test
    // helpers: the wanted definition is rarely the 10th `run` in a test file,
    // and the ambiguity error's `file:Name` example then names it.
    pool.sort_by_key(|(p, ..)| crate::entries::is_test_path(p));
    Ok(pool)
}

/// THE `file.rs:Name` locator grammar: `Some((path, name))` for a
/// path-qualified symbol, `None` for a plain one. Shared so the resolver and
/// `show`'s path sniffing can't disagree.
pub(crate) fn split_locator(arg: &str) -> Option<(&str, &str)> {
    match arg.rsplit_once(':') {
        Some((f, n))
            if (f.contains('.') || f.contains('/')) && !n.is_empty() && !n.contains('/') =>
        {
            Some((f, n))
        }
        _ => None,
    }
}

/// `show --path P Name` → `P:Name`, so a scope reaches the one resolver, not a
/// parallel filter. An existing locator wins (it is the narrower address).
pub fn scoped_locator(path: Option<&str>, symbol: &str) -> String {
    match path
        .map(|p| p.trim_end_matches('/'))
        .filter(|p| !p.is_empty())
    {
        Some(p) if split_locator(symbol).is_none() => format!("{p}:{symbol}"),
        _ => symbol.to_string(),
    }
}

/// Raw candidate rows plus the bare name after stripping a `path:` prefix.
fn locate_rows(
    conn: &Connection,
    symbol: &str,
    kind: Option<&str>,
) -> Result<(Vec<Located>, String)> {
    // `path:Name` narrows to that file (exact path or `/`-guarded suffix) —
    // exactly the shape the ambiguity listing prints.
    let (file_filter, symbol) = match split_locator(symbol) {
        Some((f, n)) => (Some(f.to_string()), n),
        None => (None, symbol),
    };
    let mut stmt = conn.prepare(
        "SELECT f.path, s.start_line, s.end_line, s.qualified
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE (s.qualified = ?1 OR s.name = ?1) AND (?2 IS NULL OR s.kind = ?2)
         ORDER BY CASE WHEN s.qualified = ?1 THEN 0 ELSE 1 END, length(s.qualified)",
    )?;
    let mut rows: Vec<Located> = stmt
        .query_map(rusqlite::params![symbol, kind], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .flatten()
        .collect();
    let unscoped = if file_filter.is_some() {
        rows.clone()
    } else {
        Vec::new()
    };
    if let Some(f) = &file_filter {
        // An exact path wins outright — else `src/main.rs` also suffix-matches
        // `src/resolve-helper/src/main.rs`, defeating the escape hatch (inv. 4).
        if rows.iter().any(|(p, ..)| p == f) {
            rows.retain(|(p, ..)| p == f);
        } else {
            // suffix (`views/Plan.tsx:Name`) or directory scope (`web/src:Name`)
            rows.retain(|(p, ..)| p.ends_with(&format!("/{f}")) || path_matches_dir(p, f, true));
        }
    }
    if rows.is_empty() {
        let hint = kind
            .map(|k| format!(" with kind '{k}'"))
            .unwrap_or_default();
        if let (Some(f), false) = (&file_filter, unscoped.is_empty()) {
            // `file:Name` for a name the file only imports or uses: say where
            // it IS defined rather than send the agent to `find` for it.
            let at: Vec<String> = unscoped
                .iter()
                .take(8)
                .map(|(p, s, _, q)| format!("  {q}  {p}:{s}"))
                .collect();
            bail!(
                "'{symbol}'{hint} is not defined in '{f}' — defined at:\n{}",
                at.join("\n")
            );
        }
        bail!("symbol '{symbol}'{hint} not found — try `cona find {symbol}`");
    }
    Ok((rows, symbol.to_string()))
}

/// Like `locate_symbol`, optionally narrowed to a kind (`--kind struct` splits
/// the classic struct/impl pair). Still errors on ambiguity WITHIN the narrowed
/// pool — invariant 4 stands.
fn locate_symbol_kind(conn: &Connection, symbol: &str, kind: Option<&str>) -> Result<Located> {
    let pool = locate_candidates(conn, symbol, kind)?;
    let symbol = symbol.rsplit_once(':').map_or(symbol, |(_, n)| n);
    if pool.len() == 1 {
        return Ok(pool[0].clone());
    }
    let opts: Vec<String> = pool
        .iter()
        .take(8)
        .map(|(p, s, _, q)| format!("  {q}  {p}:{s}"))
        .collect();
    // Suggest only hatches that can separate THIS pool: a same-file enum + impl
    // pair shares path and qualified name, so `file:Name`/`Parent.Name` would
    // be dead ends before `--kind`.
    let mut hatches = Vec::new();
    if pool.windows(2).any(|w| w[0].3 != w[1].3) {
        hatches.push("Parent.Name".to_string());
    }
    if pool.windows(2).any(|w| w[0].0 != w[1].0) {
        let example = pool
            .first()
            .map(|(p, _, _, q)| format!("{p}:{}", db::name_tail(q)))
            .unwrap_or_default();
        hatches.push(format!("file (`{example}`)"));
    }
    hatches.push("--kind".to_string());
    bail!(
        "ambiguous '{symbol}' ({} matches) — qualify with {}, or show them all with --all:\n{}",
        pool.len(),
        hatches.join(", "),
        opts.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_locator_prefixes_unless_already_located() {
        assert_eq!(scoped_locator(Some("web/src/"), "day"), "web/src:day");
        assert_eq!(scoped_locator(Some("a.ts"), "x.ts:day"), "x.ts:day");
        assert_eq!(scoped_locator(None, "day"), "day");
        // a directory locator is a locator too
        assert_eq!(split_locator("web/src:day"), Some(("web/src", "day")));
        assert_eq!(split_locator("Parent.Name"), None);
    }

    #[test]
    fn path_filter_matches_file_dir_and_prefix() {
        // exact file
        assert!(path_matches_dir("src/db.rs", "src/db.rs", false));
        // real directory, with and without a trailing slash
        assert!(path_matches_dir(
            "src/commands/query.rs",
            "src/commands",
            true
        ));
        assert!(path_matches_dir(
            "src/commands/query.rs",
            "src/commands/",
            true
        ));
        // the directory itself
        assert!(path_matches_dir("src/commands", "src/commands", true));
        // a half-typed path is not a directory → prefix reading applies
        assert!(path_matches_dir("src/commands/query.rs", "src/comm", false));
        assert!(path_matches_dir("src/db.rs", "src/d", false));
    }

    #[test]
    fn path_filter_respects_directory_boundary() {
        // THE bug this policy prevents: a real-directory filter must not leak
        // into a same-prefixed sibling. Only this no-slash form pins the
        // `dir_filter` rule (the trailing-slash form is rejected lexically).
        assert!(!path_matches_dir(
            "src/commands_old.rs",
            "src/commands",
            true
        ));
        assert!(!path_matches_dir(
            "src/commands_old.rs",
            "src/commands/",
            false
        ));
        assert!(!path_matches_dir(
            "src/other/query.rs",
            "src/commands",
            true
        ));
        // …while as a partial name it matches — hence `dir_filter`.
        assert!(path_matches_dir(
            "src/commands_old.rs",
            "src/commands",
            false
        ));
    }

    #[test]
    fn path_filter_none_matches_everything() {
        let root = Path::new("/nonexistent-cona-test-root");
        assert!(PathFilter::new(root, None).ok("anything/at/all.rs"));
        assert!(!PathFilter::new(root, Some("tests")).ok("src/db.rs"));
    }

    #[test]
    fn ambiguous_candidates_list_real_code_before_tests() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE symbols(file_id INTEGER, kind TEXT, name TEXT,
                 qualified TEXT, start_line INTEGER, end_line INTEGER);
             INSERT INTO files VALUES (1, 'a_test.ts'), (2, 'm.ts');
             INSERT INTO symbols VALUES (1, 'fn', 'step', 'step', 1, 1),
                                        (2, 'fn', 'step', 'step', 1, 1);",
        )
        .unwrap();
        let paths: Vec<String> = locate_all(&conn, "step", None)
            .unwrap()
            .into_iter()
            .map(|(p, ..)| p)
            .collect();
        assert_eq!(paths, ["m.ts", "a_test.ts"]);
        let err = locate_symbol(&conn, "step").unwrap_err().to_string();
        assert!(err.contains("file (`m.ts:step`)"), "{err}");
    }
}
