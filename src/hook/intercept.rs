//! The two tool-call intercepts (Read and Grep, native or via a shell tool)
//! and the PostToolUse re-nudge: payload parsing, fact gathering, and the
//! emitted decision. The pure policy they consult lives in the parent module.

use super::markers::{note_denied, nudge_due, peek_reads, record_read, session_id, tick_toolcall};
use super::*;
use crate::{db, indexer, lang};
use std::io::Read;
use std::path::{Path, PathBuf};

pub(crate) fn try_pretooluse() -> Result<()> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let v: serde_json::Value = serde_json::from_str(&buf)?;

    match v["tool_name"].as_str() {
        Some("Read") => {
            let input = &v["tool_input"];
            let Some(file_path) = input["file_path"].as_str() else {
                return Ok(());
            };
            let partial = !input["offset"].is_null() || !input["limit"].is_null();
            try_read(&v, file_path, partial, None)
        }
        Some("Grep") => {
            let input = &v["tool_input"];
            let Some(pattern) = input["pattern"].as_str() else {
                return Ok(());
            };
            // any narrowing signal = surgical, pass through untouched
            let path = input["path"].as_str();
            let surgical = !input["glob"].is_null()
                || !input["type"].is_null()
                || !input["head_limit"].is_null()
                || path.map(|p| Path::new(p).is_file()).unwrap_or(false);
            // bounded output (file list, counts, context windows) = the same
            // restraint the shell path reads from -l/-c/-C — advisory tier
            let soft = matches!(
                input["output_mode"].as_str(),
                Some("files_with_matches" | "count")
            ) || !input["-A"].is_null()
                || !input["-B"].is_null()
                || !input["-C"].is_null();
            try_grep(&v, pattern, path, surgical, soft)
        }
        // Harnesses whose only file tool is a shell (Codex runs `sed -n
        // '1,240p' f` / `rg Foo` through `tool_name = "Bash"`) never emit a
        // Read or Grep call. Recover the intent from the command line so the
        // same two intercepts work there; anything unrecognised passes.
        Some(name) if SHELL_TOOLS.contains(&name) => {
            let Some(cmd) = v["tool_input"]["command"].as_str() else {
                return Ok(());
            };
            try_shell(&v, cmd)
        }
        _ => Ok(()),
    }
}

