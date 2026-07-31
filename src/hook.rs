//! Agent tool-call hooks.
//!
//! cona is a *navigation accelerator*, never a gatekeeper. The PreToolUse
//! hook only ever redirects the agent toward a faster path and always fails
//! open — any parse error, missing index, unknown/non-code file, small file or
//! partial read passes straight through untouched. It never blocks anything the
//! agent's own machinery (caching, batching, other tools) relies on.
//!
//! Two intercepts. A large full read / broad identifier grep in an INDEXED
//! project is *redirected* (blocked, with the faster cona command in the
//! reason). The same call in a git repo that simply hasn't been indexed yet is
//! *nudged* (allowed, but with a one-time hint that indexing unlocks cona) —
//! this is the cold-open case where orientation help matters most and the old
//! "indexed only" rule stayed silent. Everything else passes.
//!
//! Between "small enough to ignore" and "big enough to block" sits the case that
//! actually drains context: a 150–300 line file read in full to understand ONE
//! function. Three *advisory* outcomes cover it, all of which ALLOW the read and
//! merely attach a hint — because the same read is correct when the agent is
//! about to rewrite the file, and only the agent knows which it is:
//!   - mid-size indexed file (>= `CONA_ADVISE_MIN_LINES`) read in full
//!   - re-read of a path already fully read this session (size-blind: those
//!     bytes are already in context, so a repeat is redundant at any length)
//!   - the N-th full read in one session (`CONA_READ_STREAK`) — individually
//!     innocent reads are how context leaks; no single-call rule sees the run

use crate::{db, indexer, lang};
use anyhow::Result;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Default line threshold above which a full read of an indexed code file is
/// redirected to `cona outline`/`show`. Override with `CONA_READ_MAX_LINES`.
const DEFAULT_MAX_LINES: i64 = 300;

/// Default line threshold above which a full read is *advised* against (allowed,
/// with a hint attached) rather than redirected. Sits below `DEFAULT_MAX_LINES`:
/// a 150-line source file is ~1.5k tokens, and reading all of it to understand
/// ONE function is the single most common way an agent wastes context — but it
/// is also sometimes correct (about to rewrite the file), so this tier never
/// blocks. Override with `CONA_ADVISE_MIN_LINES`; 0 disables the tier.
const DEFAULT_ADVISE_MIN_LINES: i64 = 120;

/// Default number of allowed full reads in one session+project before the hook
/// points out the pattern. Individually-innocent sub-threshold reads are how
/// context actually drains (four 200-line reads = ~7k tokens for what a few
/// `show` calls deliver in a few hundred). Override with `CONA_READ_STREAK`;
/// 0 disables.
const DEFAULT_READ_STREAK: i64 = 4;

/// Default cadence for the periodic re-nudge: OFF. Repeating the same guidance
/// across SessionStart, the agent guide and a timer is over-constraint for
/// current models — they hold the habit from one statement, and the PreToolUse
/// redirect still catches an actual wrong Read/Grep. Opt in with
/// `CONA_RENUDGE_EVERY=<n>` (n tool calls between reminders) on a model that
/// drifts; 0 keeps it disabled.
const DEFAULT_RENUDGE_EVERY: i64 = 0;

/// What the hook should do about a candidate tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Pass through untouched.
    Allow,
    /// Block and point at the faster cona path (project is indexed, so the
    /// redirect is actionable right now).
    Redirect,
    /// Allow, but surface a one-time hint that indexing would unlock cona.
    /// Used when the file/project is a good cona fit but not yet indexed —
    /// the moment orientation help matters most (agent cold-opening a repo).
    Nudge,
    /// Allow, but attach a hint that a symbol-scoped read would be cheaper.
    /// Unlike `Redirect` this never blocks: the read may well be justified
    /// (the agent is about to rewrite the file), so we inform and step aside.
    Advise,
}

