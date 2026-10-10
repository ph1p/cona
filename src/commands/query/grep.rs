//! `grep`: literal/regex line search over indexed files, hits mapped to their
//! enclosing symbol. `Matcher` is THE line-matching rule; `grep_prefilter`
//! narrows candidate files via rg/grep with the same mode.

use crate::commands::{defaults, jout, GrepOpts, PathFilter, ENCLOSING_SYMBOL_SQL, LIMIT_TRAILER};
use crate::{db, indexer};
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

/// Substring search over indexed code files. Each hit carries its enclosing
/// symbol so the agent can jump straight to `show <Symbol>`.
pub fn cmd_grep(
    root: &Path,
    conn: &Connection,
    pattern: &str,
    opts: GrepOpts<'_>,
    json: bool,
) -> Result<(String, i64)> {
    let GrepOpts {
        ignore_case,
        regex,
        limit,
        path: path_filter,
        include_deps,
        before,
        after,
        files_only,
        count,
    } = opts;
    let matcher = Matcher::new(pattern, ignore_case, regex)?;
    let pf = PathFilter::new(root, path_filter);
    let mut stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let mut files: Vec<String> = stmt.query_map([], |r| r.get(0))?.flatten().collect();
    // rg (or grep) prefilters candidate files far faster than an in-process
    // scan; on failure we fall back to the full scan. A directory scope becomes
    // rg's search root, so a scoped query walks only that subtree.
    match grep_prefilter(
        root,
        matcher.source(pattern),
        &matcher,
        ignore_case,
        pf.search_root(),
        include_deps,
    ) {
        // --include-deps searches OUTSIDE the index by design: dependency trees
        // are never indexed, so intersecting with the index would make the flag
        // a no-op. The prefilter's list becomes the file list (hits there just
        // carry no enclosing symbol), sorted for deterministic output.
        Some(candidates) if include_deps => {
            files = candidates.into_iter().collect();
            files.sort();
        }
        Some(candidates) => files.retain(|f| candidates.contains(f)),
        // No rg and no grep: the index is all we have, so the flag cannot widen
        // the search. Say so rather than silently returning repo-only hits.
        None if include_deps => {
            return Err(anyhow::anyhow!(
                "--include-deps needs `rg` or `grep` on PATH — dependency dirs are not indexed, so there is nothing to search without one"
            ));
        }
        None => {}
    }
    files.retain(|f| pf.ok(f));
    if files_only || count {
        return grep_files(root, conn, pattern, &matcher, files, opts, json);
    }
    let mut enclosing = conn.prepare(ENCLOSING_SYMBOL_SQL)?;
    let mut hits: Vec<(String, usize, String, String)> = Vec::new();
    // Per hit, the surrounding lines asked for with -A/-B/-C (empty otherwise).
    let mut ctx: Vec<Vec<(usize, String)>> = Vec::new();
    // Honest baseline: per hit file, a grep pass + a Read window around each
    // match — what this search costs without cona, NOT the whole file.
    let mut baseline: i64 = 0;
    let mut truncated = false;
    'outer: for rel in files {
        let Ok(src) = std::fs::read_to_string(root.join(&rel)) else {
            continue;
        };
        let mut match_lines: Vec<usize> = Vec::new();
        // Full line lengths up front, so the ±READ_PAD_LINES window isn't
        // clamped near the file's end or when the limit truncates mid-scan.
        let line_lens: Vec<usize> = src.lines().map(str::len).collect();
        let lines: Vec<&str> = if before + after > 0 {
            src.lines().collect()
        } else {
            Vec::new()
        };
        for (ln, line) in src.lines().enumerate() {
            if !matcher.is_match(line) {
                continue;
            }
            // Symbol ranges come from the index — refresh before labeling.
            // Skipped under --include-deps: most dep hits have no `files` row,
            // and is_stale() treats that as stale, so each would pay a full
            // parse + write txn for symbols the indexer never creates.
            if match_lines.is_empty() && !include_deps {
                indexer::ensure_fresh(root, conn, &rel);
            }
            match_lines.push(ln + 1);
            let sym: String = enclosing
                .query_row(rusqlite::params![rel, (ln + 1) as i64], |r| r.get(0))
                .unwrap_or_default();
            hits.push((rel.clone(), ln + 1, sym, line.trim().to_string()));
            if !lines.is_empty() {
                let lo = ln.saturating_sub(before);
                let hi = (ln + after).min(lines.len() - 1);
                ctx.push(
                    (lo..=hi)
                        .map(|i| (i + 1, lines[i].trim_end().to_string()))
                        .collect(),
                );
            }
            if hits.len() >= limit {
                truncated = true;
                baseline += db::baseline_tokens(&line_lens, &match_lines);
                break 'outer;
            }
        }
        if !match_lines.is_empty() {
            baseline += db::baseline_tokens(&line_lens, &match_lines);
        }
    }
    if json {
        let items: Vec<_> = hits
            .iter()
            .map(|(f, l, sym, t)| {
                serde_json::json!({"file": f, "line": l, "symbol": sym, "text": t})
            })
            .collect();
        return jout(&items, baseline);
    }
    let mut out = String::new();
    if !ctx.is_empty() {
        render_with_context(&mut out, &hits, &ctx, &matcher);
    }
    for (f, l, sym, t) in hits.iter().filter(|_| ctx.is_empty()) {
        if sym.is_empty() {
            out.push_str(&format!("{f}:{l}: {t}\n"));
        } else {
            out.push_str(&format!("{f}:{l} (in {sym}): {t}\n"));
        }
    }
    if truncated {
        out.push_str(LIMIT_TRAILER);
    }
    if hits.is_empty() {
        out.push_str(&no_matches(root, conn, pattern, &matcher, opts)?);
    }
    Ok((out, baseline))
}

