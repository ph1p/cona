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
use std::sync::LazyLock;

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

/// What a shell command turns out to be, once normalized. Harnesses that run
/// every file operation through a shell tool (Codex: `tool_name = "Bash"`,
/// `tool_input.command = "sed -n '1,240p' main.rs"`) never emit a `Read`/`Grep`
/// tool call, so without this the whole PreToolUse tier is dead there.
///
/// Deliberately narrow. Anything not recognised is `Other` and passes through —
/// this hook may never block work it does not fully understand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellIntent {
    /// A read of `path` starting at line 1. `upto` is the last line the command
    /// asks for (`None` = to the end, as with `cat` or `sed -n '1,$p'`).
    ///
    /// A numeric bound is NOT automatically a partial read: `sed -n '1,240p'` is
    /// exactly how an agent spells "show me the file" — it picks a bound it
    /// expects to exceed the length. The caller compares `upto` against the real
    /// line count and only treats it as partial when the file is genuinely
    /// longer, so a capped read of a longer file still passes through.
    Read { path: String, upto: Option<i64> },
    /// A read the agent already narrowed (line range, `head -n`, …) — the
    /// shell-side equivalent of Read with offset/limit. Never intercepted, but
    /// distinguished from `Other` so the intent is explicit.
    PartialRead,
    /// A broad content search for `pattern` under an optional path.
    Grep {
        pattern: String,
        path: Option<String>,
    },
    /// Not a read or a search we recognise.
    Other,
}

/// Split ONE simple command into words, honouring single/double quotes.
/// Returns `None` on anything that makes the words untrustworthy: an
/// unterminated quote, a redirect, a substitution (`$(`, backticks) or a
/// backslash escape. Chaining operators are handled by `split_segments` before
/// this ever runs, so reaching one here is also a bail.
pub fn shell_words(cmd: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut had = false;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    had = true;
                }
                ';' | '|' | '&' | '>' | '<' | '`' | '\n' => return None,
                '$' if chars.peek() == Some(&'(') => return None,
                '\\' => return None,
                c if c.is_whitespace() => {
                    if !cur.is_empty() || had {
                        words.push(std::mem::take(&mut cur));
                        had = false;
                    }
                }
                c => cur.push(c),
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    if !cur.is_empty() || had {
        words.push(cur);
    }
    Some(words)
}