/// Facts about a candidate `Read` call — kept primitive so the decision is
/// pure and unit-testable.
#[derive(Debug, Clone)]
pub struct ReadFacts {
    /// The Read already has an explicit offset/limit (agent is being surgical).
    pub partial: bool,
    /// cona indexes this language.
    pub is_code: bool,
    /// The file is present in the project index.
    pub indexed: bool,
    /// The file lives inside a git repo (a project cona could index).
    pub in_repo: bool,
    /// Line count on disk.
    pub lines: i64,
    /// Threshold above which we redirect.
    pub max_lines: i64,
    /// Lower threshold above which we merely advise. 0 disables the tier.
    pub advise_min_lines: i64,
    /// The language has functions/methods worth reading one at a time. False for
    /// prose/data (Markdown, JSON, YAML…), where "read one symbol" is not real
    /// advice — those files are read as prose or whole. Gates the advisory tier
    /// only; a genuinely huge file still redirects on size alone.
    pub callable: bool,
    /// This exact file was already fully read earlier this session. Size-blind:
    /// a re-read is redundant at any length, and it is the highest-confidence
    /// waste signal available (the content is already in context).
    pub reread: bool,
}

/// Decide what to do with a candidate `Read`.
/// - large indexed code file, full read → Redirect (block, actionable now)
/// - large UNINDEXED code file in a git repo, full read → Nudge (index me)
/// - re-read of an already-read indexed file → Advise (any size)
/// - mid-size indexed code file, full read → Advise (allowed, hint attached)
/// - everything else (partial, small, non-code, loose file) → Allow
pub fn decide_read(f: &ReadFacts) -> Decision {
    if f.partial || !f.is_code {
        return Decision::Allow;
    }
    // Under the redirect threshold: a repeat full read is wasteful regardless of
    // size (the bytes are already in context), and a mid-size file is worth a
    // hint. Both only apply when indexed and callable — otherwise there is
    // nothing useful to point at — and `advise_min_lines == 0` turns the whole
    // advisory tier off, re-reads included.
    if f.lines <= f.max_lines {
        let advisable = f.indexed && f.callable && f.advise_min_lines > 0;
        if advisable && (f.reread || f.lines >= f.advise_min_lines) {
            return Decision::Advise;
        }
        return Decision::Allow;
    }
    if f.indexed {
        Decision::Redirect
    } else if f.in_repo {
        Decision::Nudge
    } else {
        Decision::Allow
    }
}

/// Facts about a candidate `Grep` call — kept primitive so the decision is
/// pure and unit-testable.
#[derive(Debug, Clone)]
pub struct GrepFacts {
    /// The Grep is already narrowed (glob/type filter, head_limit, or a
    /// single-file path) — the agent is being surgical.
    pub surgical: bool,
    /// The pattern is a plain identifier cona can serve semantically.
    pub identifier: bool,
    /// The search root is an indexed cona project.
    pub indexed_project: bool,
    /// The search root is inside a git repo (indexable, if not yet indexed).
    pub in_repo: bool,
}

/// Decide what to do with a candidate `Grep`.
/// - broad identifier search over an indexed project → Redirect
/// - broad identifier search over an UNINDEXED git repo → Nudge
/// - surgical / regex / non-repo → Allow
pub fn decide_grep(f: &GrepFacts) -> Decision {
    if f.surgical || !f.identifier {
        return Decision::Allow;
    }
    if f.indexed_project {
        Decision::Redirect
    } else if f.in_repo {
        Decision::Nudge
    } else {
        Decision::Allow
    }
}

/// Read an i64 tuning knob from the environment, ignoring anything unparseable
/// or below `min` so a typo falls back to the default instead of disabling a
/// tier silently. Every `CONA_*` threshold goes through here.
fn env_i64(key: &str, default: i64, min: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n >= min)
        .unwrap_or(default)
}

fn max_lines() -> i64 {
    env_i64("CONA_READ_MAX_LINES", DEFAULT_MAX_LINES, 1)
}

fn advise_min_lines() -> i64 {
    env_i64("CONA_ADVISE_MIN_LINES", DEFAULT_ADVISE_MIN_LINES, 0)
}

fn read_streak_every() -> i64 {
    env_i64("CONA_READ_STREAK", DEFAULT_READ_STREAK, 0)
}

/// Entry point for `cona hook <event>`. Reads the Claude hook payload from
/// stdin and prints a decision to stdout. ALWAYS exits 0 — this is a helper for
/// the agent, so a failure here must never break a tool call.
pub fn run(event: &str) -> Result<()> {
    if std::env::var("CONA_HOOK_DISABLE").is_ok() {
        return Ok(());
    }
    // Any error → do nothing silently.
    match event {
        "PreToolUse" => {
            let _ = try_pretooluse();
        }
        "PostToolUse" => {
            let _ = try_posttooluse();
        }
        _ => {}
    }
    Ok(())
}