/// `-l`/`-c`: one line per matching file. Nothing to label, so no index
/// lookups; the baseline is 0 — `grep -l` costs the same without cona.
fn grep_files(
    root: &Path,
    conn: &Connection,
    pattern: &str,
    matcher: &Matcher,
    files: Vec<String>,
    opts: GrepOpts<'_>,
    json: bool,
) -> Result<(String, i64)> {
    let mut found: Vec<(String, usize)> = Vec::new();
    let mut truncated = false;
    for rel in files {
        let Ok(src) = std::fs::read_to_string(root.join(&rel)) else {
            continue;
        };
        let n = src.lines().filter(|l| matcher.is_match(l)).count();
        if n == 0 {
            continue;
        }
        if found.len() >= opts.limit {
            truncated = true;
            break;
        }
        found.push((rel, n));
    }
    if json {
        let items: Vec<_> = found
            .iter()
            .map(|(f, n)| serde_json::json!({"file": f, "count": n}))
            .collect();
        return jout(&items, 0);
    }
    let mut out = String::new();
    for (f, n) in &found {
        if opts.count {
            out.push_str(&format!("{f}:{n}\n"));
        } else {
            out.push_str(&format!("{f}\n"));
        }
    }
    if truncated {
        out.push_str(LIMIT_TRAILER);
    }
    if found.is_empty() {
        out.push_str(&no_matches(root, conn, pattern, matcher, opts)?);
    }
    Ok((out, 0))
}

/// The zero-hit message. Under `--path` it searches the rest of the repo
/// itself and shows what is there — a "try without --path" hint only costs the
/// agent a second call to learn the same thing.
fn no_matches(
    root: &Path,
    conn: &Connection,
    pattern: &str,
    matcher: &Matcher,
    opts: GrepOpts<'_>,
) -> Result<String> {
    let mut out = String::new();
    let path_filter = opts.path;
    out.push_str(&format!("no matches for '{pattern}'"));
    // A regex-looking pattern with zero literal hits is the worst failure
    // mode — the agent concludes the code doesn't exist. Name the flag.
    if let Some(literal) =
        regexish_literal(pattern).filter(|_| matches!(matcher, Matcher::Literal { .. }))
    {
        out.push_str(&format!(
            "\n  note: matching is literal by default — '{pattern}' was searched verbatim.\
             \n  try `cona grep {pattern} --regex`"
        ));
        if !literal.is_empty() {
            out.push_str(&format!(" — or the literal part: `cona grep {literal}`"));
        }
    } else if let Some(scope) = path_filter {
        let wide = GrepOpts {
            path: None,
            limit: defaults::GREP_ELSEWHERE,
            before: 0,
            after: 0,
            ..opts
        };
        let (elsewhere, _) = cmd_grep(root, conn, pattern, wide, false)?;
        if elsewhere.starts_with("no matches") {
            out.push_str(" anywhere in the repo");
        } else {
            let elsewhere = elsewhere.replace(LIMIT_TRAILER, "… more without --path\n");
            out.push_str(&format!(" under '{scope}' — elsewhere:\n{elsewhere}"));
            return Ok(out);
        }
    } else if looks_like_name(pattern) {
        // A bare identifier with no hits anywhere: likely a typo — point
        // at the fuzzy symbol search.
        out.push_str(&format!(
            "\n  try `cona find {pattern}` — symbol search with a typo-tolerant fallback"
        ));
    } else {
        out.push_str(" anywhere in the repo");
    }
    out.push('\n');
    Ok(out)
}

