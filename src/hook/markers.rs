//! Per-(project, session) marker files: liveness stamp, read log, nudge and
//! tool-call counters. Every write is best-effort — a failed marker only
//! costs bookkeeping, never the tool call.

use super::{env_i64, DEFAULT_NUDGE_EVERY};
use crate::db;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Liveness stamp for `cona doctor`: its mtime says when a hook last fired,
/// separating "configured but never run" (stale settings, broken PATH) from a
/// healthy install. Rewritten at most hourly, so the hot path costs one stat.
pub(crate) fn touch_liveness() {
    let Ok(dir) = db::data_dir() else { return };
    let path = dir.join(LIVENESS_FILE);
    let fresh = file_age_secs(&path).is_some_and(|age| age < 3600);
    if !fresh {
        let _ = std::fs::write(&path, b"");
    }
}

/// Filename of the liveness stamp — `cona doctor` reads the same file.
pub const LIVENESS_FILE: &str = "hook-last-seen";

/// A session marker (and the liveness stamp) older than this is certainly
/// dead: the per-day fallback key rolls daily, real session ids within a week
/// are plausibly live. Shared with `cona doctor`'s silence threshold.
pub const MARKER_MAX_AGE_SECS: u64 = 7 * 86_400;

/// Seconds since `path` was last written, None when it is missing/unreadable.
pub fn file_age_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|age| age.as_secs())
}

/// Session identity for the per-session markers, in preference order:
/// `CLAUDE_SESSION_ID` (some harnesses), then the payload's `session_id`
/// (Codex sends one, exports nothing), then a per-day key so a long-lived
/// shell buckets by day instead of churning a marker every call.
pub(crate) fn session_id(v: &serde_json::Value) -> String {
    std::env::var("CLAUDE_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            v["session_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("day-{}", db::now() / 86_400))
}

/// Path of a per-(project, session) marker file under `data_dir/<kind>/`.
/// Every session-scoped marker goes through here so the identity rule lives in
/// ONE place. `None` when the data dir is unavailable; callers pick a fallback.
pub(crate) fn session_marker_path(root: &Path, kind: &str, session: &str) -> Option<PathBuf> {
    let dir = db::data_dir().ok()?;
    Some(
        dir.join(kind)
            .join(format!("{}-{session}", db::project_hash(root))),
    )
}

/// Pure cadence policy shared by every periodic reminder (re-nudge, read
/// streak): fire on each multiple of `every` for a running count (this call
/// included) — never at 0, never when disabled (`every <= 0`).
pub fn fires_on_cadence(count: i64, every: i64) -> bool {
    every > 0 && count > 0 && count % every == 0
}

/// Look up this (project, session)'s read log WITHOUT writing: whether `rel`
/// was already fully read, and how many *counted* reads it holds. One path per
/// line; a tab-prefixed line is a read that already carried an advisory — it
/// marks the path as seen (the bytes DID land in context) but does not count
/// toward the streak, else one mistake would be nagged twice.
///
/// No data dir → "not a re-read", silent rather than guessing.
pub(crate) fn peek_reads(root: &Path, rel: &str, session: &str) -> (bool, i64) {
    let Some(log) = session_marker_path(root, "reads", session) else {
        return (false, 0);
    };
    if !log.exists() {
        prepare_marker(&log);
        return (false, 0);
    }
    let (mut seen, mut count) = (false, 0i64);
    for line in std::fs::read_to_string(&log).unwrap_or_default().lines() {
        seen |= line.strip_prefix('\t').unwrap_or(line) == rel;
        count += i64::from(!line.starts_with('\t'));
    }
    (seen, count)
}

/// Record one NARROW read of `rel` and report how many this session has now
/// seen (including this one).
///
/// A single bounded read (`Read` offset/limit, `sed -n '300,330p'`,
/// `head -n 50`) always passes. But paging one file in four slices is a
/// costlier `outline` + `show`: the agent gropes for symbol boundaries and
/// re-pays context each slice — a shape that only exists across calls.
///
/// Its own marker kind, not `reads`: a slice is not the file, so it must not
/// mark a path "fully in context" nor move the full-read counter. Any failure
/// → 0, silent.
pub(crate) fn bump_partial_reads(root: &Path, rel: &str, session: &str) -> i64 {
    let Some(log) = session_marker_path(root, "partials", session) else {
        return 0;
    };
    prepare_marker(&log);
    let prior = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .filter(|l| *l == rel)
        .count() as i64;
    append_marker_line(&log, rel);
    prior + 1
}

/// Append one read that actually went through to the session's read log.
/// `counted` = the read carried no advisory (see `peek_reads`). A DENIED read
/// is never recorded — the bytes never reached the agent, so a retry must not
/// look like a re-read.
pub(crate) fn record_read(root: &Path, rel: &str, session: &str, counted: bool) {
    let Some(log) = session_marker_path(root, "reads", session) else {
        return;
    };
    prepare_marker(&log);
    let prefix = if counted { "" } else { "\t" };
    // Append rather than rewrite: the log grows for the whole session.
    append_marker_line(&log, &format!("{prefix}{rel}"));
}

/// Record that a full read of `rel` was denied this session and report whether
/// it already had been. A SECOND attempt after a block means the agent still
/// wants the file — denying again is a loop, not guidance, so the caller lets
/// it through. No data dir → "not denied yet" (degrades to always-deny).
pub(crate) fn note_denied(root: &Path, rel: &str, session: &str) -> bool {
    let Some(log) = session_marker_path(root, "denied", session) else {
        return false;
    };
    let seen = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .any(|l| l == rel);
    if !seen {
        prepare_marker(&log);
        append_marker_line(&log, rel);
    }
    seen
}

/// Increment and return the per-(project, session) tool-call counter. Any IO
/// failure returns 0, so the caller simply doesn't re-nudge this call.
pub(crate) fn tick_toolcall(root: &Path, session: &str) -> i64 {
    let Some(counter) = session_marker_path(root, "toolcalls", session) else {
        return 0;
    };
    let prev = std::fs::read_to_string(&counter)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let next = prev + 1;
    if let Some(parent) = counter.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&counter, next.to_string());
    next
}

/// Whether the "this repo isn't indexed" hint is due. Fires on the FIRST
/// nudge-eligible event of a (project, session), then after every
/// `CONA_NUDGE_EVERY` suppressed ones (default 10, 0 = never repeat). The
/// marker file holds the running event count.
pub(crate) fn nudge_due(root: &Path, session: &str) -> bool {
    let Some(marker) = session_marker_path(root, "nudged", session) else {
        return true; // can't track → don't suppress the (useful) first hint
    };
    prepare_marker(&marker);
    let count = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
        + 1;
    // best-effort, like every marker write
    let _ = std::fs::write(&marker, count.to_string());
    count == 1
        || fires_on_cadence(
            count - 1,
            env_i64("CONA_NUDGE_EVERY", DEFAULT_NUDGE_EVERY, 0),
        )
}

/// Delete session markers older than `MARKER_MAX_AGE_SECS`. Called only when a
/// NEW session touches the dir, so steady-state hook calls pay no scan.
fn prune_marker_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        if file_age_secs(&e.path()).is_some_and(|age| age > MARKER_MAX_AGE_SECS) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// First touch of a session's marker file: create its directory and prune
/// markers whose sessions are over. Steady-state calls cost one stat.
fn prepare_marker(log: &Path) {
    if log.exists() {
        return;
    }
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
        prune_marker_dir(parent);
    }
}

/// Append one line to a marker log (best-effort).
fn append_marker_line(log: &Path, line: &str) {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut f| f.write_all(format!("{line}\n").as_bytes()));
}
