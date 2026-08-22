use super::intercept::grep_start;
use super::*;
use crate::lang;
use std::path::PathBuf;

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
    // xml and html index real symbols (elements, named by tag plus an
    // identifying child/attribute), so the advisory tier must fire on a
    // pom.xml or a template like it does on any code file
    for l in [
        "rust",
        "typescript",
        "tsx",
        "sql",
        "hcl",
        "swift",
        "perl",
        "xml",
        "html",
    ] {
        assert!(lang::has_callable_symbols(l), "{l} should be advisable");
    }
    for l in ["markdown", "json", "yaml", "toml", "css"] {
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
        ("a.css", "css"),
        ("a.graphql", "graphql"),
        ("a.nix", "nix"),
        ("a.svelte", "svelte"),
        ("a.vue", "vue"),
        ("a.r", "r"),
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
        soft: false,
    }
}

#[test]
fn redirects_broad_identifier_grep_in_indexed_project() {
    assert_eq!(decide_grep(&grep_facts()), Decision::Redirect);
}

#[test]
fn advises_soft_grep_in_indexed_project() {
    // Bounded output softens the redirect to a hint — the search runs.
    assert_eq!(
        decide_grep(&GrepFacts {
            soft: true,
            ..grep_facts()
        }),
        Decision::Advise
    );
    // Outside an indexed project, soft changes nothing.
    assert_eq!(
        decide_grep(&GrepFacts {
            soft: true,
            indexed_project: false,
            ..grep_facts()
        }),
        Decision::Nudge
    );
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
            path: Some("src".into()),
            soft: false
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
            path: None,
            soft: false
        }
    );
    assert_eq!(
        classify_shell("grep -rn UserService src"),
        ShellIntent::Grep {
            pattern: "UserService".into(),
            path: Some("src".into()),
            soft: false
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
    ] {
        assert_eq!(classify_shell(cmd), ShellIntent::Other, "{cmd}");
    }
}

#[test]
fn output_bounded_shell_greps_are_soft() {
    // Bounded output = still a broad search, but the agent showed
    // restraint — classify as Grep{soft} so it gets the advisory tier.
    for cmd in [
        "rg -l UserService",
        "rg -c UserService",
        "grep -rn --count UserService src",
        "rg -C3 UserService",
        "rg -C 3 UserService",
        "rg --context=2 UserService",
        "rg -A 2 UserService src",
    ] {
        match classify_shell(cmd) {
            ShellIntent::Grep { soft: true, .. } => {}
            other => panic!("expected soft Grep for {cmd}, got {other:?}"),
        }
    }
    // A context flag with a non-numeric value is untrustworthy → Other.
    assert_eq!(classify_shell("rg -C x UserService"), ShellIntent::Other);
}

#[test]
fn grep_start_resolves_relative_paths_against_payload_cwd() {
    // The regression: a relative path arg must join the payload cwd, or the
    // project-root walk lands on a relative "root" no index hash matches.
    assert_eq!(
        grep_start(Some("src/"), Some("/repo")),
        PathBuf::from("/repo/src/")
    );
    assert_eq!(
        grep_start(Some("."), Some("/repo")),
        PathBuf::from("/repo/.")
    );
    assert_eq!(
        grep_start(Some("/abs/dir"), Some("/repo")),
        PathBuf::from("/abs/dir")
    );
    assert_eq!(grep_start(None, Some("/repo")), PathBuf::from("/repo"));
    assert_eq!(grep_start(None, None), PathBuf::from("."));
}