/// Route a shell command through the same two intercepts as native Read/Grep.
/// Fails open on every intent we do not recognise.
fn try_shell(v: &serde_json::Value, cmd: &str) -> Result<()> {
    match classify_shell(cmd) {
        ShellIntent::Read { path, upto } => try_read(v, &path, false, upto),
        ShellIntent::Grep {
            pattern,
            path,
            soft,
        } => {
            let surgical = path.as_deref().map(|p| Path::new(p).is_file()) == Some(true);
            try_grep(v, &pattern, path.as_deref(), surgical, soft)
        }
        ShellIntent::PartialRead | ShellIntent::Other => Ok(()),
    }
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
pub(crate) fn try_posttooluse() -> Result<()> {
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
    let count = tick_toolcall(&root, &session_id(&v));
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

/// The shared read intercept. `partial` is the caller's "the agent already
/// narrowed this" signal; `upto` is a shell-side upper line bound (see
/// `ShellIntent::Read`) that only counts as narrowing once we know the file is
/// actually longer than it.
fn try_read(
    v: &serde_json::Value,
    file_path: &str,
    partial: bool,
    upto: Option<i64>,
) -> Result<()> {
    // A relative path (`sed -n '1,240p' main.rs`) resolves against the tool
    // call's cwd, not ours — the hook runs wherever the harness launched it.
    let cwd = v["cwd"].as_str().map(PathBuf::from);
    let file_abs = match (Path::new(file_path).is_absolute(), &cwd) {
        (false, Some(c)) => c.join(file_path),
        _ => PathBuf::from(file_path),
    };
    let dir = file_abs.parent().unwrap_or(Path::new("."));
    // prefer the git root the agent is working in
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
    // A shell-side upper bound only narrows the read if the file actually runs
    // past it. `sed -n '1,240p'` over a 30-line file read the whole thing.
    if upto.is_some_and(|n| n < lines) {
        return Ok(());
    }
    // Never print a line count we did not actually measure.
    let size_desc = if measured {
        format!("{lines} lines (~{tokens} tokens)")
    } else {
        format!("~{tokens} tokens")
    };

    // Is it indexed? Only open the DB if one already exists; never create a
    // project DB from a hook (mirrors try_grep's has_index gate). A missing DB
    // is NOT an early exit — the Nudge tier exists precisely for repos with no
    // index yet, so we fall through with indexed=false and let decide_read run.
    let conn = if db::project_db_path(&root).exists() {
        Some(db::open_project_db(&root)?)
    } else {
        None
    };
    let indexed: bool = conn.as_ref().is_some_and(|c| {
        c.query_row("SELECT 1 FROM files WHERE path = ?1", [&rel], |_| Ok(true))
            .unwrap_or(false)
    });

    // Track full reads of indexed source files: re-read detection and the
    // read-volume streak are both size-blind, so this applies even to files
    // far below the advisory floor. Skipped when unindexed (nothing to point at)
    // or non-callable (prose/data — a run of README reads is not the pattern we
    // are looking for, and must not inflate the counter for real source files),
    // and skipped entirely when both tiers that consume it are disabled, so the
    // default hot path pays no marker IO it cannot use. Peek here; each arm
    // records the read only if it actually goes through (a denied read never
    // reached the agent), and marks it uncounted when it carried an advisory.
    let streak_every = read_streak_every();
    let tracking_reads = advise_min_lines > 0 || streak_every > 0;
    let tracking = indexed && callable && tracking_reads;
    let (reread, prior_reads) = if tracking {
        peek_reads(&root, &rel, &session_id(v))
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
            // Always counted — even when the streak reminder fires on this
            // very read, it must advance the counter or the same multiple
            // would re-fire on every subsequent read.
            let read_count = prior_reads + 1;
            if tracking {
                record_read(&root, &rel, &session_id(v), true);
            }
            if tracking && fires_on_cadence(read_count, streak_every) {
                let lead = format!(
                    "That's {read_count} full file reads this session in an indexed project"
                );
                return allow_with_reason(&root, "hook:read-streak", &rel, &advisory(&lead, &rel));
            }
            Ok(())
        }
        Decision::Advise => {
            if tracking {
                record_read(&root, &rel, &session_id(v), false);
            }
            // A re-read gets its own short, imperative message instead of the
            // generic "one function is cheaper" tail — the file is already in
            // context, so the useful advice is different in kind.
            let msg = if reread {
                format!(
                    "Re-read: {rel} ({size_desc}) is already in your context from \
                     earlier this session. Need one part again? `cona show <Symbol>` \
                     re-reads just that symbol. Expect it changed? `cona outline {rel}` \
                     re-maps it first. This read ran as-is."
                )
            } else {
                advisory(&format!("{rel} is {size_desc}"), &rel)
            };
            allow_with_reason(&root, "hook:read-advise", &rel, &msg)
        }
        Decision::Redirect => {
            // refresh a stale index entry so line ranges we point at are correct
            // (Redirect implies indexed, so the connection is always present here)
            if let Some(conn) = &conn {
                if indexer::is_stale(&root, conn, &rel) {
                    let _ = indexer::reindex_file(&root, conn, &rel);
                }
            }
            // A second full-read attempt after a block yields: the loop where
            // following "read it in chunks" (or plain stubbornness) meets the
            // same wall with the same words forever must not exist.
            if note_denied(&root, &rel, &session_id(v)) {
                if tracking {
                    // this read goes through — seen, but advised, so uncounted
                    record_read(&root, &rel, &session_id(v), false);
                }
                let lead = format!(
                    "You retried the full read of {rel} ({size_desc}) after a \
                     redirect, so it went through"
                );
                return allow_with_reason(&root, "hook:read-advise", &rel, &advisory(&lead, &rel));
            }
            // Only promise a chunked escape we can honour: a range is partial
            // when it ends BEFORE the last line, so name that bound concretely
            // when we measured it (an unmeasured `lines` is a floor, not a
            // count). State the rule but never pre-compute the split — a
            // copy-paste recipe turns the redirect into a two-call full read
            // for agents that never needed every line.
            let chunk_hint = if measured {
                format!(
                    "read it in bounded ranges (Read offset/limit, or `sed -n` \
                     ranges) that stop short of line {lines}"
                )
            } else {
                "read it in bounded ranges (Read offset/limit, or `sed -n` \
                 ranges that stop short of the end)"
                    .to_string()
            };
            let reason = format!(
                "{rel} is {size_desc}. cona can take you straight to \
                 the right spot for a fraction of the tokens: `cona outline {rel}` lists \
                 every symbol with its line range, then `cona show <Symbol>` prints only \
                 those lines. To understand a symbol (its body + what it calls + who calls it) \
                 in ONE call, prefer `cona context <Symbol>`; before changing one, \
                 `cona impact <Symbol>` shows its blast radius. (Also `cona find <Name>` \
                 / `cona refs <Name>`.) If you genuinely need the whole file, {chunk_hint}."
            );
            deny(&root, "hook:read-block", &rel, &reason)
        }
        Decision::Nudge => {
            // Fresh repo, large code file — indexing unlocks the fast path.
            // One hint per session so it never nags on subsequent reads.
            if !nudge_due(&root, &session_id(v)) {
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

/// The shared grep intercept. `surgical` is the caller's "already narrowed"
/// signal — the native path derives it from glob/type/head_limit, the shell
/// path from the command's own flags.
/// Where a grep starts searching: an absolute path argument wins; a relative
/// one (`grep -rn foo src/`, `rg foo .`) resolves against the tool call's cwd
/// — the hook runs wherever the harness launched it, same rule as try_read.
/// Without the join, `src/` resolved against the HOOK's own cwd, walked to a
/// relative "root" whose hash matches no project DB, and a fully-indexed repo
/// answered Nudge ("isn't indexed yet") instead of the redirect.
pub(crate) fn grep_start(path: Option<&str>, cwd: Option<&str>) -> PathBuf {
    match (path, cwd) {
        (Some(p), Some(c)) if !Path::new(p).is_absolute() => Path::new(c).join(p),
        (Some(p), _) => PathBuf::from(p),
        (None, Some(c)) => PathBuf::from(c),
        (None, None) => PathBuf::from("."),
    }
}

fn try_grep(
    v: &serde_json::Value,
    pattern: &str,
    path: Option<&str>,
    surgical: bool,
    soft: bool,
) -> Result<()> {
    // cheap gates first — only a broad identifier search pays for the DB check
    if surgical || !lang::is_valid_ident(pattern) {
        return Ok(());
    }

    let start = grep_start(path, v["cwd"].as_str());
    let root = db::git_root_from(&start);

    let facts = GrepFacts {
        surgical,
        identifier: true,
        // only projects the user already indexed — never create a DB from a hook
        indexed_project: db::has_index(&root),
        in_repo: root.join(".git").exists(),
        soft,
    };
    match decide_grep(&facts) {
        Decision::Allow => Ok(()),
        Decision::Advise => {
            // Output already bounded (-l/-c/context) — respect the restraint,
            // let it run, and point at the semantic equivalent.
            let reason = format!(
                "this project is cona-indexed — `cona grep {pattern}` searches code only \
                 and labels every hit with its enclosing symbol; `cona refs {pattern}` \
                 gives semantic usage sites (strings/comments never match). This search \
                 ran as-is."
            );
            allow_with_reason(&root, "hook:grep-advise", pattern, &reason)
        }
        Decision::Redirect => {
            let reason = format!(
                "this project is cona-indexed — `cona grep {pattern}` searches code only \
                 and labels every hit with its enclosing symbol, and `cona refs {pattern}` \
                 gives semantic usage sites (strings/comments never match). cona grep also \
                 does regex: `cona grep <pattern> --regex`. If you need to search \
                 non-code files too, re-issue the search narrowed to a glob, type, single \
                 file or result limit."
            );
            deny(&root, "hook:grep-block", pattern, &reason)
        }
        Decision::Nudge => {
            if !nudge_due(&root, &session_id(v)) {
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
