//! Agent tool-call hooks.
//!
//! cona is a *navigation accelerator*, never a gatekeeper. The PreToolUse hook
//! only redirects toward a faster path and always fails open — parse errors,
//! missing index, non-code/small files and partial reads pass untouched.
//!
//! A large full read / broad identifier grep in an INDEXED project is
//! *redirected* (blocked, with the cona command in the reason). The same call
//! in an unindexed git repo is *nudged* (allowed, with a one-time hint that
//! indexing unlocks cona) — the cold-open case where orientation matters most.
//!
//! Between "ignore" and "block" sits what actually drains context: a 150–300
//! line file read in full to understand ONE function. Three *advisory*
//! outcomes ALLOW the read and attach a hint, since the read is correct when
//! the agent is about to rewrite the file and only the agent knows which:
//!   - mid-size indexed file (>= `CONA_ADVISE_MIN_LINES`) read in full
//!   - re-read of a path already fully read this session (size-blind: the
//!     bytes are already in context)
//!   - the N-th full read in one session (`CONA_READ_STREAK`) — no
//!     single-call rule sees a run of individually innocent reads

use anyhow::Result;
use std::sync::LazyLock;

mod intercept;
mod markers;
mod shell;
#[cfg(test)]
mod tests;

pub use markers::{file_age_secs, fires_on_cadence, LIVENESS_FILE, MARKER_MAX_AGE_SECS};

/// The one builder for a hint payload: `additionalContext` carries text and
/// decides nothing. Every hint path (advisory, streak, nudge, re-nudge,
/// compaction) emits this shape, differing only in the event name. The
/// `permissionDecision` sibling is deliberately NOT here: the redirect is the
/// only decision cona emits and its single call site should stay visible.
///
/// If serialization somehow fails, an empty string means "no hint" (fail-open).
pub fn additional_context(event: &str, ctx: &str) -> String {
    let payload = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": ctx,
        }
    });
    match serde_json::to_string(&payload) {
        Ok(s) => format!("{s}\n"),
        Err(_) => String::new(),
    }
}
pub use shell::{
    classify_command, classify_shell, shell_words, split_pipeline, split_segments,
    unwrap_shell_wrapper, ShellIntent,
};

/// Tool names that carry a file read or search directly, as their own tool.
const NATIVE_TOOLS: &[&str] = &["Read", "Grep"];

/// Tool names that carry one as a shell command line instead. A harness whose
/// only file tool is a shell (Codex runs `cat f` / `rg Foo` as `Bash`) never
/// emits Read/Grep — `classify_shell` recovers the intent; anything
/// unrecognised passes. An extra name costs a no-op hook run, a missing one
/// costs the whole tier.
const SHELL_TOOLS: &[&str] = &[
    "Bash",
    "Shell",
    "shell",
    "exec",
    "run_command",
    "local_shell",
];

/// The `PreToolUse` matcher admitting exactly the tools `try_pretooluse`
/// dispatches on. Derived from the two lists above so matcher and dispatcher
/// cannot drift. `plugin/hooks/hooks.json` declares the SAME matcher;
/// `plugin_hook_matcher_matches_the_installer` pins them equal.
pub static PRETOOL_MATCHER: LazyLock<String> = LazyLock::new(|| {
    NATIVE_TOOLS
        .iter()
        .chain(SHELL_TOOLS)
        .copied()
        .collect::<Vec<_>>()
        .join("|")
});

/// The `PostToolUse` matcher for the reindex hook — the write tools whose
/// output can invalidate the index. Single source for the installer
/// (install/agents/apply.rs `claude_hooks`) and the plugin copy
/// (`plugin/hooks/hooks.json`, pinned by `plugin_hooks_match_the_installer`).
pub const POSTTOOL_MATCHER: &str = "Edit|Write|MultiEdit|NotebookEdit";

/// Default line threshold above which a full read of an indexed code file is
/// redirected to `cona outline`/`show`. Override with `CONA_READ_MAX_LINES`.
const DEFAULT_MAX_LINES: i64 = 300;

/// Default line threshold above which a full read is *advised* against
/// (allowed, with a hint) rather than redirected. A 150-line file is ~1.5k
/// tokens, and reading it all for ONE function is the most common context
/// waste — but sometimes correct (about to rewrite), so this tier never
/// blocks. Override with `CONA_ADVISE_MIN_LINES`; 0 disables the tier.
const DEFAULT_ADVISE_MIN_LINES: i64 = 120;

/// Default number of full reads in one session+project before the hook points
/// out the pattern: four 200-line reads = ~7k tokens for what a few `show`
/// calls deliver in a few hundred. Override with `CONA_READ_STREAK`; 0 disables.
const DEFAULT_READ_STREAK: i64 = 4;

/// Default number of suppressed nudge-eligible events (large reads / broad
/// greps in an UNINDEXED repo) between repeats of the "not indexed" hint. The
/// first fires immediately; without repeats, a hint dropped early in a long
/// session is gone for good. Override with `CONA_NUDGE_EVERY`; 0 = once per
/// session.
const DEFAULT_NUDGE_EVERY: i64 = 10;

