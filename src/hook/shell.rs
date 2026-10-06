//! Shell-command normalization: recover Read/Grep intent from a command line.

use std::path::Path;

/// What a shell command turns out to be, once normalized. Shell-only harnesses
/// (Codex: `tool_name = "Bash"`, `command = "sed -n '1,240p' main.rs"`) never
/// emit `Read`/`Grep`, so without this the PreToolUse tier is dead there.
///
/// Deliberately narrow: anything unrecognised is `Other` and passes — the hook
/// may never block work it does not fully understand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellIntent {
    /// A read of `path` from line 1. `upto` is the last line asked for (`None`
    /// = to the end, as with `cat` or `sed -n '1,$p'`).
    ///
    /// A numeric bound is NOT automatically partial: `sed -n '1,240p'` is how
    /// an agent spells "show me the file". The caller treats it as partial only
    /// when the real file is longer than `upto`.
    Read { path: String, upto: Option<i64> },
    /// A read the agent already narrowed (line range, `head -n`, …) — the
    /// shell equivalent of Read with offset/limit. Never intercepted.
    ///
    /// `path` is `Some` only when ONE named file's slice enters context (what
    /// the slice accounting counts). Metadata probes (`wc`, `ls`, `stat`,
    /// `echo`) share the variant so they cannot poison a recognised line, but
    /// carry no path: they read no content and must not be nagged about.
    PartialRead { path: Option<String> },
    /// `sed -n 'A,Bp' f` with A > 1: a slice of `span` lines. One wider than
    /// the full-read threshold is a split full read and judged as one (the
    /// threshold lives in config, so intercept decides).
    Slice { path: String, span: i64 },
    /// A broad content search for `pattern` under an optional path. `soft` =
    /// output already bounded (`-l`, `-c`, context flags): advisory, not block.
    Grep {
        pattern: String,
        path: Option<String>,
        soft: bool,
    },
    /// Not a read or a search we recognise.
    Other,
}

/// Split ONE simple command into words, honouring single/double quotes.
/// `None` on anything that makes the words untrustworthy: unterminated quote,
/// redirect, substitution (`$(`, backticks), backslash escape, or a chaining
/// operator (`split_segments` should have removed those already).
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
/// respecting quotes. Compound commands are the NORM in shell harnesses
/// (`wc -l f && sed -n '1,500p' f`), so each segment is classified on its own
/// and the caller acts only when every one is a read. `None` on unbalanced
/// quoting.
pub fn split_segments(cmd: &str) -> Option<Vec<String>> {
    Some(
        split_pipeline(cmd)?
            .into_iter()
            .map(|(seg, _)| seg)
            .collect(),
    )
}

/// `split_segments`, with each segment flagged `true` when it reads the
/// previous one's output through a single `|`, so `classify_shell` can treat
/// a piped filter (`sort`, `cut`, `grep -v`) as neutral.
pub fn split_pipeline(cmd: &str) -> Option<Vec<(String, bool)>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut piped = false;
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
                ';' | '\n' => {
                    out.push((std::mem::take(&mut cur), piped));
                    piped = false;
                }
                '&' | '|' => {
                    // `&&`/`||` collapse to one separator; a bare `&`
                    // (background) or `|` (pipe) separates just the same.
                    let doubled = chars.peek() == Some(&c);
                    if doubled {
                        chars.next();
                    }
                    out.push((std::mem::take(&mut cur), piped));
                    piped = c == '|' && !doubled;
                }
                c => cur.push(c),
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    out.push((cur, piped));
    Some(
        out.into_iter()
            .map(|(s, p)| (s.trim().to_string(), p))
            .filter(|(s, _)| !s.is_empty())
            .collect(),
    )
}

