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

use anyhow::Result;
use std::sync::LazyLock;

mod intercept;
mod markers;
mod shell;
#[cfg(test)]
mod tests;

pub use markers::{file_age_secs, fires_on_cadence, LIVENESS_FILE, MARKER_MAX_AGE_SECS};
pub use shell::{
    classify_command, classify_shell, shell_words, split_segments, unwrap_shell_wrapper,
    ShellIntent,
};

/// Tool names that carry a file read or search directly, as their own tool.
const NATIVE_TOOLS: &[&str] = &["Read", "Grep"];

/// Tool names that carry one as a shell command line instead. A harness whose
/// ONLY file tool is a shell (Codex runs `cat f` / `rg Foo` as
/// `tool_name: "Bash"`) never emits a Read or Grep call — `classify_shell`
/// recovers the intent from the command line and anything it does not
/// recognise passes. Listing a tool cona then ignores costs one no-op hook
/// run, missing one costs the whole tier.
const SHELL_TOOLS: &[&str] = &[
    "Bash",
    "Shell",
    "shell",
    "exec",
    "run_command",
    "local_shell",
];

/// The `PreToolUse` matcher admitting exactly the tools `try_pretooluse`
/// dispatches on. Derived from the two lists above so the matcher and the
/// dispatcher cannot drift: a name added to one is a name added to both.
///
/// `plugin/hooks/hooks.json` declares the SAME matcher for the plugin
/// distribution path; `plugin_hook_matcher_matches_the_installer` pins them
/// equal.
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

/// Default number of suppressed nudge-eligible events (large reads / broad
/// greps in an UNINDEXED repo) between repeats of the "this repo isn't
/// indexed" hint. The first one fires immediately; without a repeat, a hint
/// dropped early in a long session is gone for good even though `cona index`
/// stays a one-second fix the whole time. Override with `CONA_NUDGE_EVERY`;
/// 0 = fire once per session and never repeat.
const DEFAULT_NUDGE_EVERY: i64 = 10;

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
    /// The search is still broad but its OUTPUT is bounded (file list, counts,
    /// context windows) — the agent already reached for restraint, so it gets
    /// a hint instead of a block.
    pub soft: bool,
}

/// Decide what to do with a candidate `Grep`.
/// - broad identifier search over an indexed project → Redirect
///   (Advise instead when the output is already bounded — `soft`)
/// - broad identifier search over an UNINDEXED git repo → Nudge
/// - surgical / regex / non-repo → Allow
pub fn decide_grep(f: &GrepFacts) -> Decision {
    if f.surgical || !f.identifier {
        return Decision::Allow;
    }
    if f.indexed_project {
        if f.soft {
            Decision::Advise
        } else {
            Decision::Redirect
        }
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

fn renudge_every() -> i64 {
    env_i64("CONA_RENUDGE_EVERY", DEFAULT_RENUDGE_EVERY, 0)
}

/// Entry point for `cona hook <event>`. Reads the Claude hook payload from
/// stdin and prints a decision to stdout. ALWAYS exits 0 — this is a helper for
/// the agent, so a failure here must never break a tool call.
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
        _ => {}
    }
    Ok(())
}
