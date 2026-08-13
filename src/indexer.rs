use crate::{db, lang};
use anyhow::{bail, Result};
use ignore::WalkBuilder;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Files larger than this are skipped (minified bundles, generated code, blobs).
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// Submodule paths declared in a `.gitmodules` body, in declaration order.
///
/// Pure so it can be tested without a git tree. Deliberately a line parser
/// rather than a real INI/config reader: `.gitmodules` is git config syntax and
/// only the `path =` entries matter here, so a `submodule.<name>.path` lookup
/// would pull in a config-parsing dependency to answer a question three lines
/// of string handling already answer. Values are taken verbatim except for
/// surrounding whitespace — git does not quote or escape paths in this file.
pub fn parse_gitmodules(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|l| {
            let l = l.trim();
            // Skip comments before splitting: `# path = x` is not a declaration.
            if l.starts_with('#') || l.starts_with(';') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            (k.trim() == "path").then(|| v.trim().to_string())
        })
        .filter(|p| {
            // A submodule path must stay inside the superproject. An absolute
            // path or one climbing out with `..` would make the walk index a
            // tree outside the project root, whose files then can't be stored
            // as project-relative paths.
            !p.is_empty() && !Path::new(p).is_absolute() && !p.split('/').any(|c| c == "..")
        })
        .collect()
}

/// Registered submodule directories that exist on disk, relative to `root`.
/// Missing entries (a submodule that was never `git submodule update`d) are
/// dropped — adding a non-existent walk root would surface as a walk error.
fn submodule_dirs(root: &Path) -> Vec<String> {
    let Ok(body) = std::fs::read_to_string(root.join(".gitmodules")) else {
        return Vec::new();
    };
    parse_gitmodules(&body)
        .into_iter()
        .filter(|p| root.join(p).is_dir())
        .collect()
}

/// Directory names always pruned from the walk, regardless of git status. Keeps
/// the index (and ~/.cona) from ballooning when run in non-git trees.
const EXCLUDED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "vendor",
    "venv",
    ".venv",
    "__pycache__",
    ".git",
    ".svn",
    ".hg",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".gradle",
    ".idea",
    ".cache",
    "bower_components",
    "Pods",
    "DerivedData",
];

/// True if a directory with this name should never be descended into.
pub fn is_excluded_dir(name: &str) -> bool {
    EXCLUDED_DIRS.contains(&name)
}

pub struct IndexReport {
    pub scanned: usize,
    pub parsed: usize,
    pub removed: usize,
    pub total_files: i64,
    pub total_symbols: i64,
}

struct Candidate {
    rel: String,
    abs: PathBuf,
    mtime: i64,
    size: i64,
    lang: &'static str,
}