/// Peel a `sh -c "…"` / `bash -lc "…"` / `zsh -lc "…"` wrapper off a command
/// line, returning the inner script (Codex issues every call as
/// `/bin/zsh -lc "<script>"`). `None` when the command is not such a wrapper.
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
/// A line is a read/search only when EVERY segment is one — one unrecognised
/// segment (an edit, a build, `rm`) makes it `Other`, since blocking the line
/// blocks that segment too. The strongest intent wins (`Read` > `Grep` >
/// `PartialRead`), so `wc -l f && sed -n '1,500p' f` is judged on the read.
///
/// Recognised but not intents: `cd DIR` (moves where later relative paths
/// resolve) and a piped stdin filter (`| sort`, `| grep -v x`).
pub fn classify_shell(cmd: &str) -> ShellIntent {
    let inner = unwrap_shell_wrapper(cmd);
    let line = inner.as_deref().unwrap_or(cmd);
    let Some(segments) = split_pipeline(line) else {
        return ShellIntent::Other;
    };
    let mut dir: Option<String> = None;
    let mut best = ShellIntent::Other;
    for (seg, piped) in &segments {
        match cd_target(seg) {
            Some(Some(target)) => {
                dir = Some(match dir {
                    Some(d) => shell_join(&d, &target),
                    None => target,
                });
                continue;
            }
            // `cd`, `cd -`, `cd ~/x`: a directory we cannot resolve, so every
            // relative path after it is unknown too.
            Some(None) => return ShellIntent::Other,
            None => {}
        }
        if *piped && is_stdin_filter(seg) {
            continue;
        }
        match classify_command(seg) {
            // One segment we don't understand poisons the whole line.
            ShellIntent::Other => return ShellIntent::Other,
            intent => {
                let intent = rebase(intent, dir.as_deref());
                if rank(&intent) > rank(&best) {
                    best = intent;
                }
            }
        }
    }
    best
}

/// `Some(Some(dir))` for a resolvable `cd DIR`/`pushd DIR`, `Some(None)` for a
/// `cd` whose target we cannot know (none, `-`, `~…`, a flag), `None` when the
/// segment is not a `cd` at all.
fn cd_target(seg: &str) -> Option<Option<String>> {
    let words = shell_words(seg)?;
    let (prog, args) = words.split_first()?;
    if !matches!(prog.as_str(), "cd" | "pushd") {
        return None;
    }
    Some(match args {
        [dir] if !dir.is_empty() && !dir.starts_with(['-', '~']) => Some(dir.clone()),
        _ => None,
    })
}

/// Join a shell path onto a `cd` directory with `/` (`Path::join` would emit
/// `\` on Windows). An absolute `p` replaces `dir`, as a shell would.
fn shell_join(dir: &str, p: &str) -> String {
    if p.starts_with('/') || Path::new(p).is_absolute() {
        return p.to_string();
    }
    format!("{}/{p}", dir.trim_end_matches(['/', '\\']))
}

/// Resolve an intent's relative paths against the `cd` directory in effect.
/// A path-less search (`rg X`) becomes a search of that directory.
fn rebase(intent: ShellIntent, dir: Option<&str>) -> ShellIntent {
    let Some(dir) = dir else {
        return intent;
    };
    let join = |p: &str| shell_join(dir, p);
    match intent {
        ShellIntent::Read { path, upto } => ShellIntent::Read {
            path: join(&path),
            upto,
        },
        ShellIntent::PartialRead { path: Some(p) } => ShellIntent::PartialRead {
            path: Some(join(&p)),
        },
        ShellIntent::Slice { path, span } => ShellIntent::Slice {
            path: join(&path),
            span,
        },
        ShellIntent::Grep {
            pattern,
            path,
            soft,
        } => ShellIntent::Grep {
            pattern,
            path: Some(path.map_or_else(|| dir.to_string(), |p| join(&p))),
            soft,
        },
        other => other,
    }
}

/// A command that, fed through a pipe, only filters or reshapes its stdin.
/// Only consulted for piped segments (`ls | grep x` searches only `ls`'s
/// output). A grep/sed naming a file of its own is classified normally.
fn is_stdin_filter(seg: &str) -> bool {
    let Some(words) = shell_words(seg) else {
        return false;
    };
    let Some((prog, args)) = words.split_first() else {
        return false;
    };
    let prog = Path::new(prog.as_str())
        .file_name()
        .map_or_else(|| prog.as_str().into(), |s| s.to_string_lossy());
    let operands = || args.iter().filter(|a| !a.starts_with('-')).count();
    match prog.as_ref() {
        "sort" | "uniq" | "cut" | "tr" | "column" | "nl" | "rev" | "tac" | "fold" | "fmt"
        | "awk" | "jq" | "head" | "tail" | "wc" => true,
        // No operand after the pattern = greps stdin; a second one is a path.
        "grep" | "rg" | "ag" | "ack" => operands() <= 1,
        // Only a script, no file: prints stdin. `-i` needs a file to edit.
        "sed" => {
            !args
                .iter()
                .any(|a| a.starts_with("-i") || a == "--in-place")
                && operands() <= 1
        }
        _ => false,
    }
}

