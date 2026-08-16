use anyhow::{anyhow, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

static READ_ONLY: AtomicBool = AtomicBool::new(false);
static EPHEMERAL_DATA_DIR_NOTICE: Once = Once::new();

/// Enable the process-wide inspection-only mode used by the CLI's
/// `--read-only` flag. This deliberately applies to the database layer too,
/// so a future command cannot accidentally write telemetry or schema changes.
pub fn set_read_only(enabled: bool) {
    READ_ONLY.store(enabled, Ordering::Relaxed);
}

pub fn is_read_only() -> bool {
    READ_ONLY.load(Ordering::Relaxed)
}

/// Find the project root: nearest ancestor containing .git, else cwd.
/// Nearest ancestor of `start` (inclusive) containing a `.git`, else `start`
/// itself. The single source of truth for "which project does this path belong
/// to" — the hook and `project_root` both resolve through here so a file's root
/// and the indexed root can never diverge on the walk.
pub fn git_root_from(start: &Path) -> PathBuf {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return start.to_path_buf(),
        }
    }
}

pub fn project_root() -> Result<PathBuf> {
    Ok(git_root_from(&std::env::current_dir()?))
}

/// Resolved once per process. `data_dir` is on the hook hot path and reached
/// several times per command (global db, project db, `project_db_path`), and
/// resolving it writes: `ensure_writable_data_dir` does mkdir + create + unlink.
/// Repeating that probe per call turns the documented "single stat, no DB open"
/// hook check into a dozen syscalls. Caching is sound because neither input
/// changes mid-process: `CONA_DATA_DIR` is read from the environment, and
/// `--read-only` is set before any storage access.
static DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn data_dir() -> Result<PathBuf> {
    if let Some(d) = DATA_DIR.get() {
        return Ok(d.clone());
    }
    let d = resolve_data_dir()?;
    // A racing thread may win; either value is equivalent, so keep the stored one.
    Ok(DATA_DIR.get_or_init(|| d).clone())
}

/// The automatic sandbox fallback location — THE one definition. A read-only
/// lookup that disagrees with where the fallback index was written reports "no
/// existing index", indistinguishable from an unindexed repo, so the path must
/// not be spelled out per call site.
pub fn ephemeral_data_dir() -> PathBuf {
    std::env::temp_dir().join("cona")
}

fn resolve_data_dir() -> Result<PathBuf> {
    // CONA_DATA_DIR is explicit: never silently redirect a user's chosen
    // storage location. The fallback below is only for the default ~/.cona,
    // which agent sandboxes commonly make read-only.
    if let Some(v) = std::env::var_os("CONA_DATA_DIR").filter(|v| !v.is_empty()) {
        let d = PathBuf::from(v);
        if !is_read_only() {
            ensure_writable_data_dir(&d)?;
        }
        return Ok(d);
    }

    let preferred = dirs::home_dir()
        .ok_or_else(|| anyhow!("no home dir; set CONA_DATA_DIR to a writable directory"))?
        .join(".cona");
    if is_read_only() {
        // In read-only mode never probe by creating directories. A readable
        // global database is the durable-storage marker: merely seeing the
        // ~/.cona directory is not enough, because sandboxes often expose the
        // directory but deny database access inside it.
        return Ok(
            if std::fs::File::open(preferred.join("global.db")).is_ok() {
                preferred
            } else {
                ephemeral_data_dir()
            },
        );
    }

    match ensure_writable_data_dir(&preferred) {
        Ok(()) => Ok(preferred),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            let fallback = ephemeral_data_dir();
            ensure_writable_data_dir(&fallback)?;
            EPHEMERAL_DATA_DIR_NOTICE.call_once(|| {
                    eprintln!(
                        "warning: default cona storage is unavailable; using ephemeral {} (set CONA_DATA_DIR for persistent indexes)",
                        fallback.display()
                    );
            });
            Ok(fallback)
        }
        Err(err) => Err(err.into()),
    }
}