fn file_mtime(meta: &std::fs::Metadata) -> i64 {
    // nanosecond precision: whole-second mtimes made two same-size writes
    // within one second invisible to is_stale (stale line ranges served as
    // fresh). Existing DBs hold second values → everything reads stale once
    // and reindexes — a one-time, self-healing cost.
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

pub fn index_project(root: &Path, conn: &Connection) -> Result<IndexReport> {
    // preload the file table once instead of one query per file
    let mut existing: HashMap<String, (i64, i64, i64)> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT path, id, mtime, size FROM files")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?)))
        })?;
        for row in rows.flatten() {
            existing.insert(row.0, row.1);
        }
    }

    // phase 1: walk, collect changed/new candidates
    let mut seen: HashSet<String> = HashSet::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    // Git submodules are real project source, but `ignore` treats a nested
    // `.git` as a separate repository and skips its contents — so a submodule's
    // code is invisible to find/refs/grep from the superproject root, silently
    // (an empty result, not an error). Registered submodules are opted back in;
    // an unregistered nested repo (vendored clone, a stray checkout) stays
    // excluded, which is the reading that matches .gitmodules.
    let submodules = submodule_dirs(root);
    let mut builder = WalkBuilder::new(root);
    for sub in &submodules {
        builder.add(root.join(sub));
    }
    let walker = builder
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        // Skip files bigger than MAX_FILE_BYTES (minified bundles, generated code).
        .max_filesize(Some(MAX_FILE_BYTES))
        // Always prune heavy vendor/build/cache dirs by name — even in non-git
        // trees where .gitignore doesn't apply (prevents indexing e.g. all of
        // node_modules when run outside a repo).
        // A submodule registered at e.g. `vendor/sdk` must survive the name
        // prune that `vendor` would otherwise trigger: it was declared as
        // project source, so an EXCLUDED_DIRS name along its path can't be
        // read as "generated/vendored". Only the submodule's own path segments
        // are spared — node_modules INSIDE it still prunes.
        .filter_entry({
            let subs: Vec<PathBuf> = submodules.iter().map(|s| root.join(s)).collect();
            move |e| {
                if !e.file_type().is_some_and(|t| t.is_dir()) {
                    return true;
                }
                let p = e.path();
                if subs.iter().any(|s| s.starts_with(p) || s == p) {
                    return true;
                }
                !is_excluded_dir(e.file_name().to_str().unwrap_or(""))
            }
        })
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let abs = entry.path().to_path_buf();
        let rel = match abs.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => continue,
        };
        let Some(language) = lang::detect_lang(&rel) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        let (mtime, size) = (file_mtime(&meta), meta.len() as i64);
        seen.insert(rel.clone());
        if let Some((_, m, s)) = existing.get(&rel) {
            if *m == mtime && *s == size {
                continue;
            }
        }
        candidates.push(Candidate {
            rel,
            abs,
            mtime,
            size,
            lang: language,
        });
    }
    let scanned = seen.len();

    // phase 2: parse candidates in parallel (one tree-sitter parser per thread)
    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(candidates.len().max(1));
    // Greedy longest-processing-time bin packing: hand each file (largest
    // first) to the currently-lightest thread. Round-robin by index could pile
    // every big file onto one thread when sizes vary widely.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.size));
    let mut chunks: Vec<Vec<Candidate>> = (0..n_threads).map(|_| Vec::new()).collect();
    let mut loads: Vec<i64> = vec![0; n_threads];
    for c in candidates {
        let t = loads
            .iter()
            .enumerate()
            .min_by_key(|(_, &l)| l)
            .map(|(i, _)| i)
            .unwrap_or(0);
        loads[t] += c.size.max(1);
        chunks[t].push(c);
    }
    let mut results: Vec<(Candidate, Vec<lang::Sym>)> = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                scope.spawn(move || {
                    let mut out = Vec::new();
                    for c in chunk {
                        let Ok(src) = std::fs::read_to_string(&c.abs) else {
                            continue;
                        };
                        if let Ok(syms) = lang::extract_symbols(c.lang, &src) {
                            out.push((c, syms));
                        }
                    }
                    out
                })
            })
            .collect();
        for h in handles {
            if let Ok(v) = h.join() {
                results.extend(v);
            }
        }
    });
    let parsed = results.len();

    // phase 3: single write transaction. The guard rolls back automatically
    // if any upsert, symbol insert, or cleanup operation fails.
    // IMMEDIATE takes the write lock at BEGIN, so a concurrent writer waits out
    // busy_timeout instead of hitting the deferred-upgrade SQLITE_BUSY (which
    // SQLite returns instantly, timeout ignored) — the watch+hook combination
    // makes two simultaneous writers a normal occurrence, not an edge case.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    {
        let mut upsert = tx.prepare(
            "INSERT INTO files(path, mtime, size, lang) VALUES(?1,?2,?3,?4)
             ON CONFLICT(path) DO UPDATE SET mtime=?2, size=?3, lang=?4",
        )?;
        let mut get_id = tx.prepare("SELECT id FROM files WHERE path=?1")?;
        let mut del = tx.prepare("DELETE FROM symbols WHERE file_id=?1")?;
        let mut ins = tx.prepare(
            "INSERT INTO symbols(file_id,name,qualified,kind,parent,start_line,end_line,signature)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        )?;
        for (c, syms) in &results {
            upsert.execute(rusqlite::params![c.rel, c.mtime, c.size, c.lang])?;
            let file_id: i64 = get_id.query_row([&c.rel], |r| r.get(0))?;
            del.execute([file_id])?;
            for s in syms {
                ins.execute(rusqlite::params![
                    file_id,
                    s.name,
                    s.qualified,
                    s.kind,
                    s.parent,
                    s.start_line as i64,
                    s.end_line as i64,
                    s.signature
                ])?;
            }
        }
    }
    // remove files that disappeared
    let mut removed = 0usize;
    for (path, (id, _, _)) in &existing {
        if !seen.contains(path) {
            tx.execute("DELETE FROM symbols WHERE file_id=?1", [id])?;
            tx.execute("DELETE FROM files WHERE id=?1", [id])?;
            removed += 1;
        }
    }
    tx.commit()?;

    let total_files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
    let total_symbols: i64 = conn.query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?;
    db::register_project(root, total_files, total_symbols)?;

    Ok(IndexReport {
        scanned,
        parsed,
        removed,
        total_files,
        total_symbols,
    })
}