/// Default number of NARROW reads of the SAME file in one session before the
/// hook suggests an outline. Each slice is individually correct and passes;
/// the waste is the pattern (re-paying context while hunting for boundaries).
/// Override with `CONA_PARTIAL_STREAK`; 0 disables.
const DEFAULT_PARTIAL_STREAK: i64 = 3;

/// Default cadence for the periodic re-nudge: OFF. Current models hold the
/// habit from one statement, and the PreToolUse redirect still catches a wrong
/// Read/Grep. Opt in with `CONA_RENUDGE_EVERY=<n>` (tool calls between
/// reminders) on a model that drifts.
const DEFAULT_RENUDGE_EVERY: i64 = 0;

/// What the hook should do about a candidate tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Pass through untouched.
    Allow,
    /// Block and point at the faster cona path (project is indexed, so the
    /// redirect is actionable right now).
    Redirect,
    /// Allow, but surface a one-time hint that indexing would unlock cona
    /// (good cona fit, not yet indexed — the cold-open case).
    Nudge,
    /// Allow, but hint that a symbol-scoped read would be cheaper. Never
    /// blocks: the read may be justified (about to rewrite the file).
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
    /// The language has functions/methods worth reading one at a time. False
    /// for prose/data (Markdown, JSON, YAML…). Gates the advisory tier only; a
    /// huge file still redirects on size alone.
    pub callable: bool,
    /// This exact file was already fully read this session. Size-blind: the
    /// highest-confidence waste signal (the content is already in context).
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
    // Under the redirect threshold: a re-read (any size) or a mid-size file is
    // worth a hint — only when indexed and callable, else there is nothing to
    // point at. `advise_min_lines == 0` turns the whole tier off, re-reads too.
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
    /// The Grep is already narrowed by a glob/type filter or a head_limit —
    /// surgical, cona has nothing better to offer.
    pub surgical: bool,
    /// The search is scoped to ONE file. Never blocks, but it is the strongest
    /// "I want a symbol" signal: grep returns a line number to slice around,
    /// where `show` returns the symbol. Advisory tier.
    pub single_file: bool,
    /// The pattern is a plain identifier cona can serve semantically.
    pub identifier: bool,
    /// The search root is an indexed cona project.
    pub indexed_project: bool,
    /// The search root is inside a git repo (indexable, if not yet indexed).
    pub in_repo: bool,
    /// Broad search with BOUNDED output (file list, counts, context windows) —
    /// already restrained, so a hint instead of a block.
    pub soft: bool,
}

/// Decide what to do with a candidate `Grep`.
/// - broad identifier search over an indexed project → Redirect
///   (Advise instead when the output is already bounded — `soft`)
/// - single-file identifier search over an indexed project → Advise (never
///   blocks: the search is already narrow, but `show`/`refs` beats a line number)
/// - broad identifier search over an UNINDEXED git repo → Nudge
/// - broad literal/regex search over an indexed project → Advise
/// - surgical / non-repo / other literal or regex searches → Allow
pub fn decide_grep(f: &GrepFacts) -> Decision {
    if f.surgical {
        return Decision::Allow;
    }
    // A literal/regex (`dmf-primary-[a-z]*`, `foo.bar`) has no symbol for
    // `refs`, but `cona grep` searches it code-only — a hint in an indexed
    // project, never a block or nudge.
    if !f.identifier {
        return if f.indexed_project && !f.single_file {
            Decision::Advise
        } else {
            Decision::Allow
        };
    }
    if f.indexed_project {
        // `soft` and `single_file` are "already restrained" — inform, never
        // block. A single-file search in an UNINDEXED repo stays Allow: too
        // narrow to justify a whole-project index nudge.
        if f.soft || f.single_file {
            Decision::Advise
        } else {
            Decision::Redirect
        }
    } else if f.in_repo && !f.single_file {
        Decision::Nudge
    } else {
        Decision::Allow
    }
}

/// Read an i64 `CONA_*` knob from the environment; unparseable or below-`min`
/// values fall back to the default instead of silently disabling a tier.
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

fn partial_streak_every() -> i64 {
    env_i64("CONA_PARTIAL_STREAK", DEFAULT_PARTIAL_STREAK, 0)
}

fn renudge_every() -> i64 {
    env_i64("CONA_RENUDGE_EVERY", DEFAULT_RENUDGE_EVERY, 0)
}

/// Entry point for `cona hook <event>`: payload on stdin, decision on stdout.
/// ALWAYS exits 0 — a failure here must never break a tool call.
pub fn run(event: &str) -> Result<()> {
    if std::env::var("CONA_HOOK_DISABLE").is_ok() {
        return Ok(());
    }
    markers::touch_liveness();
    // Any error → do nothing silently.
    match event {
        "PreToolUse" => {
            let _ = intercept::try_pretooluse();
        }
        "PostToolUse" => {
            let _ = intercept::try_posttooluse();
        }
        "PreCompact" => {
            let _ = intercept::try_precompact();
        }
        _ => {}
    }
    Ok(())
}
