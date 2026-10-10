use anyhow::{anyhow, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

static READ_ONLY: AtomicBool = AtomicBool::new(false);
static EPHEMERAL_DATA_DIR_NOTICE: Once = Once::new();

/// Process-wide inspection-only mode (`--read-only`). Enforced in the DB layer
/// so no command can accidentally write telemetry or schema changes.
pub fn set_read_only(enabled: bool) {
    READ_ONLY.store(enabled, Ordering::Relaxed);
}

pub fn is_read_only() -> bool {
    READ_ONLY.load(Ordering::Relaxed)
}

/// Nearest ancestor of `start` (inclusive) containing a `.git`, else `start`.
/// THE answer to "which project does this path belong to" — the hook and
/// `project_root` both resolve here, so a file's root and the indexed root
/// can never diverge.
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

/// Resolved once per process. `data_dir` sits on the hook hot path, is reached
/// several times per command, and resolving it writes (mkdir + create + unlink
/// probe) — per call that turns the "single stat, no DB open" hook check into a
/// dozen syscalls. Sound because neither input changes mid-process:
/// `CONA_DATA_DIR` comes from the env, `--read-only` is set before any storage
/// access.
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
/// lookup that disagrees with where the fallback index was written looks like
/// an unindexed repo, so never spell the path out per call site.
pub fn ephemeral_data_dir() -> PathBuf {
    std::env::temp_dir().join("cona")
}

fn resolve_data_dir() -> Result<PathBuf> {
    // CONA_DATA_DIR is explicit: never silently redirect it. The fallback
    // below is only for the default ~/.cona, which sandboxes often make read-only.
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
        // Read-only: never probe by creating directories. A readable global.db
        // marks durable storage — the bare ~/.cona dir is not enough, since
        // sandboxes often expose it but deny database access inside.
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

/// A created directory doesn't prove SQLite can write there: a sandbox may
/// expose `~/.cona` but deny new files inside. A PID-scoped probe file catches
/// that before rusqlite's opaque open error does.
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
    // Rows are only re-extracted when mtime/size moves, so an extractor change
    // would never reach an existing index. An older stamp marks every file
    // stale; the next refresh reparses them all once.
    let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if ver < EXTRACT_VERSION {
        conn.execute_batch(&format!(
            "UPDATE files SET mtime = -1; PRAGMA user_version = {EXTRACT_VERSION};"
        ))?;
    }
    Ok(conn)
}

/// Bump whenever symbol extraction changes what an unchanged file yields
/// (new kinds, new parents) — see `open_project_db`.
/// 1: Go methods qualified by receiver type; JS/TS/Go top-level constants.
/// 2: JS/TS signatures keep their `export` prefix.
const EXTRACT_VERSION: i64 = 2;

/// Open an already-built project index without creating files or migrating.
/// Prefers durable storage, but also checks the sandbox fallback so
/// `--read-only` can inspect an index built earlier in the same sandbox.
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
    // migration: `detail` = query target (symbol/file/pattern) for top-target
    // stats. Additive — safe on old DBs.
    if !column_exists(&conn, "usage", "detail")? {
        conn.execute_batch("ALTER TABLE usage ADD COLUMN detail TEXT NOT NULL DEFAULT ''")?;
    }
    // migration: `outcome` separates answered queries ('') from misses,
    // ambiguities, empty results and errors — the input of `cona learn`.
    if !column_exists(&conn, "usage", "outcome")? {
        conn.execute_batch("ALTER TABLE usage ADD COLUMN outcome TEXT NOT NULL DEFAULT ''")?;
    }
    // hook-conversion stats probe "a query in this project shortly after ts"
    conn.execute_batch("CREATE INDEX IF NOT EXISTS usage_project_ts ON usage(project, ts)")?;
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

/// Whether `root` already has a populated index. Never creates a DB — safe
/// from hooks on arbitrary directories.
///
/// Uses the same read-only-aware open as `open_indexed`: gating on
/// `project_db_path` alone would miss an index in the sandbox fallback that
/// `open_indexed` finds, so the two would disagree about the same repo.
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
/// N sessions opening together each fire SessionStart; without this each
/// walks the same tree — N times the CPU and peak memory for one result. The
/// lock is an exclusively-created marker next to the project DB; the loser
/// skips rather than waits, since the winner builds the same index and brief
/// staleness is invisible (`locate_fresh` re-checks per query anyway).
///
/// Held for the guard's lifetime, unlinked on drop. A marker left by a killed
/// process would block every later walk, so one older than
/// `IndexLock::STALE_SECS` is reclaimed.
pub struct IndexLock(PathBuf);

impl IndexLock {
    /// Age past which a marker is assumed orphaned: far longer than any real
    /// walk, short enough that a crash doesn't wedge indexing for a session.
    const STALE_SECS: u64 = 300;

    /// Whether a marker of this age is orphaned. `None` = the age is unknown
    /// (unreadable mtime, or a clock that moved backwards) and counts as stale:
    /// one redundant walk beats indexing wedged until the file is removed.
    fn marker_is_stale(age: Option<std::time::Duration>) -> bool {
        age.is_none_or(|a| a.as_secs() > Self::STALE_SECS)
    }

    /// `Some(guard)` if this process may index `root`; `None` if another one
    /// is. Filesystem trouble yields a guard — fail-open: indexing matters more
    /// than dedup.
    pub fn acquire(root: &Path) -> Option<Self> {
        Self::at(&project_db_path(root).with_extension("indexing"))
    }

    /// `acquire` against an explicit marker path — testable without a data dir.
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

mod maint;
mod stats;
#[cfg(test)]
mod tests;
mod usage;

pub use maint::*;
pub use stats::*;
pub use usage::*;