/// True if the file on disk differs (mtime/size) from the indexed version.
/// `cona index --watch`: block on fs events, debounce, then run the
/// normal incremental index (mtime-based, so irrelevant events cost ~ms).
/// The watcher never partially updates — every wake-up goes through
/// `index_project`, the single write path.
pub fn watch_project(root: &Path, conn: &Connection) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc;
    use std::time::Duration;

    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    eprintln!("watching {} — Ctrl-C to stop", root.display());
    loop {
        // block until something happens, then swallow the burst
        let first = match rx.recv() {
            Ok(ev) => ev,
            Err(_) => return Ok(()), // watcher gone
        };
        let mut relevant = event_is_relevant(root, &first);
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        while let Ok(ev) =
            rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            relevant |= event_is_relevant(root, &ev);
        }
        if !relevant {
            continue;
        }
        let t0 = std::time::Instant::now();
        match index_project(root, conn) {
            Ok(r) if r.parsed > 0 || r.removed > 0 => {
                eprintln!(
                    "reindexed in {}ms — {} parsed, {} removed, {} symbols",
                    t0.elapsed().as_millis(),
                    r.parsed,
                    r.removed,
                    r.total_symbols
                );
            }
            Ok(_) => {}
            Err(e) => eprintln!("watch: index error: {e}"),
        }
    }
}

/// An event matters when it touches a source file we could index (or a
/// removal), outside excluded dirs. Errs on the side of true — the
/// incremental indexer makes false wake-ups cheap.
fn event_is_relevant(root: &Path, ev: &notify::Result<notify::Event>) -> bool {
    let Ok(ev) = ev else { return true };
    ev.paths.iter().any(|p| {
        let rel = p.strip_prefix(root).unwrap_or(p);
        let excluded = rel
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .any(|seg| seg == ".git" || is_excluded_dir(seg));
        if excluded {
            return false;
        }
        match rel.to_str() {
            Some(s) => crate::lang::detect_lang(s).is_some() || !p.exists(),
            None => false,
        }
    })
}

pub fn is_stale(root: &Path, conn: &Connection, rel: &str) -> bool {
    let Ok(meta) = std::fs::metadata(root.join(rel)) else {
        return true;
    };
    let db: Option<(i64, i64)> = conn
        .query_row("SELECT mtime, size FROM files WHERE path=?1", [rel], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .ok();
    match db {
        Some((m, s)) => !meta_matches(&meta, m, s),
        None => true,
    }
}

/// True when on-disk `meta` matches the indexed `(mtime, size)` — the ONE
/// freshness comparison, shared by `is_stale` and the dashboard's batched scan.
pub fn meta_matches(meta: &std::fs::Metadata, mtime: i64, size: i64) -> bool {
    file_mtime(meta) == mtime && meta.len() as i64 == size
}

/// Refresh one file's index rows if its mtime/size changed — the single owner
/// of the "never use index line numbers blindly" invariant for file-level
/// refreshes (locate_fresh is the symbol-level sibling). Best-effort: a file
/// that vanished mid-scan is simply skipped.
pub fn ensure_fresh(root: &Path, conn: &Connection, rel: &str) -> bool {
    if is_stale(root, conn, rel) {
        // a failed reindex must not report "refreshed" — callers would then
        // trust stale line ranges as if they matched the live file
        return reindex_file(root, conn, rel).is_ok();
    }
    false
}

/// Outcome of refreshing a set of files: which ones could NOT be brought up to
/// date (read-only mode, vanished mid-scan), and whether any refresh actually
/// wrote — the second half tells a caller whether rows it already fetched are
/// now invalid and must be re-read.
pub struct Refreshed {
    pub stale: Vec<String>,
    pub any_refreshed: bool,
}

/// Refresh every path in `paths` and report what stayed stale — THE shared
/// "bring these files up to date before printing their line ranges" step
/// (invariant 2) for commands that render index-derived ranges for a whole file
/// set, rather than one symbol (`locate_fresh`) or one file (`ensure_fresh`).
///
/// Stats each path once: `ensure_fresh` re-checks staleness internally, so
/// pairing it with an outer `is_stale` guard would double the syscalls — and its
/// `false` means both "was fresh" and "refresh failed", which callers must not
/// conflate. Duplicate paths are skipped, so a path-ordered row set can be fed
/// in directly.
pub fn refresh_files<'a>(
    root: &Path,
    conn: &Connection,
    paths: impl IntoIterator<Item = &'a str>,
) -> Refreshed {
    let mut out = Refreshed {
        stale: Vec::new(),
        any_refreshed: false,
    };
    let mut seen = "";
    for path in paths {
        if path == seen {
            continue;
        }
        seen = path;
        if !is_stale(root, conn, path) {
            continue;
        }
        if reindex_file(root, conn, path).is_ok() {
            out.any_refreshed = true;
        } else {
            out.stale.push(path.to_string());
        }
    }
    out
}