/// Creating a directory alone is not enough to establish that SQLite can put
/// a database there: a sandbox may expose an existing `~/.cona` directory but
/// deny new files inside it. A short-lived, PID-scoped probe catches that case
/// before a navigation command reaches rusqlite's opaque open error.
fn ensure_writable_data_dir(d: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(d.join("projects"))?;
    let probe = d.join(format!(".write-probe-{}", std::process::id()));
    let _file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    drop(_file);
    let _ = std::fs::remove_file(probe);
    Ok(())
}

/// True for paths under the OS temp roots — safe to auto-purge from the
/// registry once they vanish (test repos, agent scratchpads). Deliberately
/// narrow: a project on an unplugged volume must never match.
pub fn is_ephemeral_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let mut roots = vec![
        "/tmp/".to_string(),
        "/private/tmp/".to_string(),
        "/var/folders/".to_string(),
        "/private/var/folders/".to_string(),
    ];
    let t = std::env::temp_dir();
    let sep = std::path::MAIN_SEPARATOR;
    let ts = format!("{}{sep}", t.to_string_lossy().trim_end_matches(sep));
    if let Some(bare) = ts.strip_prefix("/private") {
        roots.push(bare.to_string());
    }
    roots.push(ts);
    roots.iter().any(|r| s.starts_with(r.as_str()))
}