/// Split a command line on the chaining operators `&&`, `||`, `;` and `|`,
/// respecting quotes. Compound commands are the NORM in a shell-tool harness
/// (`wc -l f && sed -n '1,500p' f`), so refusing them outright would leave the
/// intercept dead; instead each segment is classified on its own and the caller
/// only acts when every one of them is a read.
///
/// `None` when quoting is unbalanced — we cannot tell where a segment ends.
pub fn split_segments(cmd: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                ';' | '\n' => out.push(std::mem::take(&mut cur)),
                '&' | '|' => {
                    // `&&`/`||` collapse to one separator; a bare `&`
                    // (background) or `|` (pipe) separates just the same.
                    if chars.peek() == Some(&c) {
                        chars.next();
                    }
                    out.push(std::mem::take(&mut cur));
                }
                c => cur.push(c),
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    out.push(cur);
    Some(
        out.into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// Peel a `sh -c "…"` / `bash -lc "…"` / `zsh -lc "…"` wrapper off a command
/// line, returning the inner script. Codex issues every tool call as
/// `/bin/zsh -lc "<script>"`, so without this the classifier only ever sees the
/// shell itself. Returns `None` when the command is not such a wrapper.
pub fn unwrap_shell_wrapper(cmd: &str) -> Option<String> {
    let words = shell_words(cmd)?;
    let (prog, args) = words.split_first()?;
    let prog = Path::new(prog.as_str()).file_name()?.to_string_lossy();
    if !matches!(prog.as_ref(), "sh" | "bash" | "zsh" | "dash" | "ksh") {
        return None;
    }
    // The script follows the flag bundle that contains `c` (`-c`, `-lc`, …);
    // anything after it is `$0`/positional args and irrelevant here.
    let mut it = args.iter();
    it.find(|a| a.starts_with('-') && a.contains('c'))?;
    it.next().cloned()
}

/// Classify a whole command line, wrapper and chaining included.
///
/// A line is a read/search only when EVERY segment is one — a single
/// unrecognised segment (an edit, a build, a `rm`) makes the whole line
/// `Other`, because blocking it would block that segment too. Among the
/// recognised segments the strongest intent wins: a full `Read` outranks a
/// `Grep`, which outranks a `PartialRead`, so `wc -l f && sed -n '1,500p' f`
/// is judged on the read.
pub fn classify_shell(cmd: &str) -> ShellIntent {
    let inner = unwrap_shell_wrapper(cmd);
    let line = inner.as_deref().unwrap_or(cmd);
    let Some(segments) = split_segments(line) else {
        return ShellIntent::Other;
    };
    let mut best = ShellIntent::Other;
    for seg in &segments {
        match classify_command(seg) {
            // One segment we don't understand poisons the whole line.
            ShellIntent::Other => return ShellIntent::Other,
            intent => {
                if rank(&intent) > rank(&best) {
                    best = intent;
                }
            }
        }
    }
    best
}

/// Precedence among recognised intents (see `classify_shell`).
fn rank(i: &ShellIntent) -> u8 {
    match i {
        ShellIntent::Other => 0,
        ShellIntent::PartialRead => 1,
        ShellIntent::Grep { .. } => 2,
        ShellIntent::Read { .. } => 3,
    }
}

/// Classify ONE simple command (no chaining, no wrapper). Pure and unit-tested:
/// the risky half (what does this command *do*) is decided by testable code,
/// and the decision half is the one already shared with the native Read/Grep
/// path.
pub fn classify_command(cmd: &str) -> ShellIntent {
    let Some(words) = shell_words(cmd) else {
        return ShellIntent::Other;
    };
    // Skip leading `VAR=value` assignments (`LC_ALL=C grep …`) — they change the
    // environment, not what the command does.
    let start = words
        .iter()
        .position(|w| {
            !w.split_once('=')
                .is_some_and(|(k, _)| !k.is_empty() && !k.starts_with('-'))
        })
        .unwrap_or(words.len());
    let Some((prog, args)) = words[start..].split_first() else {
        return ShellIntent::Other;
    };
    // `/bin/cat` and `cat` are the same program.
    let prog = Path::new(prog.as_str())
        .file_name()
        .map_or_else(|| prog.as_str().into(), |s| s.to_string_lossy());

    match prog.as_ref() {
        // Whole-file dumps. Exactly one operand and no flags = a full read.
        "cat" | "bat" | "less" | "more" => match args {
            [one] if !one.starts_with('-') => ShellIntent::Read {
                path: one.clone(),
                upto: None,
            },
            _ => ShellIntent::Other,
        },
        // head/tail are line-bounded by definition — always partial.
        "head" | "tail" => ShellIntent::PartialRead,
        // Metadata probes: they pull no file content into context, and an agent
        // routinely pairs one with the read it is about to do (`wc -l f &&
        // sed -n '1,500p' f`). Treated as harmless company so they cannot
        // poison an otherwise-recognised line.
        "wc" | "ls" | "pwd" | "file" | "stat" | "basename" | "dirname" | "echo" => {
            ShellIntent::PartialRead
        }
        // `sed -n '<range>p' FILE`. A range that starts past line 1 or stops
        // early is a partial read; `1,$p` / `1,99999p` over a shorter file is
        // how agents spell "read it all", so those fall through to Read.
        "sed" => classify_sed(args),
        "rg" | "grep" | "ag" | "ack" => classify_grep(args),
        _ => ShellIntent::Other,
    }
}

/// `sed -n '1,240p' FILE` → the read half of `classify_shell`. Only the
/// print-range idiom is understood; every other sed script is `Other` (it may
/// be an edit, and we must not touch it).
fn classify_sed(args: &[String]) -> ShellIntent {
    let mut script: Option<&str> = None;
    let mut files: Vec<&String> = Vec::new();
    let mut quiet = false;
    for a in args {
        if a == "-n" || a == "--quiet" || a == "--silent" {
            quiet = true;
        } else if a.starts_with('-') {
            return ShellIntent::Other; // -i, -e, -E … not ours
        } else if script.is_none() {
            script = Some(a);
        } else {
            files.push(a);
        }
    }
    let (Some(script), [file]) = (script, files.as_slice()) else {
        return ShellIntent::Other;
    };
    if !quiet {
        return ShellIntent::Other;
    }
    let Some(range) = script.strip_suffix('p') else {
        return ShellIntent::Other;
    };
    let (start, end) = match range.split_once(',') {
        Some((s, e)) => (s, e),
        // A single-line script (`sed -n '5p'`) is as partial as it gets.
        None => return ShellIntent::PartialRead,
    };
    // Only a read that starts at line 1 can be a full read; `sed -n '40,80p'`
    // is the agent already narrowing.
    if start.trim() != "1" {
        return ShellIntent::PartialRead;
    }
    match end.trim() {
        "$" => ShellIntent::Read {
            path: (*file).clone(),
            upto: None,
        },
        n => match n.parse::<i64>() {
            Ok(n) if n > 0 => ShellIntent::Read {
                path: (*file).clone(),
                upto: Some(n),
            },
            _ => ShellIntent::Other,
        },
    }
}

/// `rg PATTERN [PATH]` → the grep half of `classify_shell`. Any narrowing flag
/// (`-g`, `-t`, `--files`, `-m`, …) makes it surgical and therefore `Other`;
/// only a bare broad search is a candidate for the semantic redirect.
fn classify_grep(args: &[String]) -> ShellIntent {
    let mut positional: Vec<&String> = Vec::new();
    for a in args {
        if let Some(flag) = a.strip_prefix('-') {
            // `-r`/`-R`/`-n`/`-i` and friends only affect presentation or
            // recursion; anything else narrows the search or changes its shape
            // (a file list, a count, a context window) and is not ours.
            let plain = flag.trim_start_matches('-');
            let harmless = matches!(plain, "r" | "R" | "n" | "i" | "rn" | "nr" | "ri" | "ir")
                || plain.is_empty();
            if !harmless {
                return ShellIntent::Other;
            }
        } else {
            positional.push(a);
        }
    }
    // A bare pattern searches the cwd (rg) or is a grep without a path — both
    // are the broad search this tier wants. A second operand is the directory.
    let (pattern, path) = match positional.as_slice() {
        [p] => (p, None),
        [p, dir] => (p, Some((*dir).clone())),
        _ => return ShellIntent::Other,
    };
    ShellIntent::Grep {
        pattern: (*pattern).clone(),
        path,
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
            try_grep(&v, pattern, path, surgical)
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
        ShellIntent::Grep { pattern, path } => {
            let surgical = path.as_deref().map(|p| Path::new(p).is_file()) == Some(true);
            try_grep(v, &pattern, path.as_deref(), surgical)
        }
        ShellIntent::PartialRead | ShellIntent::Other => Ok(()),
    }
}

fn renudge_every() -> i64 {
    env_i64("CONA_RENUDGE_EVERY", DEFAULT_RENUDGE_EVERY, 0)
}

/// Session identity for the per-session markers, in preference order:
/// `CLAUDE_SESSION_ID` (exported by some harnesses), then the `session_id`
/// carried in the payload itself (Codex sends one and exports nothing), then a
/// per-day key so a long-lived shell buckets by day rather than churning a new
/// marker every call.
fn session_id(v: &serde_json::Value) -> String {
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
///
/// Both session-scoped hook mechanisms (`nudge_due`, `tick_toolcall`) share
/// this so the identity rule stays in ONE place — they only differ in `<kind>`
/// and what they store. `None` when the data dir is unavailable; each caller
/// picks its own fallback.
fn session_marker_path(root: &Path, kind: &str, session: &str) -> Option<PathBuf> {
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

/// Look up this (project, session)'s read log WITHOUT writing: whether `rel`
/// was already fully read, and how many *counted* reads it holds so far. One
/// path per line under the `reads` marker kind; a line prefixed with a tab is
/// a read that already carried an advisory — it marks the path as seen (the
/// bytes DID land in context) but does not count toward the volume streak,
/// otherwise every advised read would drag the next streak reminder closer
/// and the agent would be nagged twice for one mistake.
///
/// Best-effort like every other hook side effect: if the data dir is
/// unavailable we report "not a re-read" and stay silent rather than guessing.
fn peek_reads(root: &Path, rel: &str, session: &str) -> (bool, i64) {
    let Some(log) = session_marker_path(root, "reads", session) else {
        return (false, 0);
    };
    if !log.exists() {
        // First touch of this session: prune markers whose sessions are over.
        if let Some(parent) = log.parent() {
            let _ = std::fs::create_dir_all(parent);
            prune_marker_dir(parent);
        }
        return (false, 0);
    }
    let (mut seen, mut count) = (false, 0i64);
    for line in std::fs::read_to_string(&log).unwrap_or_default().lines() {
        seen |= line.strip_prefix('\t').unwrap_or(line) == rel;
        count += i64::from(!line.starts_with('\t'));
    }
    (seen, count)
}

/// Append one read that actually went through to the session's read log.
/// `counted` = the read carried no advisory (see `peek_reads`). A DENIED read
/// is never recorded — the bytes never reached the agent, so a retry must not
/// look like a re-read.
fn record_read(root: &Path, rel: &str, session: &str, counted: bool) {
    let Some(log) = session_marker_path(root, "reads", session) else {
        return;
    };
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let prefix = if counted { "" } else { "\t" };
    // Append rather than rewrite: the log grows for the whole session, and a
    // failed write only costs us this call's bookkeeping.
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .and_then(|mut f| f.write_all(format!("{prefix}{rel}\n").as_bytes()));
}

/// Record that a full read of `rel` was redirected (denied) this session and
/// report whether it already had been. A SECOND full-read attempt after a
/// block means the agent weighed the pointers and still wants the file —
/// denying again with the identical message is a loop, not guidance, so the
/// caller lets that attempt through. Best-effort like every marker: no data
/// dir → "not denied yet", which degrades to the pre-existing always-deny.
fn note_denied(root: &Path, rel: &str, session: &str) -> bool {
    let Some(log) = session_marker_path(root, "denied", session) else {
        return false;
    };
    let seen = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .any(|l| l == rel);
    if !seen {
        if let Some(parent) = log.parent() {
            let _ = std::fs::create_dir_all(parent);
            if !log.exists() {
                prune_marker_dir(parent);
            }
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .and_then(|mut f| f.write_all(format!("{rel}\n").as_bytes()));
    }
    seen
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

/// Increment and return the per-(project, session) tool-call counter. A tiny
/// file under the data dir holds the running count (path via
/// `session_marker_path`, so the session-identity rule is shared with
/// `nudge_due`). Best-effort: any IO failure returns 0 so the caller simply
/// doesn't re-nudge this call.
fn tick_toolcall(root: &Path, session: &str) -> i64 {
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
            // when we measured it (an unmeasured `lines` is a floor, not a count).
            let chunk_hint = if measured {
                let mid = (lines / 2).max(1);
                format!(
                    "read it in ranges that stop short of line {lines} — Read \
                     offset/limit, or `sed -n '1,{mid}p'` then `sed -n '{},$p'`",
                    mid + 1
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
fn try_grep(
    v: &serde_json::Value,
    pattern: &str,
    path: Option<&str>,
    surgical: bool,
) -> Result<()> {
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

/// Whether the "this repo isn't indexed" hint is due. Fires on the FIRST
/// nudge-eligible event of a (project, session), then again after every
/// `CONA_NUDGE_EVERY` suppressed ones (default 10, 0 = never repeat) — a hint
/// dropped once at the start of a long session is otherwise gone for good.
/// The marker file holds the running event count.
///
/// Session identity comes from `CLAUDE_SESSION_ID` when the agent exports it;
/// without it we fall back to a per-day key so a long-lived shell still only
/// nags occasionally rather than on every read.
fn nudge_due(root: &Path, session: &str) -> bool {
    let Some(marker) = session_marker_path(root, "nudged", session) else {
        return true; // can't track → don't suppress the (useful) first hint
    };
    if !marker.exists() {
        if let Some(parent) = marker.parent() {
            let _ = std::fs::create_dir_all(parent);
            prune_marker_dir(parent);
        }
    }
    let count = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
        + 1;
    // best-effort, like every marker write
    let _ = std::fs::write(&marker, count.to_string());
    count == 1 || fires_on_cadence(count - 1, env_i64("CONA_NUDGE_EVERY", DEFAULT_NUDGE_EVERY, 0))
}

/// Delete session markers old enough that their session is certainly over
/// (7 days — the per-day fallback key rolls daily, real session ids within a
/// week are plausibly live). Called only when a NEW session touches the dir,
/// so steady-state hook calls pay no directory scan. Best-effort throughout.
fn prune_marker_dir(dir: &Path) {
    const WEEK: u64 = 7 * 86_400;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > WEEK);
        if stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
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

    // ---- shell-command normalization (harnesses whose only file tool is a
    // shell: Codex sends `tool_name = "Bash"` with a command line) ----

    fn read_of(cmd: &str) -> Option<(String, Option<i64>)> {
        match classify_shell(cmd) {
            ShellIntent::Read { path, upto } => Some((path, upto)),
            _ => None,
        }
    }

    #[test]
    fn shell_words_splits_quotes() {
        assert_eq!(
            shell_words("sed -n '1,240p' main.rs").unwrap(),
            vec!["sed", "-n", "1,240p", "main.rs"]
        );
        assert_eq!(
            shell_words("cat \"my file.rs\"").unwrap(),
            vec!["cat", "my file.rs"]
        );
    }

    #[test]
    fn shell_words_refuses_untrustworthy_commands() {
        for cmd in [
            "cat a.rs > out",
            "cat $(ls)",
            "cat `ls`",
            "cat 'unterminated",
            "cat a\\ b.rs",
        ] {
            assert!(shell_words(cmd).is_none(), "should refuse: {cmd}");
        }
    }

    #[test]
    fn splits_chained_commands() {
        assert_eq!(
            split_segments("wc -l f && sed -n '1,500p' f").unwrap(),
            vec!["wc -l f", "sed -n '1,500p' f"]
        );
        assert_eq!(
            split_segments("a; b | c || d").unwrap(),
            vec!["a", "b", "c", "d"]
        );
        // A separator inside quotes is data, not a separator.
        assert_eq!(
            split_segments("grep 'a;b' src").unwrap(),
            vec!["grep 'a;b' src"]
        );
        assert!(split_segments("cat 'oops").is_none());
    }

    #[test]
    fn unwraps_shell_invocations() {
        // The shape Codex actually emits.
        assert_eq!(
            unwrap_shell_wrapper("/bin/zsh -lc \"sed -n '1,240p' big.rs\"").as_deref(),
            Some("sed -n '1,240p' big.rs")
        );
        assert_eq!(
            unwrap_shell_wrapper("bash -c 'cat a.rs'").as_deref(),
            Some("cat a.rs")
        );
        assert_eq!(unwrap_shell_wrapper("cat a.rs"), None);
    }

    #[test]
    fn a_chain_is_judged_on_its_strongest_read() {
        // The real Codex line: a metadata probe next to a whole-file read.
        assert_eq!(
            classify_shell("/bin/zsh -lc \"wc -l big.rs && cat big.rs\""),
            ShellIntent::Read {
                path: "big.rs".into(),
                upto: None
            }
        );
    }

    #[test]
    fn one_unrecognised_segment_passes_the_whole_line() {
        // Blocking this line would block the build too — always fail open.
        assert_eq!(
            classify_shell("cat a.rs && cargo build"),
            ShellIntent::Other
        );
        assert_eq!(classify_shell("cat a.rs | rm -rf x"), ShellIntent::Other);
    }

    #[test]
    fn leading_env_assignments_are_skipped() {
        assert_eq!(
            classify_shell("LC_ALL=C grep -rn UserService src"),
            ShellIntent::Grep {
                pattern: "UserService".into(),
                path: Some("src".into())
            }
        );
    }

    #[test]
    fn classifies_whole_file_dumps() {
        assert_eq!(
            read_of("cat src/main.rs"),
            Some(("src/main.rs".into(), None))
        );
        assert_eq!(
            read_of("/bin/cat src/main.rs"),
            Some(("src/main.rs".into(), None))
        );
        assert_eq!(read_of("sed -n '1,$p' a.rs"), Some(("a.rs".into(), None)));
    }

    #[test]
    fn classifies_bounded_sed_as_a_read_with_a_bound() {
        // The idiom Codex actually emits: a bound the agent expects to exceed
        // the file length. Only the caller (which knows the real line count)
        // can tell that apart from a genuine partial read.
        assert_eq!(
            read_of("sed -n '1,240p' main.rs"),
            Some(("main.rs".into(), Some(240)))
        );
    }

    #[test]
    fn narrowed_shell_reads_are_partial() {
        for cmd in [
            "sed -n '40,80p' a.rs",
            "sed -n '5p' a.rs",
            "head -n 50 a.rs",
            "tail -n 50 a.rs",
        ] {
            assert_eq!(classify_shell(cmd), ShellIntent::PartialRead, "{cmd}");
        }
    }

    #[test]
    fn unrecognised_commands_pass_through() {
        for cmd in [
            "sed -i 's/a/b/' a.rs", // an EDIT — must never be touched
            "cat a.rs b.rs",        // multiple files
            "rm -rf /",
            "cargo test",
            "sed -n '1,240p'", // no file operand
        ] {
            assert_eq!(classify_shell(cmd), ShellIntent::Other, "{cmd}");
        }
    }

    #[test]
    fn classifies_broad_shell_greps() {
        assert_eq!(
            classify_shell("rg UserService"),
            ShellIntent::Grep {
                pattern: "UserService".into(),
                path: None
            }
        );
        assert_eq!(
            classify_shell("grep -rn UserService src"),
            ShellIntent::Grep {
                pattern: "UserService".into(),
                path: Some("src".into())
            }
        );
    }

    #[test]
    fn narrowed_shell_greps_pass_through() {
        // Every one of these narrows the search; only a bare broad search is a
        // candidate for the semantic redirect.
        for cmd in [
            "rg -g '*.rs' UserService",
            "rg --files -g 'AGENTS.md' .",
            "rg -t rust UserService",
            "rg -m 5 UserService",
            "rg -l UserService",
        ] {
            assert_eq!(classify_shell(cmd), ShellIntent::Other, "{cmd}");
        }
    }
}
