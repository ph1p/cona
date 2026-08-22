//! Storage introspection + automatic maintenance (tidy, forget, caps).

use super::*;
use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// On-disk size (bytes) of a project's index database (incl. WAL/SHM sidecars).
pub fn project_db_size(root: &Path) -> i64 {
    db_family_size(&project_db_path(root))
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
