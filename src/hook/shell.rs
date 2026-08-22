//! Shell-command normalization: recover Read/Grep intent from a command line.

use std::path::Path;

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
    ///
    /// `path` is `Some` only when the command actually pulls a slice of ONE
    /// named file into context, which is what the cross-call slice accounting
    /// counts. Metadata probes (`wc`, `ls`, `stat`, `echo`) share this variant
    /// so they cannot poison an otherwise-recognised line, but they carry no
    /// path: they read no content, and counting them would nag an agent for
    /// commands that cost it nothing.
    PartialRead { path: Option<String> },
    /// A broad content search for `pattern` under an optional path. `soft`
    /// marks a search whose output is already bounded (`-l`, `-c`, context
    /// flags) — still broad, but the redirect softens to an advisory.
    Grep {
        pattern: String,
        path: Option<String>,
        soft: bool,
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

/// The one file operand of a flag-carrying command, or `None` when there isn't
/// exactly one (`head -n 5 a.rs b.rs`, or a pipe-fed `head -n 5` with no
/// operand at all). Flag VALUES are the trap: `-n 50` must not read as a file,
/// so a numeric word following a bare short flag is skipped.
fn sole_operand(args: &[String]) -> Option<String> {
    let mut files: Vec<&String> = Vec::new();
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a.starts_with('-') {
            // `-n50` / `--lines=50` carry their value; a bare `-n` takes the
            // next word.
            skip_next = a.len() <= 2 && !a.contains('=');
        } else {
            files.push(a);
        }
    }
    match files.as_slice() {
        [one] => Some((*one).clone()),
        _ => None,
    }
}

/// Precedence among recognised intents (see `classify_shell`).
fn rank(i: &ShellIntent) -> u8 {
    match i {
        ShellIntent::Other => 0,
        // A pathless metadata probe is the weakest recognised intent; a partial
        // read that names a file outranks it, so `wc -l f && sed -n '40,80p' f`
        // is judged on the slice rather than on whichever segment came first.
        ShellIntent::PartialRead { path: None } => 1,
        ShellIntent::PartialRead { path: Some(_) } => 2,
        ShellIntent::Grep { .. } => 3,
        ShellIntent::Read { .. } => 4,
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
        "head" | "tail" => ShellIntent::PartialRead {
            path: sole_operand(args),
        },
        // Metadata probes: they pull no file content into context, and an agent
        // routinely pairs one with the read it is about to do (`wc -l f &&
        // sed -n '1,500p' f`). Treated as harmless company so they cannot
        // poison an otherwise-recognised line.
        "wc" | "ls" | "pwd" | "file" | "stat" | "basename" | "dirname" | "echo" => {
            ShellIntent::PartialRead { path: None }
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
    let narrowed = ShellIntent::PartialRead {
        path: Some((*file).clone()),
    };
    let (start, end) = match range.split_once(',') {
        Some((s, e)) => (s, e),
        // A single-line script (`sed -n '5p'`) is as partial as it gets.
        None => return narrowed,
    };
    // Only a read that starts at line 1 can be a full read; `sed -n '40,80p'`
    // is the agent already narrowing.
    if start.trim() != "1" {
        return narrowed;
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

/// `rg PATTERN [PATH]` → the grep half of `classify_shell`. A narrowing flag
/// (`-g`, `-t`, `--files`, `-m`, …) makes it surgical and therefore `Other`.
/// Output-bounding flags (`-l`, `-c`, context windows) keep it a `Grep` but
/// mark it `soft` — the search is still broad, only its presentation isn't.
fn classify_grep(args: &[String]) -> ShellIntent {
    let all_digits = |v: &str| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit());
    let mut positional: Vec<&String> = Vec::new();
    let mut soft = false;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        let Some(flag) = a.strip_prefix('-') else {
            positional.push(a);
            continue;
        };
        // `-r`/`-R`/`-n`/`-i` and friends only affect presentation or
        // recursion; anything else narrows the search or changes its shape.
        let plain = flag.trim_start_matches('-');
        if matches!(plain, "r" | "R" | "n" | "i" | "rn" | "nr" | "ri" | "ir") || plain.is_empty() {
            continue;
        }
        // Output-bounding flags: same broad search, bounded presentation.
        if matches!(plain, "l" | "c" | "files-with-matches" | "count") {
            soft = true;
            continue;
        }
        // Context flags carrying their value: `-C3`, `--context=3`.
        let ctx_attached = (plain.len() > 1
            && ['A', 'B', 'C'].iter().any(|c| plain.starts_with(*c))
            && all_digits(&plain[1..]))
            || plain.split_once('=').is_some_and(|(name, v)| {
                matches!(name, "context" | "after-context" | "before-context") && all_digits(v)
            });
        if ctx_attached {
            soft = true;
            continue;
        }
        // Bare context flags take the count as the NEXT argument; anything
        // non-numeric there makes the line untrustworthy → not ours.
        if matches!(
            plain,
            "A" | "B" | "C" | "context" | "after-context" | "before-context"
        ) {
            match iter.next() {
                Some(v) if all_digits(v) => {
                    soft = true;
                    continue;
                }
                _ => return ShellIntent::Other,
            }
        }
        return ShellIntent::Other;
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
        soft,
    }
}