/// Re-index a single file after an edit.
///
/// Read-only mode cannot refresh the index, and must not pretend otherwise: the
/// connection is opened `SQLITE_OPEN_READ_ONLY`, so the write below would fail
/// with rusqlite's opaque "attempt to write a readonly database". Refusing here
/// — at the ONE write path both `ensure_fresh` and `locate_fresh` funnel
/// through — keeps invariant 2 intact: a caller either gets line numbers that
/// match the live file, or an error naming the stale file. It never gets stale
/// ranges dressed up as fresh.
pub fn reindex_file(root: &Path, conn: &Connection, rel: &str) -> Result<usize> {
    if db::is_read_only() {
        bail!("{rel} changed since it was indexed; cannot refresh in read-only mode (run `cona index` from a writable environment)");
    }
    let abs = root.join(rel);
    let Some(language) = lang::detect_lang(rel) else {
        return Ok(0);
    };
    // stat BEFORE reading: if the file is written between the two syscalls the
    // recorded mtime is older than the content on disk, so the next is_stale
    // check self-heals. The other order records a fresh mtime against stale
    // content — permanently wrong until the next external edit.
    let meta = std::fs::metadata(&abs)?;
    let src = std::fs::read_to_string(&abs)?;
    let symbols = lang::extract_symbols(language, &src)?;
    // one transaction: a crash between the files upsert and the symbol inserts
    // must not leave a "fresh" file with missing symbols. IMMEDIATE so a
    // concurrent writer (watch vs. hook) waits instead of failing — see
    // index_project's phase-3 note.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO files(path, mtime, size, lang) VALUES(?1,?2,?3,?4)
         ON CONFLICT(path) DO UPDATE SET mtime=?2, size=?3, lang=?4",
        rusqlite::params![rel, file_mtime(&meta), meta.len() as i64, language],
    )?;
    let file_id: i64 = tx.query_row("SELECT id FROM files WHERE path=?1", [&rel], |r| r.get(0))?;
    tx.execute("DELETE FROM symbols WHERE file_id=?1", [file_id])?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO symbols(file_id,name,qualified,kind,parent,start_line,end_line,signature)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        )?;
        for s in &symbols {
            stmt.execute(rusqlite::params![
                file_id,
                s.name,
                s.qualified,
                s.kind,
                s.parent,
                s.start_line as i64,
                s.end_line as i64,
                s.signature
            ])?;
        }
    }
    tx.commit()?;
    Ok(symbols.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitmodules_paths_are_parsed_in_order() {
        let body = "\
[submodule \"vendor/sdk\"]
\tpath = vendor/sdk
\turl = https://example.com/sdk.git
[submodule \"libs/core\"]
\tpath = libs/core
\turl = ../core
";
        assert_eq!(parse_gitmodules(body), vec!["vendor/sdk", "libs/core"]);
    }

    #[test]
    fn gitmodules_ignores_comments_and_other_keys() {
        // `url`/`branch` must not be mistaken for paths, and a commented-out
        // declaration is not a submodule.
        let body =
            "# path = not/real\n; path = also/not\nurl = x\nbranch = main\npath = real/one\n";
        assert_eq!(parse_gitmodules(body), vec!["real/one"]);
    }

    #[test]
    fn gitmodules_rejects_paths_escaping_the_root() {
        // These would make the walk index a tree outside the project root,
        // whose files cannot be stored as project-relative paths.
        let body = "path = /etc\npath = ../outside\npath = a/../../b\npath =\npath = ok/here\n";
        assert_eq!(parse_gitmodules(body), vec!["ok/here"]);
    }
}