/// One identifier (or a dotted path) — something `find` could resolve. Text
/// with spaces, quotes or punctuation never names a symbol.
fn looks_like_name(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '.' | ':'))
}

/// `-A`/`-B`/`-C` output: one header per block (`file:line (in sym)`), then
/// numbered lines with `>` on the hits. Overlapping or adjacent windows in one
/// file merge into a single block, so no line is printed twice.
fn render_with_context(
    out: &mut String,
    hits: &[(String, usize, String, String)],
    ctx: &[Vec<(usize, String)>],
    matcher: &Matcher,
) {
    let mut last: Option<(&str, usize)> = None;
    for ((f, l, sym, _), window) in hits.iter().zip(ctx) {
        let first = window.first().map_or(*l, |w| w.0);
        let joined = matches!(last, Some((lf, ll)) if lf == f && first <= ll + 1);
        if !joined {
            if last.is_some() {
                out.push_str("--\n");
            }
            if sym.is_empty() {
                out.push_str(&format!("{f}:{l}\n"));
            } else {
                out.push_str(&format!("{f}:{l} (in {sym})\n"));
            }
        }
        let done = if joined { last.map_or(0, |x| x.1) } else { 0 };
        for (n, text) in window.iter().filter(|(n, _)| *n > done) {
            let mark = if matcher.is_match(text) { '>' } else { ' ' };
            out.push_str(&format!("{n:>5}{mark} {text}\n"));
        }
        let end = window.last().map_or(*l, |w| w.0);
        last = Some((f, end.max(done)));
    }
}

/// THE line-matching rule behind `grep`, in one place so the mode can't drift
/// between the in-process scan and the rg/grep prefilter.
///
/// Literal is the default: `foo.bar` or `Vec<T>` are ordinary code and must not
/// be reinterpreted. `--regex` opts in.
pub(crate) enum Matcher {
    /// Pre-lowercased when `ignore_case`, so the needle isn't rebuilt per line.
    Literal {
        needle: String,
        ignore_case: bool,
    },
    Regex(regex::Regex),
}

impl Matcher {
    /// Case-sensitive literal for identifier patterns — a name holding `$` or
    /// `.` must match itself.
    pub(crate) fn literal(pattern: &str) -> Self {
        Matcher::Literal {
            needle: pattern.to_string(),
            ignore_case: false,
        }
    }

    /// `Err` only for an invalid regex — the caller surfaces it verbatim, since
    /// a silent fallback to literal would answer a different question.
    pub(super) fn new(pattern: &str, ignore_case: bool, regex: bool) -> Result<Self> {
        // `a\|b` is grep's (BRE) alternation, typed out of habit in both modes.
        // Verbatim it matches nothing ("the code doesn't exist"), so it means
        // either branch: literals stay literal; under --regex it becomes `|`
        // (a literal pipe there is `[|]`).
        if pattern.contains("\\|") {
            let alt = if regex {
                pattern.replace("\\|", "|")
            } else {
                pattern
                    .split("\\|")
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join("|")
            };
            return Self::new(&alt, ignore_case, true);
        }
        if !regex {
            let needle = if ignore_case {
                pattern.to_lowercase()
            } else {
                pattern.to_string()
            };
            return Ok(Matcher::Literal {
                needle,
                ignore_case,
            });
        }
        regex::RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .map(Matcher::Regex)
            .map_err(|e| anyhow!("invalid regex '{pattern}': {e}"))
    }

    /// The pattern as this matcher reads it, for the prefilter — differs from
    /// the user's text only when `\|` alternation was rewritten into a regex.
    fn source<'a>(&'a self, pattern: &'a str) -> &'a str {
        match self {
            Matcher::Regex(re) => re.as_str(),
            Matcher::Literal { .. } => pattern,
        }
    }

    fn is_match(&self, line: &str) -> bool {
        match self {
            Matcher::Literal {
                needle,
                ignore_case: true,
            } => line.to_lowercase().contains(needle),
            Matcher::Literal { needle, .. } => line.contains(needle),
            Matcher::Regex(re) => re.is_match(line),
        }
    }

    /// The flag rg/grep needs to read the pattern as we do, if any. rg already
    /// speaks our Rust-regex dialect; system grep needs ERE to come close. A
    /// disagreeing prefilter would drop files holding real matches.
    fn prefilter_flag(&self, bin: &str) -> Option<&'static str> {
        match self {
            Matcher::Literal { .. } if bin == "rg" => Some("--fixed-strings"),
            Matcher::Literal { .. } => Some("-F"),
            Matcher::Regex(_) if bin == "rg" => None,
            Matcher::Regex(_) => Some("-E"),
        }
    }
}