/// Stable FNV-1a hash — must not change across Rust versions,
/// otherwise existing project databases would be orphaned.
pub fn project_hash(root: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in root.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Open (and init) the per-project index database.
pub fn open_project_db(root: &Path) -> Result<Connection> {
    let db_path = data_dir()?
        .join("projects")
        .join(format!("{}.db", project_hash(root)));
    if is_read_only() {
        return Ok(Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?);
    }
    let conn = Connection::open(db_path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;
         CREATE TABLE IF NOT EXISTS files(
            id INTEGER PRIMARY KEY,
            path TEXT UNIQUE NOT NULL,
            mtime INTEGER NOT NULL,
            size INTEGER NOT NULL,
            lang TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS symbols(
            id INTEGER PRIMARY KEY,
            file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            qualified TEXT NOT NULL,
            kind TEXT NOT NULL,
            parent TEXT,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            signature TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_sym_name ON symbols(name);
         CREATE INDEX IF NOT EXISTS idx_sym_qual ON symbols(qualified);
         CREATE INDEX IF NOT EXISTS idx_sym_file ON symbols(file_id);
         CREATE TABLE IF NOT EXISTS notes(
            id INTEGER PRIMARY KEY,
            symbol TEXT NOT NULL,
            note TEXT NOT NULL,
            ts INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_notes_symbol ON notes(symbol);",
    )?;
    Ok(conn)
}

/// Open an already-built project index without creating files or running
/// migrations. Prefer durable storage, but also look in the automatic
/// sandbox fallback so `cona --read-only` can inspect an index created earlier
/// in the same sandbox.
pub fn open_existing_project_db(root: &Path) -> Result<Connection> {
    let rel = PathBuf::from("projects").join(format!("{}.db", project_hash(root)));
    [project_db_path(root), ephemeral_data_dir().join(rel)]
        .into_iter()
        .find_map(|path| {
            let conn =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .ok()?;
            // A file can exist and still be unusable (truncated, wrong schema);
            // probing `files` is what distinguishes a real index.
            conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))
                .ok()?;
            Some(conn)
        })
        .ok_or_else(|| anyhow!("no existing project index"))
}

/// Open (and init) the global database (project registry + usage stats).
pub fn open_global_db() -> Result<Connection> {
    let path = data_dir()?.join("global.db");
    if is_read_only() {
        return Ok(Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?);
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;
         CREATE TABLE IF NOT EXISTS projects(
            hash TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            last_indexed INTEGER,
            files INTEGER DEFAULT 0,
            symbols INTEGER DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS meta(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS usage(
            id INTEGER PRIMARY KEY,
            ts INTEGER NOT NULL,
            project TEXT NOT NULL,
            cmd TEXT NOT NULL,
            ms INTEGER NOT NULL,
            results INTEGER NOT NULL,
            tokens_out INTEGER NOT NULL,
            tokens_saved INTEGER NOT NULL
         );",
    )?;
    // migration: `detail` records the query target (symbol/file/pattern) so
    // stats can surface top symbols/files. Additive + nullable — safe on old DBs.
    if !column_exists(&conn, "usage", "detail")? {
        conn.execute_batch("ALTER TABLE usage ADD COLUMN detail TEXT NOT NULL DEFAULT ''")?;
    }
    Ok(conn)
}

/// Does `table` already have a column named `col`?
fn column_exists(conn: &Connection, table: &str, col: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let found = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .flatten()
        .any(|name| name == col);
    Ok(found)
}

pub fn meta_set(key: &str, value: &str) -> Result<()> {
    let g = open_global_db()?;
    g.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key, value],
    )?;
    Ok(())
}

pub fn meta_del(key: &str) -> Result<()> {
    let g = open_global_db()?;
    g.execute("DELETE FROM meta WHERE key = ?1", [key])?;
    Ok(())
}

pub fn meta_get(key: &str) -> Result<Option<String>> {
    let g = open_global_db()?;
    let v = g
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
    Ok(v)
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Every project root the indexer has registered, sorted. Fail-open: an
/// unreadable global DB yields an empty list rather than an error.
pub fn registered_project_paths() -> Vec<String> {
    let Ok(g) = open_global_db() else {
        return Vec::new();
    };
    g.prepare("SELECT path FROM projects ORDER BY path")
        .and_then(|mut s| {
            s.query_map([], |r| r.get(0))
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default()
}

pub fn register_project(root: &Path, files: i64, symbols: i64) -> Result<()> {
    let g = open_global_db()?;
    g.execute(
        "INSERT INTO projects(hash, path, last_indexed, files, symbols)
         VALUES(?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(hash) DO UPDATE SET
            path=excluded.path, last_indexed=excluded.last_indexed,
            files=excluded.files, symbols=excluded.symbols",
        rusqlite::params![
            project_hash(root),
            root.to_string_lossy(),
            now(),
            files,
            symbols
        ],
    )?;
    Ok(())
}

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
    if let Ok(g) = open_global_db() {
        let _ = g.execute(
            "INSERT INTO usage(ts, project, cmd, ms, results, tokens_out, tokens_saved, detail)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                now(),
                root.to_string_lossy(),
                cmd,
                ms,
                results,
                tokens_out,
                tokens_saved.max(0),
                detail
            ],
        );
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

// ---------------------------------------------------------------------------
// stats aggregation (shared by `cona stats` and `cona ui`)
// ---------------------------------------------------------------------------

/// Whether `root` already has a populated index. Never creates a DB —
/// safe to call from hooks on arbitrary directories.
///
/// Routes through the same read-only-aware open as `open_indexed`: gating on
/// `project_db_path` alone would answer "not indexed" for an index sitting in
/// the sandbox fallback, which `open_indexed` finds and would then happily
/// query — two callers disagreeing about whether the same repo is indexed.
pub fn has_index(root: &Path) -> bool {
    let conn = if is_read_only() {
        open_existing_project_db(root)
    } else if project_db_path(root).exists() {
        open_project_db(root)
    } else {
        return false;
    };
    conn.and_then(|c| {
        Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM files)", [], |r| {
            r.get::<_, bool>(0)
        })?)
    })
    .unwrap_or(false)
}

/// Path to a project's index database.
pub fn project_db_path(root: &Path) -> PathBuf {
    data_dir()
        .map(|d| {
            d.join("projects")
                .join(format!("{}.db", project_hash(root)))
        })
        .unwrap_or_default()
}

/// Guard for "one walk of this project at a time across processes".
///
/// Several agent sessions opening at once each fire the SessionStart hook, and
/// every one of them used to start its own full walk of the same tree — N times
/// the CPU and the peak memory for one shared result. The lock is an
/// exclusively-created marker file next to the project DB; the loser skips its
/// walk rather than waiting, because the winner is producing exactly the index
/// it wanted and a few seconds of staleness is invisible (`locate_fresh`
/// re-checks per query anyway).
///
/// Held for the lifetime of the returned guard, which unlinks on drop. A marker
/// left behind by a killed process would block every later walk, so one older
/// than `IndexLock::STALE_SECS` is reclaimed.
pub struct IndexLock(PathBuf);

impl IndexLock {
    /// Age past which a marker is assumed orphaned. Comfortably longer than any
    /// real walk (a huge tree indexes in seconds), short enough that a crashed
    /// process doesn't wedge indexing for a whole session.
    const STALE_SECS: u64 = 300;

    /// Whether a marker of this age is orphaned. `None` = the age is unknown
    /// (unreadable mtime, or a clock that moved backwards) and counts as stale:
    /// one redundant walk beats indexing wedged until the file is removed.
    fn marker_is_stale(age: Option<std::time::Duration>) -> bool {
        age.is_none_or(|a| a.as_secs() > Self::STALE_SECS)
    }

    /// `Some(guard)` if this process may index `root`; `None` if another one is
    /// already doing it. Any filesystem trouble yields a guard — failing open
    /// keeps indexing working, which matters more than the deduplication.
    pub fn acquire(root: &Path) -> Option<Self> {
        Self::at(&project_db_path(root).with_extension("indexing"))
    }

    /// `acquire` against an explicit marker path — the whole policy, with the
    /// data-dir lookup lifted out so it is testable without a global data dir.
    fn at(path: &Path) -> Option<Self> {
        if path.as_os_str().is_empty() {
            return Some(Self(PathBuf::new()));
        }
        let path = path.to_path_buf();
        if let Ok(md) = std::fs::metadata(&path) {
            let age = md.modified().ok().and_then(|m| m.elapsed().ok());
            if !Self::marker_is_stale(age) {
                return None;
            }
            let _ = std::fs::remove_file(&path);
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Some(Self(path)),
            // Lost the create race to a concurrent process — it is indexing.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
            Err(_) => Some(Self(PathBuf::new())),
        }
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// On-disk size (bytes) of a project's index database (incl. WAL/SHM sidecars).
pub fn project_db_size(root: &Path) -> i64 {
    db_family_size(&project_db_path(root))
}

/// Headline totals over the usage table, optionally scoped to one project.
#[derive(Default, Clone)]
pub struct Totals {
    pub calls: i64,
    pub tokens_out: i64,
    pub tokens_saved: i64,
    pub reads_blocked: i64,
    pub total_ms: i64,
}

impl Totals {
    /// Tokens the agent would have spent reading files wholesale.
    pub fn baseline(&self) -> i64 {
        self.tokens_out + self.tokens_saved
    }
    /// Percentage of the baseline that cona avoided (0..=100).
    pub fn pct_saved(&self) -> f64 {
        let b = self.baseline();
        if b <= 0 {
            0.0
        } else {
            (self.tokens_saved as f64 / b as f64) * 100.0
        }
    }
}

fn scope_clause(project: Option<&str>) -> (String, Vec<String>) {
    match project {
        Some(p) => (" WHERE project = ?1".into(), vec![p.to_string()]),
        None => (String::new(), vec![]),
    }
}

pub fn totals(g: &Connection, project: Option<&str>) -> Result<Totals> {
    let (where_, params) = scope_clause(project);
    let sql = format!(
        "SELECT COUNT(*), COALESCE(SUM(tokens_out),0), COALESCE(SUM(tokens_saved),0),
                COALESCE(SUM(CASE WHEN cmd LIKE 'hook:%-block' THEN 1 ELSE 0 END),0),
                COALESCE(SUM(ms),0)
         FROM usage{where_}"
    );
    let p = rusqlite::params_from_iter(params.iter());
    let t = g.query_row(&sql, p, |r| {
        Ok(Totals {
            calls: r.get(0)?,
            tokens_out: r.get(1)?,
            tokens_saved: r.get(2)?,
            reads_blocked: r.get(3)?,
            total_ms: r.get(4)?,
        })
    })?;
    Ok(t)
}

/// Per-command aggregate row: (cmd, calls, avg_ms, tokens_out, tokens_saved).
pub type CommandRow = (String, i64, f64, i64, i64);

/// Per-command aggregate, most-called first.
pub fn per_command(g: &Connection, project: Option<&str>) -> Result<Vec<CommandRow>> {
    let (where_, params) = scope_clause(project);
    let sql = format!(
        "SELECT cmd, COUNT(*), AVG(ms), COALESCE(SUM(tokens_out),0), COALESCE(SUM(tokens_saved),0)
         FROM usage{where_} GROUP BY cmd ORDER BY COUNT(*) DESC"
    );
    let mut stmt = g.prepare(&sql)?;
    let p = rusqlite::params_from_iter(params.iter());
    let rows = stmt
        .query_map(p, |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .flatten()
        .collect();
    Ok(rows)
}

/// Most frequent query targets: (detail, count, tokens_saved).
pub fn top_targets(
    g: &Connection,
    project: Option<&str>,
    limit: i64,
) -> Result<Vec<(String, i64, i64)>> {
    let (mut where_, params) = scope_clause(project);
    if where_.is_empty() {
        where_ = " WHERE detail <> ''".into();
    } else {
        where_.push_str(" AND detail <> ''");
    }
    let sql = format!(
        "SELECT detail, COUNT(*), COALESCE(SUM(tokens_saved),0)
         FROM usage{where_} GROUP BY detail ORDER BY COUNT(*) DESC, 3 DESC LIMIT {limit}"
    );
    let mut stmt = g.prepare(&sql)?;
    let p = rusqlite::params_from_iter(params.iter());
    let rows = stmt
        .query_map(p, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .flatten()
        .collect();
    Ok(rows)
}

/// Recent-query row: (ts, cmd, detail, tokens_saved, ms).
pub type RecentRow = (i64, String, String, i64, i64);

/// Recent queries, newest first. With `queries_only`, maintenance commands
/// (index/edit/rename/note/hook:* — see `is_maintenance_cmd`) are dropped;
/// they carry no savings and are just noise in an activity feed.
pub fn recent(
    g: &Connection,
    project: Option<&str>,
    limit: i64,
    queries_only: bool,
) -> Result<Vec<RecentRow>> {
    let (mut where_, params) = scope_clause(project);
    if queries_only {
        // keep in lockstep with is_maintenance_cmd
        let filter = "cmd NOT IN ('index','edit','rename','note') AND cmd NOT LIKE 'hook:%'";
        where_ = if where_.is_empty() {
            format!(" WHERE {filter}")
        } else {
            format!("{where_} AND {filter}")
        };
    }
    let sql = format!(
        "SELECT ts, cmd, detail, tokens_saved, ms FROM usage{where_} ORDER BY id DESC LIMIT {limit}"
    );
    let mut stmt = g.prepare(&sql)?;
    let p = rusqlite::params_from_iter(params.iter());
    let rows = stmt
        .query_map(p, |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .flatten()
        .collect();
    Ok(rows)
}

/// Human-friendly relative time, e.g. "3m ago", "just now".
pub fn ago(ts: i64) -> String {
    let d = (now() - ts).max(0);
    if d < 5 {
        "just now".into()
    } else if d < 60 {
        format!("{d}s ago")
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86400)
    }
}

/// Human-friendly byte size.
pub fn human_bytes(n: i64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

// ---------------------------------------------------------------------------
// storage introspection + automatic maintenance
// ---------------------------------------------------------------------------

/// Path to the global registry/stats database.
pub fn global_db_path() -> Option<PathBuf> {
    data_dir().ok().map(|d| d.join("global.db"))
}

fn file_len(p: &Path) -> i64 {
    std::fs::metadata(p).map(|m| m.len() as i64).unwrap_or(0)
}

/// Size of a SQLite DB including its `-wal`/`-shm` sidecar files.
fn db_family_size(db: &Path) -> i64 {
    let mut n = file_len(db);
    if let Some(s) = db.to_str() {
        n += file_len(Path::new(&format!("{s}-wal")));
        n += file_len(Path::new(&format!("{s}-shm")));
    }
    n
}

/// On-disk size (bytes) of the global registry/stats database.
pub fn global_db_size() -> i64 {
    global_db_path().map(|p| db_family_size(&p)).unwrap_or(0)
}

/// Total on-disk size of everything under ~/.cona (recursive).
pub fn total_storage_bytes() -> i64 {
    let Ok(root) = data_dir() else { return 0 };
    fn walk(dir: &Path) -> i64 {
        let mut total = 0;
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                match e.file_type() {
                    Ok(t) if t.is_dir() => total += walk(&p),
                    Ok(t) if t.is_file() => total += file_len(&p),
                    _ => {}
                }
            }
        }
        total
    }
    walk(&root)
}

/// Number of usage rows currently logged.
pub fn usage_row_count(g: &Connection) -> i64 {
    g.query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
        .unwrap_or(0)
}

/// Everything the storage report shows, computed once. The single source for
/// both `stats` (append_storage) and `doctor` — the two renderers format this
/// struct, never re-query on their own.
pub struct StorageSummary {
    pub data_dir: PathBuf,
    pub total: i64,
    pub global_db: PathBuf,
    pub global_db_size: i64,
    pub usage_rows: i64,
    pub projects: i64,
    pub project_db: PathBuf,
    pub project_db_size: i64,
    pub last_tidy: String,
    /// >100 MB — suggest `tidy --orphans`
    pub over_limit: bool,
}

pub fn storage_summary(g: &Connection, root: &Path) -> Result<StorageSummary> {
    let total = total_storage_bytes();
    Ok(StorageSummary {
        data_dir: data_dir()?,
        total,
        global_db: global_db_path().unwrap_or_default(),
        global_db_size: global_db_size(),
        usage_rows: usage_row_count(g),
        projects: g
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap_or(0),
        project_db: project_db_path(root),
        project_db_size: project_db_size(root),
        last_tidy: meta_get("last_tidy")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok())
            .map(ago)
            .unwrap_or_else(|| "never".into()),
        over_limit: total > 100 * 1024 * 1024,
    })
}

/// True if `p` is the user's home directory or a filesystem root — indexing
/// these walks enormous trees and is almost always a mistake.
pub fn is_home_or_fs_root(p: &Path) -> bool {
    if p.parent().is_none() {
        return true;
    }
    // Compared through `canonicalize` because the two paths reach us by
    // different routes: the home dir from the environment, the root from the
    // cwd the process was started in. One side resolving a symlink the other
    // spells literally (macOS `/tmp` → `/private/tmp` is the everyday case)
    // would make the guard silently miss and walk the whole tree. Falls back to
    // the literal path when canonicalize fails — a comparison is better than
    // none.
    let real = |q: &Path| q.canonicalize().unwrap_or_else(|_| q.to_path_buf());
    dirs::home_dir().is_some_and(|h| real(&h) == real(p))
}

/// Drop a project's index entirely: delete its DB files, registry row and usage
/// rows. Returns the bytes reclaimed.
pub fn forget_project(root: &Path) -> Result<i64> {
    remove_project_data(root, false)
}

/// Delete a project's index DB (+ registry row); `keep_stats` preserves the
/// usage history so a reset doesn't zero the savings numbers.
pub fn remove_project_data(root: &Path, keep_stats: bool) -> Result<i64> {
    let bytes = project_db_size(root);
    let base = data_dir()?.join("projects");
    let hash = project_hash(root);
    for suffix in ["db", "db-wal", "db-shm"] {
        let _ = std::fs::remove_file(base.join(format!("{hash}.{suffix}")));
    }
    let g = open_global_db()?;
    g.execute("DELETE FROM projects WHERE hash = ?1", [&hash])?;
    if !keep_stats {
        g.execute(
            "DELETE FROM usage WHERE project = ?1",
            [root.to_string_lossy()],
        )?;
    }
    Ok(bytes)
}

fn retention_days() -> i64 {
    std::env::var("CONA_USAGE_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(90)
}

fn max_usage_rows() -> i64 {
    std::env::var("CONA_MAX_USAGE_ROWS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(200_000)
}

/// What a tidy pass did.
#[derive(Default, Clone, Debug)]
pub struct TidyReport {
    pub usage_deleted: i64,
    pub orphans_removed: i64,
    pub bytes_before: i64,
    pub bytes_after: i64,
}

impl TidyReport {
    pub fn bytes_reclaimed(&self) -> i64 {
        (self.bytes_before - self.bytes_after).max(0)
    }
    pub fn did_something(&self) -> bool {
        self.usage_deleted > 0 || self.orphans_removed > 0
    }
}

/// Prune the usage log (by age + row cap), optionally drop indexes for projects
/// whose directory no longer exists, and reclaim space. Keeps ~/.cona bounded.
pub fn tidy(purge_orphans: bool, vacuum: bool) -> Result<TidyReport> {
    let bytes_before = total_storage_bytes();
    let g = open_global_db()?;

    // 1) age-based prune
    let cutoff = now() - retention_days() * 86_400;
    let mut deleted = g.execute("DELETE FROM usage WHERE ts < ?1", [cutoff])? as i64;

    // 2) hard cap on total rows (delete oldest beyond the cap)
    let max = max_usage_rows();
    let count = usage_row_count(&g);
    if count > max {
        deleted += g.execute(
            "DELETE FROM usage WHERE id IN (SELECT id FROM usage ORDER BY id ASC LIMIT ?1)",
            [count - max],
        )? as i64;
    }

    // 3) orphaned project indexes (path gone from disk). Without
    // purge_orphans only ephemeral paths (temp roots) are dropped — a project
    // on an unmounted volume must survive the daily auto_tidy.
    let mut orphans = 0i64;
    {
        let base = data_dir()?.join("projects");
        let rows: Vec<(String, String)> = {
            let mut stmt = g.prepare("SELECT hash, path FROM projects")?;
            let v: Vec<(String, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .flatten()
                .collect();
            v
        };
        for (hash, path) in rows {
            if Path::new(&path).exists() {
                continue;
            }
            if !purge_orphans && !is_ephemeral_path(Path::new(&path)) {
                continue;
            }
            for suffix in ["db", "db-wal", "db-shm"] {
                let _ = std::fs::remove_file(base.join(format!("{hash}.{suffix}")));
            }
            g.execute("DELETE FROM projects WHERE hash = ?1", [&hash])?;
            g.execute("DELETE FROM usage WHERE project = ?1", [&path])?;
            orphans += 1;
        }
    }

    // 4) reclaim space if we actually removed anything
    if vacuum && (deleted > 0 || orphans > 0) {
        g.execute_batch("VACUUM")?;
    }
    meta_set("last_tidy", &now().to_string())?;

    Ok(TidyReport {
        usage_deleted: deleted,
        orphans_removed: orphans,
        bytes_before,
        bytes_after: total_storage_bytes(),
    })
}

/// Called on normal commands: runs a light tidy at most once per day so the
/// usage log never grows without bound. Cheap and silent; never fails loudly.
pub fn auto_tidy() {
    let last = meta_get("last_tidy")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    if now() - last < 86_400 {
        return;
    }
    // conservative: prune + vacuum global; orphaned indexes are dropped only
    // for ephemeral (temp-root) paths — full purge stays `tidy --orphans`
    let _ = tidy(false, true);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(name: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("cona-test-{name}-{}.indexing", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn index_lock_excludes_a_second_holder() {
        let p = marker("lock-excl");
        let first = IndexLock::at(&p).expect("first acquire wins");
        assert!(
            IndexLock::at(&p).is_none(),
            "second acquire must be refused"
        );
        drop(first);
        // Released on drop, so the next session can index again.
        assert!(IndexLock::at(&p).is_some(), "lock must free on drop");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn index_lock_reclaims_a_stale_marker() {
        // A marker from a killed process must not wedge indexing forever, and a
        // fresh one must still exclude. Age is judged by `marker_is_stale`, so
        // the policy is checked without having to backdate a real file.
        assert!(!IndexLock::marker_is_stale(Some(
            std::time::Duration::from_secs(1)
        )));
        assert!(IndexLock::marker_is_stale(Some(
            std::time::Duration::from_secs(IndexLock::STALE_SECS + 1)
        )));
        // An unreadable/absurd mtime (clock skew makes `elapsed` fail) counts as
        // stale: better one duplicate walk than indexing wedged for good.
        assert!(IndexLock::marker_is_stale(None));
    }

    #[test]
    fn baseline_windows_not_whole_file() {
        // 500 lines of 80 chars: whole file ≈ (500*81)/4 ≈ 10125 tok.
        let lens = vec![80usize; 500];
        let whole = est_tokens(500 * 80 + 500);
        // one hit in the middle → only a ±40 window (81 lines) counts.
        let one = baseline_tokens(&lens, &[250]);
        assert!(one < whole / 4, "one hit {one} vs whole {whole}");
        let expect_win = est_tokens(81 * 80 + 81);
        assert_eq!(one, expect_win);
        // empty hits ⇒ whole file (agent scanned everything, found no anchor).
        assert_eq!(baseline_tokens(&lens, &[]), whole);
        // adjacent hits merge into one window, not double-counted.
        let merged = baseline_tokens(&lens, &[250, 251, 252]);
        assert!(merged < one * 2, "merged {merged} vs 2×one {}", one * 2);
        // out-of-range hits are ignored (fail-safe, never panics).
        assert_eq!(baseline_tokens(&lens, &[9999]), whole);
        assert_eq!(baseline_tokens(&[], &[1]), 0);
    }

    #[test]
    fn maintenance_cmds_classified() {
        for cmd in ["index", "edit", "hook:read-block"] {
            assert!(is_maintenance_cmd(cmd), "{cmd}");
        }
        for cmd in ["tree", "outline", "find", "show", "refs"] {
            assert!(!is_maintenance_cmd(cmd), "{cmd}");
        }
    }

    #[test]
    fn ephemeral_paths_classified() {
        // The system temp dir is ephemeral on every platform.
        let tmp = std::env::temp_dir().join("cona-it-1");
        assert!(is_ephemeral_path(&tmp), "{}", tmp.display());

        // Hard-coded unix roots only classify as ephemeral on unix.
        #[cfg(unix)]
        for p in [
            "/tmp/ltest",
            "/private/tmp/it2",
            "/private/var/folders/6q/x/T/cona-git-123",
        ] {
            assert!(is_ephemeral_path(Path::new(p)), "{p}");
        }
        for p in ["/Users/u/dev/proj", "/Volumes/ext/repo", "/home/u/tmp/x"] {
            assert!(!is_ephemeral_path(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn usage_detail_migration_is_idempotent() {
        // simulate an old global.db whose usage table predates the `detail` column
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage(id INTEGER PRIMARY KEY, ts INTEGER, tokens_saved INTEGER)",
        )
        .unwrap();
        assert!(!column_exists(&conn, "usage", "detail").unwrap());
        // running the guarded migration twice must be safe
        for _ in 0..2 {
            if !column_exists(&conn, "usage", "detail").unwrap() {
                conn.execute_batch("ALTER TABLE usage ADD COLUMN detail TEXT NOT NULL DEFAULT ''")
                    .unwrap();
            }
        }
        assert!(column_exists(&conn, "usage", "detail").unwrap());
        conn.execute(
            "INSERT INTO usage(ts, tokens_saved, detail) VALUES(1, 2, 'sym')",
            [],
        )
        .unwrap();
        let d: String = conn
            .query_row("SELECT detail FROM usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(d, "sym");
    }

    #[test]
    fn totals_baseline_and_pct() {
        let t = Totals {
            calls: 3,
            tokens_out: 100,
            tokens_saved: 900,
            reads_blocked: 1,
            total_ms: 5,
        };
        assert_eq!(t.baseline(), 1000);
        assert!((t.pct_saved() - 90.0).abs() < 1e-9);
        assert_eq!(Totals::default().pct_saved(), 0.0);
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert!(human_bytes(5 * 1024 * 1024).ends_with("MB"));
    }

    #[test]
    fn usage_row_cap_deletes_oldest() {
        // the tidy row-cap query must drop the OLDEST rows down to the cap
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE usage(id INTEGER PRIMARY KEY, ts INTEGER)")
            .unwrap();
        for i in 0..10 {
            conn.execute("INSERT INTO usage(ts) VALUES(?1)", [i])
                .unwrap();
        }
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .unwrap();
        let max = 4;
        let deleted = conn
            .execute(
                "DELETE FROM usage WHERE id IN (SELECT id FROM usage ORDER BY id ASC LIMIT ?1)",
                [count - max],
            )
            .unwrap();
        assert_eq!(deleted, 6);
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 4);
        let min_id: i64 = conn
            .query_row("SELECT MIN(id) FROM usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(min_id, 7); // oldest ids 1..=6 gone, newest 7..=10 kept
    }
}