/// The one file operand of a flag-carrying command, or `None` unless exactly
/// one (`head -n 5 a.rs b.rs`, pipe-fed `head -n 5`). Flag VALUES are the
/// trap: `-n 50` must not read as a file.
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
        // A pathless probe is weakest, so `wc -l f && sed -n '40,80p' f` is
        // judged on the slice, not on whichever segment came first.
        ShellIntent::PartialRead { path: None } => 1,
        ShellIntent::PartialRead { path: Some(_) } | ShellIntent::Slice { .. } => 2,
        ShellIntent::Grep { .. } => 3,
        ShellIntent::Read { .. } => 4,
    }
}

/// Classify ONE simple command (no chaining, no wrapper). Pure and unit-tested;
/// the decision half is shared with the native Read/Grep path.
pub fn classify_command(cmd: &str) -> ShellIntent {
    let Some(words) = shell_words(cmd) else {
        return ShellIntent::Other;
    };
    // Skip leading `VAR=value` assignments (`LC_ALL=C grep …`).
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
        // Metadata probes pull no content and routinely accompany a read
        // (`wc -l f && sed -n '1,500p' f`); harmless, so they cannot poison it.
        "wc" | "ls" | "pwd" | "file" | "stat" | "basename" | "dirname" | "echo" => {
            ShellIntent::PartialRead { path: None }
        }
        // `sed -n '<range>p' FILE`; `1,$p` / `1,99999p` is "read it all".
        "sed" => classify_sed(args),
        "rg" | "grep" | "ag" | "ack" => classify_grep(args),
        _ => ShellIntent::Other,
    }
}

/// `sed -n '1,240p' FILE` → the read half of `classify_shell`. Only the
/// print-range idiom; any other sed script is `Other` (it may be an edit).
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
    // Only a read from line 1 is a full read; `sed -n '40,80p'` is narrowing —
    // unless wide enough to be a full read in pieces (intercept judges).
    if start.trim() != "1" {
        return match (start.trim().parse::<i64>(), end.trim().parse::<i64>()) {
            (Ok(a), Ok(b)) if b >= a => ShellIntent::Slice {
                path: (*file).clone(),
                span: b - a + 1,
            },
            _ => narrowed,
        };
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

/// Short grep/rg flags that change only presentation, recursion or regex
/// dialect — the search stays exactly as broad.
const GREP_PRESENTATION_SHORT: &str = "rRniHhowsEFPIa";
/// Short flags that keep the search broad but bound its output.
const GREP_SOFT_SHORT: &str = "lc";
/// Long flags (value after `=` ignored) that leave the search as broad.
/// `--include`/`--glob` are absent on purpose: they narrow it.
const GREP_PRESENTATION_LONG: &[&str] = &[
    "color",
    "colour",
    "no-heading",
    "heading",
    "line-number",
    "with-filename",
    "no-filename",
    "ignore-case",
    "smart-case",
    "recursive",
    "fixed-strings",
    "word-regexp",
    "extended-regexp",
    "only-matching",
    "exclude",
    "exclude-dir",
];

/// `rg PATTERN [PATH]` → the grep half of `classify_shell`. A narrowing flag
/// (`-g`, `-t`, `--files`, `-m`, …) makes it surgical → `Other`.
/// Output-bounding flags (`-l`, `-c`, context) keep it a `Grep`, marked `soft`.
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
        let plain = flag.trim_start_matches('-');
        if plain.is_empty() {
            continue;
        }
        // A short-flag cluster (`-rn`, `-rhno`) is judged per letter; one
        // narrowing or inverting letter (`-v`, `-e`, `-m`) → not ours.
        let is_cluster = !flag.starts_with('-') && plain.chars().all(|c| c.is_ascii_alphabetic());
        if is_cluster
            && plain
                .chars()
                .all(|c| GREP_PRESENTATION_SHORT.contains(c) || GREP_SOFT_SHORT.contains(c))
        {
            soft |= plain.chars().any(|c| GREP_SOFT_SHORT.contains(c));
            continue;
        }
        let long_name = plain.split_once('=').map_or(plain, |(name, _)| name);
        if flag.starts_with('-') && GREP_PRESENTATION_LONG.contains(&long_name) {
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
        // Bare context flags take the count as the NEXT argument.
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
    // A bare pattern searches the cwd — the broad search this tier wants. A
    // second operand is the directory.
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
