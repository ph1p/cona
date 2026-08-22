//! `grep`: literal/regex line search over indexed files, hits mapped to their
//! enclosing symbol. `Matcher` is THE line-matching rule; `grep_prefilter`
//! narrows candidate files via rg/grep with the same mode.

use crate::commands::{jout, GrepOpts, PathFilter, ENCLOSING_SYMBOL_SQL, LIMIT_TRAILER};
use crate::{db, indexer};
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

/// Substring search over indexed code files only. Each hit is mapped to its
/// enclosing symbol so the agent can jump straight to `show <Symbol>` instead
/// of reading around the line.
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
    } = opts;
    let matcher = Matcher::new(pattern, ignore_case, regex)?;
    let pf = PathFilter::new(root, path_filter);
    let mut stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let mut files: Vec<String> = stmt.query_map([], |r| r.get(0))?.flatten().collect();
    // rg (or grep) prefilters the candidate files far faster than reading
    // everything in-process; on any failure we fall back to the full scan.
    // A directory scope is handed to rg as its search root, so a scoped query
    // walks that subtree instead of the whole repo.
    match grep_prefilter(
        root,
        pattern,
        &matcher,
        ignore_case,
        pf.search_root(),
        include_deps,
    ) {
        // --include-deps searches OUTSIDE the index by design: dependency trees
        // are deliberately never indexed, so intersecting with the index would
        // make the flag a no-op. The prefilter's own list becomes the file list;
        // hits in unindexed files simply carry no enclosing symbol, which the
        // renderer already handles. Sorted so output stays deterministic.
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
    let mut enclosing = conn.prepare(ENCLOSING_SYMBOL_SQL)?;
    let mut hits: Vec<(String, usize, String, String)> = Vec::new();
    // Honest baseline: per hit file, a grep pass + a Read window around each
    // match line — what the same search costs an agent without cona, NOT
    // the whole file.
    let mut baseline: i64 = 0;
    let mut truncated = false;
    'outer: for rel in files {
        let Ok(src) = std::fs::read_to_string(root.join(&rel)) else {
            continue;
        };
        let mut match_lines: Vec<usize> = Vec::new();
        // Full per-line lengths up front so the ±READ_PAD_LINES baseline window
        // isn't clamped short when a match sits near the file's end or when the
        // limit truncates this file mid-scan.
        let line_lens: Vec<usize> = src.lines().map(str::len).collect();
        for (ln, line) in src.lines().enumerate() {
            if !matcher.is_match(line) {
                continue;
            }
            // symbol ranges come from the index — refresh before labeling.
            // Skipped under --include-deps: those hits come from the prefilter,
            // not the index, so most have no `files` row. is_stale() reports a
            // missing row as stale, which would make every dependency hit pay a
            // full parse plus a write txn to insert symbols the indexer
            // deliberately never creates.
            if match_lines.is_empty() && !include_deps {
                indexer::ensure_fresh(root, conn, &rel);
            }
            match_lines.push(ln + 1);
            let sym: String = enclosing
                .query_row(rusqlite::params![rel, (ln + 1) as i64], |r| r.get(0))
                .unwrap_or_default();
            hits.push((rel.clone(), ln + 1, sym, line.trim().to_string()));
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
    for (f, l, sym, t) in &hits {
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
        out.push_str(&format!("no matches for '{pattern}'"));
        // Literal is the default. A regex-looking pattern returning zero hits is
        // the worst failure mode — the agent concludes the code doesn't exist.
        // Name the flag that would have matched instead of staying silent.
        if let Some(literal) = regexish_literal(pattern).filter(|_| !regex) {
            out.push_str(&format!(
                "\n  note: matching is literal by default — '{pattern}' was searched verbatim.\
                 \n  try `cona grep {pattern} --regex`"
            ));
            if !literal.is_empty() {
                out.push_str(&format!(" — or the literal part: `cona grep {literal}`"));
            }
        } else if path_filter.is_some() {
            out.push_str(" — try without --path");
        } else {
            // Plain identifier, no filter, zero hits: a typo or a half-remembered
            // name is the likeliest cause — point at the recovery that handles it.
            out.push_str(&format!(
                "\n  try `cona find {pattern}` — symbol search with a typo-tolerant fallback"
            ));
        }
        out.push('\n');
    }
    Ok((out, baseline))
}

/// Regex metacharacters that make a pattern *look* like a regex. Used only to
/// explain a zero-hit fixed-string search — never to change matching.
/// THE line-matching rule behind `grep`, in one place so the per-line test is a
/// single call and the mode can't drift between the in-process scan and the
/// rg/grep prefilter that narrows the candidate files.
///
/// Literal is the default: patterns like `foo.bar` or `Vec<T>` are ordinary code
/// and must not be reinterpreted. `--regex` opts in.
pub(crate) enum Matcher {
    /// Pre-lowercased when `ignore_case`, so the needle isn't rebuilt per line.
    Literal {
        needle: String,
        ignore_case: bool,
    },
    Regex(regex::Regex),
}

impl Matcher {
    /// Case-sensitive literal — for callers whose pattern is an identifier, where
    /// regex is never the right reading (a name holding `$` or `.` must match
    /// itself).
    pub(crate) fn literal(pattern: &str) -> Self {
        Matcher::Literal {
            needle: pattern.to_string(),
            ignore_case: false,
        }
    }

    /// `Err` only for an invalid regex — the caller surfaces it verbatim, since
    /// a silent fallback to literal would answer a different question.
    pub(super) fn new(pattern: &str, ignore_case: bool, regex: bool) -> Result<Self> {
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

    /// The extra flag rg/grep needs to read the pattern the same way we do, if
    /// any. rg is already Rust-regex by default — exactly our regex dialect —
    /// so the regex case needs nothing from it; system grep needs ERE to come
    /// close. A prefilter that disagreed would drop files holding real matches.
    fn prefilter_flag(&self, bin: &str) -> Option<&'static str> {
        match self {
            Matcher::Literal { .. } if bin == "rg" => Some("--fixed-strings"),
            Matcher::Literal { .. } => Some("-F"),
            Matcher::Regex(_) if bin == "rg" => None,
            Matcher::Regex(_) => Some("-E"),
        }
    }
}

fn regexish_literal(pattern: &str) -> Option<String> {
    const META: [char; 11] = ['(', ')', '[', ']', '|', '+', '*', '?', '^', '$', '\\'];
    if !pattern.contains(|c| META.contains(&c)) {
        return None;
    }
    // The longest run of plain characters is the best literal fallback to
    // suggest (`tokens_(out|saved)` → `tokens_`).
    Some(
        pattern
            .split(|c| META.contains(&c) || c == '.' || c == '{' || c == '}')
            .max_by_key(|s| s.len())
            .unwrap_or("")
            .to_string(),
    )
}

/// Fixed-string list of files containing `pattern`, via ripgrep when
/// installed, system grep as fallback. `None` = no prefilter available
/// (tool missing or errored) — caller scans everything, fail-open.
/// `scope` (a repo-relative directory) becomes the search root, so a scoped
/// query makes rg walk only that subtree. Hits come back relative to it, so the
/// scope is prefixed again to keep every path repo-relative.
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
        // rg honours .gitignore, which is what usually hides node_modules — so
        // widening the search means telling rg to stop ignoring. It also needs
        // --follow: a pnpm `node_modules` is a tree of symlinks into the store,
        // and without following them the flag finds NOTHING on the package
        // manager most likely to have a large dep tree. `grep -r` ignores no
        // files and follows nothing, so `-R` is its counterpart.
        if include_deps {
            if bin == "rg" {
                args.extend(["--no-ignore", "--follow"]);
            } else {
                // -R is -r plus symlink following; swap rather than add, since
                // passing both is a conflicting-flag error on some greps.
                // Note plain grep never honoured .gitignore, so it was already
                // searching dep dirs — here -R only adds the symlink farm.
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

    /// The prefilter narrows which files are scanned at all, so it MUST read the
    /// pattern the same way the in-process matcher does — a disagreement drops
    /// files that hold real matches.
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

    /// `--include-deps` has two independent ways to silently find nothing:
    /// rg's .gitignore filter, and rg not following symlinks. A pnpm
    /// `node_modules` is a symlink farm, so BOTH must be defeated or the flag
    /// is a no-op exactly where it matters most.
    ///
    /// Asserted against rg only. The guarantee is genuinely rg-specific: plain
    /// `grep` has no .gitignore concept, so it already descends into
    /// `node_modules` and reports the symlink target by its REAL path — under
    /// the grep fallback the flag is a no-op and there is nothing to assert.
    /// CI runners without rg would otherwise test the wrong backend.
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
        // No tempfile dev-dependency (this crate keeps its dep set lean), so
        // build a uniquely-named dir by hand and clean it up at the end.
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