fn try_pretooluse() -> Result<()> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let v: serde_json::Value = serde_json::from_str(&buf)?;

    match v["tool_name"].as_str() {
        Some("Read") => try_read(&v),
        Some("Grep") => try_grep(&v),
        _ => Ok(()),
    }
}

fn renudge_every() -> i64 {
    env_i64("CONA_RENUDGE_EVERY", DEFAULT_RENUDGE_EVERY, 0)
}

/// Path of a per-(project, session) marker file under `data_dir/<kind>/`.
///
/// Session identity comes from `CLAUDE_SESSION_ID` when the agent exports it;
/// without it we fall back to a per-day key so a long-lived shell buckets by
/// day rather than churning a new marker every call. Both session-scoped hook
/// mechanisms (`nudge_once`, `tick_toolcall`) share this so the identity rule
/// stays in ONE place — they only differ in `<kind>` and what they store.
/// `None` when the data dir is unavailable; each caller picks its own fallback.
fn session_marker_path(root: &Path, kind: &str) -> Option<PathBuf> {
    let session = std::env::var("CLAUDE_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("day-{}", db::now() / 86_400));
    let dir = db::data_dir().ok()?;
    Some(
        dir.join(kind)
            .join(format!("{}-{session}", db::project_hash(root))),
    )
}

/// Pure cadence policy shared by every periodic hook reminder: given a running
/// count (this call included) and a cadence, fire on each multiple of `every` —
/// never at 0, never when disabled (`every <= 0`). Used for both the PostToolUse
/// re-nudge and the read-volume streak so the two behave predictably together.
pub fn fires_on_cadence(count: i64, every: i64) -> bool {
    every > 0 && count > 0 && count % every == 0
}

/// Record a full read of `rel` for this (project, session) and report whether the
/// same path was already read. One newline-delimited path per line under the
/// `reads` marker kind; the file is the session's read log, so its line count
/// doubles as the read-volume counter (see `read_streak`).
///
/// Best-effort like every other hook side effect: if the data dir is unavailable
/// we report "not a re-read" and stay silent rather than guessing.
fn note_read(root: &Path, rel: &str) -> (bool, i64) {
    let Some(log) = session_marker_path(root, "reads") else {
        return (false, 0);
    };
    let (mut seen, mut count) = (false, 0i64);
    for line in std::fs::read_to_string(&log).unwrap_or_default().lines() {
        seen |= line == rel;
        count += 1;
    }
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Append rather than rewrite: the log grows for the whole session, and a
    // failed write only costs us this call's bookkeeping.
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .and_then(|mut f| f.write_all(format!("{rel}\n").as_bytes()));
    let _ = appended;
    (seen, count + 1)
}

/// Build a non-blocking read advisory: the caller's specific observation, then
/// the one shared "here's the cheaper move" tail. Kept in one place so the three
/// advisory triggers (mid-size, re-read, volume streak) cannot drift apart.
fn advisory(lead: &str, rel: &str) -> String {
    format!(
        "{lead}. If you need one function, `cona show <Symbol>` prints just its lines and \
         `cona context <Symbol>` adds callers/callees in the same call; `cona outline {rel}` \
         lists every symbol first. Reading a whole file is right when you're about to \
         rewrite it — this read ran as-is."
    )
}

/// PostToolUse: the opt-in periodic re-nudge (see `DEFAULT_RENUDGE_EVERY` —
/// off unless `CONA_RENUDGE_EVERY=<n>`). additionalContext ONLY — never a
/// permission decision — so it can never block or auto-approve a call. Fully
/// fail-open.
fn try_posttooluse() -> Result<()> {
    let every = renudge_every();
    if every == 0 {
        return Ok(());
    }
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let v: serde_json::Value = serde_json::from_str(&buf)?;

    let root = v["cwd"]
        .as_str()
        .map(PathBuf::from)
        .map(|c| db::git_root_from(&c))
        .unwrap_or_else(|| db::git_root_from(Path::new(".")));

    // Cheap indexed-repo gate: a single stat, no DB open. Ticking + the cadence
    // check run BEFORE the expensive `has_index` (which opens the SQLite DB) so
    // the common case — a call that will NOT nudge — never pays for a
    // connection. Only a call that actually lands on a nudge boundary opens
    // the DB, and only to confirm the index is real (not just a stale db file).
    if !db::project_db_path(&root).exists() {
        return Ok(());
    }
    let count = tick_toolcall(&root);
    if !fires_on_cadence(count, every) || !db::has_index(&root) {
        return Ok(());
    }
    let reason = "Reminder: this project is cona-indexed. Before a full Read or broad \
                  Grep of code, reach for `cona outline`/`show`/`grep`/`refs` — one \
                  symbol, not the whole file.";
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": reason,
        }
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

/// Increment and return the per-(project, session) tool-call counter. A tiny
/// file under the data dir holds the running count (path via
/// `session_marker_path`, so the session-identity rule is shared with
/// `nudge_once`). Best-effort: any IO failure returns 0 so the caller simply
/// doesn't re-nudge this call.
fn tick_toolcall(root: &Path) -> i64 {
    let Some(counter) = session_marker_path(root, "toolcalls") else {
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

fn try_read(v: &serde_json::Value) -> Result<()> {
    let input = &v["tool_input"];
    let Some(file_path) = input["file_path"].as_str() else {
        return Ok(());
    };
    let partial = !input["offset"].is_null() || !input["limit"].is_null();

    let file_abs = PathBuf::from(file_path);
    let dir = file_abs.parent().unwrap_or(Path::new("."));
    // prefer the git root the agent is working in
    let cwd = v["cwd"].as_str().map(PathBuf::from);
    let root = cwd
        .filter(|c| file_abs.starts_with(c))
        .map(|c| db::git_root_from(&c))
        .unwrap_or_else(|| db::git_root_from(dir));

    let rel = file_abs
        .strip_prefix(&root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| file_path.to_string());

    let detected = lang::detect_lang(&rel);
    let is_code = detected.is_some();
    let callable = detected.map(lang::has_callable_symbols).unwrap_or(false);
    let in_repo = root.join(".git").exists();

    // Cheap gates BEFORE reading any bytes: partial/non-code reads are Allow
    // regardless of size, and a multi-GB data file must never be slurped into
    // the hook just to be allowed anyway. A file of N lines is ≥ N bytes, so
    // size ≤ threshold guarantees lines ≤ threshold without reading. The floor
    // is the LOWEST active threshold (the advise tier when enabled), or we would
    // bail on mid-size files before the advisory path ever sees them. These
    // gates are deliberately narrower than decide_read's own cheap-gate — they
    // must NOT fold in `in_repo`, or a large indexed file in a non-git repo
    // would bail here before we ever check the index (that was the bug).
    let max_lines = max_lines();
    let advise_min_lines = advise_min_lines();
    if partial || !is_code {
        return Ok(());
    }
    // A file of N lines is >= N bytes, so a size below the threshold proves the
    // line count is too — used to skip the read_to_string, NOT to skip the
    // bookkeeping below (a re-read and the read streak are both size-blind).
    let byte_len = match std::fs::metadata(&file_abs) {
        Ok(m) => m.len() as i64,
        Err(_) => return Ok(()),
    };
    // Cap what we are willing to slurp. Past this size the exact line count no
    // longer changes the outcome (any such file is far over max_lines, so it is
    // a Redirect), and a multi-GB file must never be pulled into the hook.
    // Bytes >= lines, so the floor below is a sound lower bound for the message.
    const MAX_HOOK_READ_BYTES: i64 = 4 * 1024 * 1024;
    let measured = byte_len <= MAX_HOOK_READ_BYTES;
    let (lines, tokens) = if measured {
        match std::fs::read_to_string(&file_abs) {
            Ok(src) => (src.lines().count() as i64, db::est_tokens(src.len())),
            // Unreadable or not UTF-8 (binary) — not our business.
            Err(_) => return Ok(()),
        }
    } else {
        (max_lines + 1, db::est_tokens(byte_len as usize))
    };
    // Never print a line count we did not actually measure.
    let size_desc = if measured {
        format!("{lines} lines (~{tokens} tokens)")
    } else {
        format!("~{tokens} tokens")
    };

    // Is it indexed? Only open the DB if one already exists; never create a
    // project DB from a hook (mirrors try_grep's has_index gate).
    if !db::project_db_path(&root).exists() {
        return Ok(());
    }
    let conn = db::open_project_db(&root)?;
    let indexed: bool = conn
        .query_row("SELECT 1 FROM files WHERE path = ?1", [&rel], |_| Ok(true))
        .unwrap_or(false);

    // Log every full read of an indexed source file: re-read detection and the
    // read-volume streak are both size-blind, so this must happen even for files
    // far below the advisory floor. Skipped when unindexed (nothing to point at)
    // or non-callable (prose/data — a run of README reads is not the pattern we
    // are looking for, and must not inflate the counter for real source files),
    // and skipped entirely when both tiers that consume it are disabled, so the
    // default hot path pays no marker IO it cannot use.
    let streak_every = read_streak_every();
    let tracking_reads = advise_min_lines > 0 || streak_every > 0;
    let (reread, read_count) = if indexed && callable && tracking_reads {
        note_read(&root, &rel)
    } else {
        (false, 0)
    };

    let facts = ReadFacts {
        partial,
        is_code,
        indexed,
        in_repo,
        lines,
        max_lines,
        advise_min_lines,
        callable,
        reread,
    };
    match decide_read(&facts) {
        Decision::Allow => {
            // Individually fine, but volume is its own cost: several full reads
            // in one session is the drain pattern no single-call rule catches.
            if indexed && fires_on_cadence(read_count, streak_every) {
                let lead = format!(
                    "That's {read_count} full file reads this session in an indexed project"
                );
                return allow_with_reason(&root, "hook:read-streak", &rel, &advisory(&lead, &rel));
            }
            Ok(())
        }
        Decision::Advise => {
            let lead = if reread {
                format!(
                    "You already read {rel} in full this session — {size_desc} \
                     still in your context"
                )
            } else {
                format!("{rel} is {size_desc}")
            };
            allow_with_reason(&root, "hook:read-advise", &rel, &advisory(&lead, &rel))
        }
        Decision::Redirect => {
            // refresh a stale index entry so line ranges we point at are correct
            if indexer::is_stale(&root, &conn, &rel) {
                let _ = indexer::reindex_file(&root, &conn, &rel);
            }
            let reason = format!(
                "{rel} is {size_desc}. cona can take you straight to \
                 the right spot for a fraction of the tokens: `cona outline {rel}` lists \
                 every symbol with its line range, then `cona show <Symbol>` prints only \
                 those lines. To understand a symbol (its body + what it calls + who calls it) \
                 in ONE call, prefer `cona context <Symbol>`; before changing one, \
                 `cona impact <Symbol>` shows its blast radius. (Also `cona find <Name>` \
                 / `cona refs <Name>`.) If you genuinely need the whole file, re-issue Read \
                 with an explicit offset/limit."
            );
            deny(&root, "hook:read-block", &rel, &reason)
        }
        Decision::Nudge => {
            // Fresh repo, large code file — indexing unlocks the fast path.
            // One hint per session so it never nags on subsequent reads.
            if !nudge_once(&root) {
                return Ok(());
            }
            let reason = format!(
                "This repo isn't cona-indexed yet. `cona index` (~1s) then \
                 `cona tree --rank` orients you, and `cona outline {rel}` / \
                 `cona show <Symbol>` read one symbol instead of all {lines} lines \
                 (~{tokens} tokens). Reading the whole file is fine for now."
            );
            allow_with_reason(&root, "hook:read-nudge", &rel, &reason)
        }
    }
}

fn try_grep(v: &serde_json::Value) -> Result<()> {
    let input = &v["tool_input"];
    let Some(pattern) = input["pattern"].as_str() else {
        return Ok(());
    };
    let path = input["path"].as_str();
    // any narrowing signal = surgical, pass through untouched
    let surgical = !input["glob"].is_null()
        || !input["type"].is_null()
        || !input["head_limit"].is_null()
        || path.map(|p| Path::new(p).is_file()).unwrap_or(false);
    // cheap gates first — only a broad identifier search pays for the DB check
    if surgical || !lang::is_valid_ident(pattern) {
        return Ok(());
    }

    let start = path
        .map(PathBuf::from)
        .or_else(|| v["cwd"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let root = db::git_root_from(&start);

    let facts = GrepFacts {
        surgical,
        identifier: true,
        // only projects the user already indexed — never create a DB from a hook
        indexed_project: db::has_index(&root),
        in_repo: root.join(".git").exists(),
    };
    match decide_grep(&facts) {
        // decide_grep has no advisory tier: a broad identifier grep is either
        // servable semantically (Redirect) or it isn't. Treated as Allow so the
        // hook stays fail-open if that ever changes.
        Decision::Allow | Decision::Advise => Ok(()),
        Decision::Redirect => {
            let reason = format!(
                "this project is cona-indexed — `cona grep {pattern}` searches code only \
                 and labels every hit with its enclosing symbol, and `cona refs {pattern}` \
                 gives semantic usage sites (strings/comments never match). cona grep also \
                 does regex: `cona grep <pattern> --regex`. If you need to search \
                 non-code files too, re-issue Grep with a glob, type, path or head_limit \
                 filter."
            );
            deny(&root, "hook:grep-block", pattern, &reason)
        }
        Decision::Nudge => {
            if !nudge_once(&root) {
                return Ok(());
            }
            let reason = format!(
                "This repo isn't cona-indexed yet. `cona index` then \
                 `cona grep {pattern}` searches code only (skips strings/comments/other \
                 files) and labels each hit with its enclosing symbol; `cona refs {pattern}` \
                 gives semantic usage sites. This Grep runs as-is."
            );
            allow_with_reason(&root, "hook:grep-nudge", pattern, &reason)
        }
    }
}

/// Emit the PreToolUse deny decision and count the intercept. Credits no
/// tokens — the follow-up cona query logs the actual savings, crediting
/// the redirect too would count the same avoided read twice.
fn deny(root: &Path, cmd: &str, target: &str, reason: &str) -> Result<()> {
    db::log_usage_detail(root, cmd, 0, 1, 0, 0, target);
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

/// Let the tool call proceed but attach a hint the agent sees — a nudge, not
/// a block. Deliberately NO permissionDecision: emitting "allow" would bypass
/// the permission system (silently auto-approving e.g. an out-of-workspace
/// read); additionalContext leaves the permission flow untouched.
fn allow_with_reason(root: &Path, cmd: &str, target: &str, reason: &str) -> Result<()> {
    db::log_usage_detail(root, cmd, 0, 1, 0, 0, target);
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": reason,
        }
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

/// True at most once per (project, session): the first time we consider nudging
/// a fresh repo. A marker file under the data dir, stamped with the current
/// session id, makes the hint fire once and then stay quiet for the rest of the
/// session — subsequent large reads in the same unindexed repo pass silently.
///
/// Session identity comes from `CLAUDE_SESSION_ID` when the agent exports it;
/// without it we fall back to a per-day key so a long-lived shell still only
/// nags occasionally rather than on every read.
fn nudge_once(root: &Path) -> bool {
    let Some(marker) = session_marker_path(root, "nudged") else {
        return true; // can't track → don't suppress the (useful) first hint
    };
    if marker.exists() {
        return false;
    }
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // create the marker (best-effort); either way we nudge this once
    let _ = std::fs::write(&marker, b"");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> ReadFacts {
        ReadFacts {
            partial: false,
            is_code: true,
            indexed: true,
            in_repo: true,
            lines: 800,
            max_lines: 300,
            advise_min_lines: 120,
            callable: true,
            reread: false,
        }
    }

    #[test]
    fn redirects_full_read_of_large_indexed_code_file() {
        assert_eq!(decide_read(&facts()), Decision::Redirect);
    }

    #[test]
    fn allows_partial_read() {
        assert_eq!(
            decide_read(&ReadFacts {
                partial: true,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn allows_small_file() {
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 42,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn allows_non_code_file() {
        assert_eq!(
            decide_read(&ReadFacts {
                is_code: false,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn nudges_large_unindexed_code_file_in_repo() {
        assert_eq!(
            decide_read(&ReadFacts {
                indexed: false,
                ..facts()
            }),
            Decision::Nudge
        );
    }

    #[test]
    fn allows_large_unindexed_file_outside_any_repo() {
        assert_eq!(
            decide_read(&ReadFacts {
                indexed: false,
                in_repo: false,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn never_redirects_exactly_at_threshold() {
        // Exactly at max_lines must not block. With the advise tier enabled it
        // lands in the advisory band (300 >= 120), which still allows the read.
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 300,
                ..facts()
            }),
            Decision::Advise
        );
        // With the advise tier off it is a plain Allow.
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 300,
                advise_min_lines: 0,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn advises_midsize_indexed_file() {
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 216,
                ..facts()
            }),
            Decision::Advise
        );
    }

    #[test]
    fn advises_exactly_at_advise_floor() {
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 120,
                ..facts()
            }),
            Decision::Advise
        );
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 119,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn advise_tier_disabled_at_zero() {
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 216,
                advise_min_lines: 0,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn advises_reread_of_any_size() {
        // Size-blind: the bytes are already in context.
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 12,
                reread: true,
                ..facts()
            }),
            Decision::Advise
        );
    }

    #[test]
    fn reread_still_redirects_when_large() {
        // Redirect outranks the advisory tier — the cheaper path is actionable.
        assert_eq!(
            decide_read(&ReadFacts {
                reread: true,
                ..facts()
            }),
            Decision::Redirect
        );
    }

    #[test]
    fn advisory_tiers_need_an_index() {
        // Nothing to point at in an unindexed project: stay silent rather than
        // advertising commands that would not work yet.
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 216,
                indexed: false,
                ..facts()
            }),
            Decision::Allow
        );
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 12,
                reread: true,
                indexed: false,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn partial_reread_is_untouched() {
        // An explicit offset/limit is the surgical path we asked for — never
        // second-guess it, even on a repeat visit.
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 216,
                partial: true,
                reread: true,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn no_advice_for_prose_or_data_files() {
        // "read one function instead" is meaningless for a README or a config.
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 216,
                callable: false,
                ..facts()
            }),
            Decision::Allow
        );
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 12,
                callable: false,
                reread: true,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn huge_prose_file_still_redirects() {
        // The advisory tier is gated on `callable`, but size-based Redirect is
        // not: outline/show are still the cheap way into a 5k-line changelog.
        assert_eq!(
            decide_read(&ReadFacts {
                callable: false,
                ..facts()
            }),
            Decision::Redirect
        );
    }

    #[test]
    fn callable_languages_classified() {
        for l in ["rust", "typescript", "tsx", "sql", "hcl", "swift", "perl"] {
            assert!(lang::has_callable_symbols(l), "{l} should be advisable");
        }
        for l in ["markdown", "json", "yaml", "toml", "xml", "html", "css"] {
            assert!(!lang::has_callable_symbols(l), "{l} should stay quiet");
        }
    }

    /// Every deny-list entry must be a language `detect_lang` can actually
    /// return, or it is dead weight pretending to cover a file type.
    #[test]
    fn non_callable_languages_are_reachable() {
        for (path, lang) in [
            ("a.md", "markdown"),
            ("a.json", "json"),
            ("a.yaml", "yaml"),
            ("a.toml", "toml"),
            ("a.xml", "xml"),
            ("a.html", "html"),
            ("a.css", "css"),
            ("a.graphql", "graphql"),
        ] {
            assert_eq!(lang::detect_lang(path), Some(lang), "{path}");
            assert!(!lang::has_callable_symbols(lang), "{lang}");
        }
    }

    #[test]
    fn cadence_fires_on_multiples_only() {
        assert!(!fires_on_cadence(1, 30));
        assert!(!fires_on_cadence(29, 30));
        assert!(fires_on_cadence(30, 30));
        assert!(!fires_on_cadence(31, 30));
        assert!(fires_on_cadence(60, 30));
    }

    #[test]
    fn renudge_disabled_at_zero() {
        assert!(!fires_on_cadence(0, 30));
        assert!(!fires_on_cadence(30, 0));
    }

    fn grep_facts() -> GrepFacts {
        GrepFacts {
            surgical: false,
            identifier: true,
            indexed_project: true,
            in_repo: true,
        }
    }

    #[test]
    fn redirects_broad_identifier_grep_in_indexed_project() {
        assert_eq!(decide_grep(&grep_facts()), Decision::Redirect);
    }

    #[test]
    fn allows_surgical_grep() {
        assert_eq!(
            decide_grep(&GrepFacts {
                surgical: true,
                ..grep_facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn allows_regex_pattern() {
        assert_eq!(
            decide_grep(&GrepFacts {
                identifier: false,
                ..grep_facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn nudges_broad_grep_in_unindexed_repo() {
        assert_eq!(
            decide_grep(&GrepFacts {
                indexed_project: false,
                ..grep_facts()
            }),
            Decision::Nudge
        );
    }

    #[test]
    fn allows_broad_grep_outside_any_repo() {
        assert_eq!(
            decide_grep(&GrepFacts {
                indexed_project: false,
                in_repo: false,
                ..grep_facts()
            }),
            Decision::Allow
        );
    }
}