/// Some(longest plain run) when the pattern *looks* like a regex. Only explains
/// a zero-hit literal search — never changes matching.
fn regexish_literal(pattern: &str) -> Option<String> {
    const META: [char; 11] = ['(', ')', '[', ']', '|', '+', '*', '?', '^', '$', '\\'];
    if !pattern.contains(|c| META.contains(&c)) {
        return None;
    }
    // e.g. `tokens_(out|saved)` → `tokens_`
    Some(
        pattern
            .split(|c| META.contains(&c) || c == '.' || c == '{' || c == '}')
            .max_by_key(|s| s.len())
            .unwrap_or("")
            .to_string(),
    )
}

/// Files containing `pattern`, via ripgrep, else system grep. `None` = no
/// prefilter (tool missing or errored) — caller scans everything, fail-open.
/// `scope` (a repo-relative dir) becomes the search root, so rg walks only
/// that subtree; it is prefixed back so every path stays repo-relative.
pub(crate) fn grep_prefilter(
    root: &Path,
    pattern: &str,
    matcher: &Matcher,
    ignore_case: bool,
    scope: Option<&str>,
    include_deps: bool,
) -> Option<HashSet<String>> {
    let attempts: [(&str, Vec<&str>); 2] = [
        ("rg", vec!["--files-with-matches", "--no-messages"]),
        ("grep", vec!["-r", "-l", "-I", "-s"]),
    ];
    for (bin, mut args) in attempts {
        // rg honours .gitignore, which usually hides node_modules, so widening
        // means --no-ignore. It also needs --follow: a pnpm `node_modules` is a
        // symlink farm into the store, and unfollowed the flag finds NOTHING.
        // `grep -r` ignores nothing and follows nothing, so `-R` is its match.
        if include_deps {
            if bin == "rg" {
                args.extend(["--no-ignore", "--follow"]);
            } else {
                // -R = -r plus symlinks; swap rather than add, since both is a
                // conflicting-flag error on some greps.
                args.retain(|a| *a != "-r");
                args.push("-R");
            }
        }
        args.extend(matcher.prefilter_flag(bin));
        if ignore_case {
            args.push("-i");
        }
        let search_dir = scope.unwrap_or(".");
        args.extend(["--", pattern, search_dir]);
        let out = match std::process::Command::new(bin)
            .args(&args)
            .current_dir(root)
            .output()
        {
            Ok(o) => o,
            Err(_) => continue, // not installed
        };
        // 0 = matches, 1 = no matches; anything else = error → next attempt
        match out.status.code() {
            Some(0) | Some(1) => {}
            _ => continue,
        }
        return Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim_start_matches("./").to_string())
                .collect(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_is_the_default_reading() {
        // Regex metachars are ordinary code — `foo.bar` must not match `fooXbar`.
        let m = Matcher::new("foo.bar", false, false).unwrap();
        assert!(m.is_match("let x = foo.bar;"));
        assert!(!m.is_match("let x = fooXbar;"));
    }

    #[test]
    fn regex_mode_applies_the_pattern() {
        let m = Matcher::new(r"tokens_(out|saved)", false, true).unwrap();
        assert!(m.is_match("let tokens_out = 1;"));
        assert!(m.is_match("let tokens_saved = 1;"));
        assert!(!m.is_match("let tokens_total = 1;"));
    }

    #[test]
    fn ignore_case_applies_in_both_modes() {
        assert!(Matcher::new("FooBar", true, false)
            .unwrap()
            .is_match("let foobar = 1;"));
        assert!(Matcher::new("^foo.ar$", true, true)
            .unwrap()
            .is_match("FooBar"));
    }

    #[test]
    fn literal_ctor_never_reads_the_name_as_a_regex() {
        // scan_ref_sites prefilters by identifier; a name holding metachars
        // (`$crate`, `x.y`) must match itself, not act as a pattern.
        let m = Matcher::literal("$crate");
        assert!(m.is_match("$crate::foo()"));
        assert!(!m.is_match("Xcrate::foo()"));
    }

    #[test]
    fn invalid_regex_is_an_error_not_a_literal_fallback() {
        // Silently searching `foo(` verbatim would answer a different question.
        assert!(Matcher::new("foo(", false, true).is_err());
        assert!(Matcher::new("foo(", false, false).is_ok());
    }

    /// The prefilter MUST read the pattern as the in-process matcher does, or it
    /// drops files that hold real matches.
    #[test]
    fn prefilter_flag_matches_the_matcher_mode() {
        let lit = Matcher::new("a.b", false, false).unwrap();
        assert_eq!(lit.prefilter_flag("rg"), Some("--fixed-strings"));
        assert_eq!(lit.prefilter_flag("grep"), Some("-F"));
        let re = Matcher::new("a.b", false, true).unwrap();
        // rg is already Rust-regex by default — our exact dialect.
        assert_eq!(re.prefilter_flag("rg"), None);
        assert_eq!(re.prefilter_flag("grep"), Some("-E"));
    }

    #[test]
    fn bre_alternation_matches_either_branch() {
        // literal mode: each branch stays literal — `.` is not a wildcard
        let m = Matcher::new(r"foo.bar\|baz", false, false).unwrap();
        assert!(m.is_match("x = foo.bar"));
        assert!(m.is_match("baz()"));
        assert!(!m.is_match("fooXbar"));
        assert_eq!(m.source("ignored"), r"foo\.bar|baz");
        // regex mode: `\|` is grep's alternation, not a literal pipe
        let r = Matcher::new(r"tok_(out\|saved)", false, true).unwrap();
        assert!(r.is_match("tok_saved"));
        assert!(Matcher::new(r"A\|b", true, false).unwrap().is_match("a"));
    }

    #[test]
    fn context_blocks_merge_and_mark_hits() {
        let m = Matcher::new("hit", false, false).unwrap();
        let h = |l: usize| ("f.rs".to_string(), l, "S".to_string(), "hit".to_string());
        let w = |a: usize, b: usize| -> Vec<(usize, String)> {
            (a..=b)
                .map(|n| {
                    (
                        n,
                        if n == 3 || n == 5 || n == 20 {
                            "hit"
                        } else {
                            "x"
                        }
                        .to_string(),
                    )
                })
                .collect()
        };
        let mut out = String::new();
        render_with_context(
            &mut out,
            &[h(3), h(5), h(20)],
            &[w(2, 4), w(4, 6), w(19, 21)],
            &m,
        );
        assert_eq!(
            out,
            "f.rs:3 (in S)\n    2  x\n    3> hit\n    4  x\n    5> hit\n    6  x\n--\nf.rs:20 (in S)\n   19  x\n   20> hit\n   21  x\n"
        );
    }

    #[test]
    fn regexish_literal_only_fires_on_metachars() {
        assert_eq!(regexish_literal("plain_name"), None);
        assert_eq!(
            regexish_literal("tokens_(out|saved)").as_deref(),
            Some("tokens_")
        );
    }
}

#[cfg(test)]
mod include_deps_tests {
    use super::*;
    use std::fs;

    /// `--include-deps` can silently find nothing two ways: rg's .gitignore
    /// filter, and rg not following symlinks. A pnpm `node_modules` is a
    /// symlink farm, so BOTH must be defeated.
    ///
    /// Asserted against rg only: plain `grep` has no .gitignore concept and
    /// reports symlink targets by their REAL path, so under the grep fallback
    /// the flag is a no-op and there is nothing to assert.
    #[test]
    #[cfg(unix)] // symlink farm is the point of the test; std::os::unix builds it
    fn include_deps_reaches_gitignored_and_symlinked_files() {
        if std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no rg on this host — the fallback makes no such promise
        }
        // No tempfile dev-dependency (lean dep set): unique dir by hand.
        let root = std::env::temp_dir().join(format!("cona-deps-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        fs::write(root.join("app.rs"), "let x = NEEDLE_TOKEN;\n").unwrap();
        // rg only applies .gitignore inside a repo, so make this one
        fs::create_dir_all(root.join(".git")).unwrap();
        // the real payload lives outside node_modules; node_modules only links to it
        let store = root.join(".store/pkg");
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join("lib.js"), "export const NEEDLE_TOKEN = 1;\n").unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        let linked = std::os::unix::fs::symlink(&store, root.join("node_modules/pkg")).is_ok();

        let matcher = Matcher::literal("NEEDLE_TOKEN");
        let hit = |include_deps| {
            grep_prefilter(&root, "NEEDLE_TOKEN", &matcher, false, None, include_deps)
                .map(|c| c.iter().any(|f| f.contains("node_modules")))
        };
        let (base, deep) = (hit(false), hit(true));
        let _ = fs::remove_dir_all(&root);

        if !linked {
            return; // no symlink privileges — nothing to prove
        }
        assert_eq!(base, Some(false), "default must not enter node_modules");
        assert_eq!(deep, Some(true), "--include-deps must reach it");
    }
}
