//! Drive the real binary against real temp repos (CLI contract tests).

#[test]
fn mutations_preserve_bytes_permissions_and_unowned_temps() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let dir = std::env::temp_dir().join(format!("cona-it-safe-mutations-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let run = |args: &[&str], input: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cona"))
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .env("CONA_NO_STATS", "1")
            .env("CONA_NO_AUTO_UPDATE", "1")
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        (
            output.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        )
    };
    let source = dir.join("a.rs");
    std::fs::write(&source, "fn old() {}\r\nfn caller() { old(); }").unwrap();
    std::fs::write(dir.join("a.rs.cona-tmp"), "sentinel").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o751)).unwrap();
    }
    let (ok, output) = run(&["index"], "");
    assert!(ok, "{output}");
    for (args, input) in [
        (vec!["edit", "old"], "fn old() { let _ = 1; }\n"),
        (vec!["edit", "a.rs", "--range", "1-1"], "fn old() {}\n"),
        (vec!["insert", "--at", "a.rs", "1"], "fn inserted() {}\n"),
        (vec!["rename", "old", "renamed"], ""),
    ] {
        let (ok, output) = run(&args, input);
        assert!(ok, "{output}");
        let bytes = std::fs::read(&source).unwrap();
        assert!(!bytes.ends_with(b"\n"));
        assert!(bytes.windows(2).any(|w| w == b"\r\n"));
        assert!(bytes
            .iter()
            .enumerate()
            .all(|(i, b)| *b != b'\n' || i > 0 && bytes[i - 1] == b'\r'));
        assert_eq!(
            std::fs::read_to_string(dir.join("a.rs.cona-tmp")).unwrap(),
            "sentinel"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&source).unwrap().permissions().mode() & 0o777,
                0o751
            );
        }
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            !(name.starts_with(".cona-") && name.ends_with(".tmp"))
        }));
    }
    assert_eq!(
        std::fs::read_to_string(&source).unwrap(),
        "fn renamed() {}\r\nfn inserted() {}\r\nfn caller() { renamed(); }"
    );
    let before = std::fs::read(&source).unwrap();
    for args in [
        vec!["edit", "renamed"],
        vec!["edit", "a.rs", "--range", "1-1"],
        vec!["insert", "--at", "a.rs", "0"],
    ] {
        let (ok, output) = run(&args, "fn {");
        assert!(!ok && output.contains("syntax error"), "{output}");
        assert_eq!(std::fs::read(&source).unwrap(), before);
    }
    // Collision introduced after indexing is rejected by the real CLI too.
    std::fs::write(&source, "fn renamed() {}\r\nfn collision() {}").unwrap();
    let before = std::fs::read(&source).unwrap();
    let (ok, output) = run(&["rename", "renamed", "collision"], "");
    assert!(!ok && output.contains("already defined"), "{output}");
    assert_eq!(std::fs::read(&source).unwrap(), before);
    std::fs::remove_dir_all(dir).unwrap();
}

// Real IO, like the recovery-bug tests: edit --range, insert --after, and
// the syntax-verify rollback they share.
#[test]
fn edit_range_and_insert_roundtrip() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("s.rs");
    std::fs::write(&file, "fn a() {\n    let x = 1;\n}\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str], stdin: &str| -> (bool, String) {
        use std::io::Write;
        let mut c = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let o = c.wait_with_output().unwrap();
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
        )
    };

    run(&["index"], "");

    // edit --range replaces just line 2
    let (ok, _) = run(&["edit", "s.rs", "--range", "2-2"], "    let x = 42;");
    assert!(ok);
    assert!(std::fs::read_to_string(&file)
        .unwrap()
        .contains("let x = 42;"));

    // insert --after adds a sibling symbol, whole file still parses
    let (ok, _) = run(&["insert", "a", "--after"], "fn b() {}\n");
    assert!(ok);
    let body = std::fs::read_to_string(&file).unwrap();
    assert!(body.contains("fn b() {}"));

    // syntax error is rejected and the file is left untouched (invariant 3)
    let before = std::fs::read_to_string(&file).unwrap();
    let (ok, msg) = run(&["edit", "s.rs", "--range", "2-2"], "    let x = ;;;");
    assert!(!ok, "expected rejection, got: {msg}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);

    // check reports a clean file as ok
    let (ok, out) = run(&["check", "s.rs"], "");
    assert!(ok && out.contains("ok"), "{out}");

    // insert --at into a brand-new file (no indexed symbol to anchor on)
    let (ok, _) = run(&["insert", "--at", "fresh.rs", "0"], "fn fresh() {}\n");
    assert!(ok);
    assert_eq!(
        std::fs::read_to_string(dir.join("fresh.rs")).unwrap(),
        "fn fresh() {}\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// `show --all` renders every candidate of an ambiguous name, including a
// same-file enum + impl pair that `file:Name` cannot separate. Pins the
// guide/skill/MCP promise and an honest ambiguity message: hatches that cannot
// separate the pool (file/Parent.Name for a same-file pair) are not suggested.
#[test]
fn show_all_renders_same_file_enum_impl_pair() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-showall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("s.rs"),
        "pub enum Thing { A }\nimpl Thing {\n    pub fn go(&self) {}\n}\n",
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str]| -> (bool, String) {
        let o = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .current_dir(&dir)
            .output()
            .unwrap();
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
        )
    };

    run(&["index"]);
    // --all prints BOTH definitions, no ambiguity error
    let (ok, out) = run(&["show", "Thing", "--all"]);
    assert!(ok, "{out}");
    assert!(out.contains("enum Thing"), "{out}");
    assert!(out.contains("impl Thing"), "{out}");
    assert!(!out.contains("ambiguous"), "{out}");
    // without --all a SMALL ambiguity (≤3 candidates, ≤400 lines) still
    // auto-renders every definition; the banner names the narrowing hatches.
    let (ok, msg) = run(&["show", "Thing"]);
    assert!(ok, "expected auto-all render, got error: {msg}");
    assert!(msg.contains("ambiguous — showing all 2"), "{msg}");
    assert!(
        msg.contains("enum Thing") && msg.contains("impl Thing"),
        "{msg}"
    );
    assert!(msg.contains("--kind"), "{msg}");
    let _ = std::fs::remove_dir_all(&dir);
}

// Grouped subcommands (`nav show`) and flat aliases (`show`) produce identical
// output: flat forms stay backward-compatible forever.
#[test]
fn grouped_and_flat_are_equivalent() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-group-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("s.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str]| -> String {
        let o = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .current_dir(&dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).to_string()
    };

    run(&["index"]);
    // one representative per group: nav/inspect/history
    for (flat, grouped) in [
        (vec!["show", "alpha"], vec!["nav", "show", "alpha"]),
        (vec!["outline", "s.rs"], vec!["nav", "outline", "s.rs"]),
        (vec!["deps"], vec!["inspect", "deps"]),
        (vec!["entries"], vec!["inspect", "entries"]),
        (vec!["dead"], vec!["inspect", "dead"]),
    ] {
        assert_eq!(
            run(&flat),
            run(&grouped),
            "flat {flat:?} != grouped {grouped:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// check with no file argument walks git for changed + untracked files.
#[test]
fn check_no_arg_walks_git_changes() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        git_in(&dir, args);
    };
    git(&["init", "-q"]);
    // one committed-clean file, one untracked broken file
    std::fs::write(dir.join("ok.rs"), "fn a() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    std::fs::write(dir.join("bad.rs"), "fn broken( {\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    Command::new(bin)
        .arg("index")
        .env("CONA_DATA_DIR", dir.join(".cona-data"))
        .current_dir(&dir)
        .output()
        .unwrap();
    let out = Command::new(bin)
        .arg("check")
        .env("CONA_DATA_DIR", dir.join(".cona-data"))
        .current_dir(&dir)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    // the untracked broken file is flagged; the clean committed one is not walked
    assert!(
        text.contains("bad.rs") && text.contains("syntax error"),
        "{text}"
    );
    assert!(
        !text.contains("ok.rs: ok"),
        "clean committed file should not be walked: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--read-only` cannot refresh the index, so it must never serve stale line
/// numbers as current (invariant 2). `show` fails naming the file and the fix
/// (not rusqlite's "attempt to write a readonly database"); `outline` labels
/// its indexed ranges stale. The writable run proves normal use still
/// self-heals.
#[test]
fn read_only_never_serves_stale_ranges_as_fresh() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-ro-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join(".cona-data");
    let bin = env!("CARGO_BIN_EXE_cona");
    let cona = |args: &[&str]| {
        Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", &data)
            .current_dir(&dir)
            .output()
            .unwrap()
    };

    std::fs::write(dir.join("lib.rs"), "fn target() {\n    let _ = 1;\n}\n").unwrap();
    cona(&["index"]);
    // fresh index: read-only resolves the real range
    let out = cona(&["--read-only", "show", "target"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("lib.rs:1-3"),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // shift the symbol down without reindexing — indexed 1-3, really 3-5
    std::fs::write(
        dir.join("lib.rs"),
        "// added\n// added\nfn target() {\n    let _ = 1;\n}\n",
    )
    .unwrap();

    // per-symbol failures go to stdout so one bad name cannot abort a batch
    let out = cona(&["--read-only", "show", "target"]);
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        err.contains("read-only mode") && err.contains("lib.rs"),
        "expected a stale-file message naming the file, got: {err}"
    );
    assert!(
        !err.contains("readonly database"),
        "raw sqlite error leaked instead of an actionable message: {err}"
    );
    // the stale range must not be printed as if it were current
    assert!(
        !err.contains("lib.rs:1-3"),
        "stale range served as fresh: {err}"
    );

    // outline still prints the indexed ranges, but discloses that they are stale
    let out = cona(&["--read-only", "outline", "lib.rs"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("stale"), "outline hid staleness: {text}");

    // writable mode is unaffected: it reindexes and reports the live range
    let out = cona(&["show", "target"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("lib.rs:3-5"),
        "writable mode should self-heal: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The SessionStart hook fires unattended in whatever directory the harness is
/// in. In $HOME a walk is never wanted (sessions launched there each walked
/// hundreds of MB). `--session-start` must bail out quietly: fail-open, exit 0,
/// no context.
///
/// Unix-only because of the harness, not the behaviour: `dirs::home_dir` reads
/// `$HOME` on unix but the Win32 known-folder API on Windows, which no env var
/// can redirect. The guard itself is platform-neutral.
#[cfg(unix)]
#[test]
fn session_start_refuses_to_index_the_home_dir() {
    use std::process::Command;
    let home = std::env::temp_dir().join(format!("cona-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("a.rs"), "fn hello() {}\n").unwrap();
    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str]| {
        Command::new(bin)
            .args(args)
            .env("HOME", &home)
            .env("CONA_DATA_DIR", home.join(".cona-data"))
            .current_dir(&home)
            .output()
            .unwrap()
    };

    let out = run(&["index", "--quiet", "--session-start"]);
    assert!(out.status.success(), "hook must never fail a session start");
    assert!(
        out.stdout.is_empty(),
        "no context block from an unindexed home dir: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // A typed `cona index` in $HOME is deliberate: it warns, never refuses.
    let out = run(&["index"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("home/filesystem root"),
        "explicit index in $HOME should warn: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&home);
}

// Agent-UX lookups: `show file:N-M` line ranges, `grep -l`/`-c`, the zero-hit
// messages, and `file:Name` misses that say where the name lives.
#[test]
fn line_ranges_grep_counts_and_honest_misses() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-ux-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(
        dir.join("a.rs"),
        "pub fn alpha() {}\n// alpha twice\npub fn beta() {}\n",
    )
    .unwrap();
    std::fs::write(dir.join("sub/b.rs"), "use crate::alpha;\nfn gamma() {}\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str]| -> (String, bool) {
        let o = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .current_dir(&dir)
            .output()
            .unwrap();
        let mut s = String::from_utf8_lossy(&o.stdout).to_string();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        (s, o.status.success())
    };
    run(&["index"]);

    let (out, ok) = run(&["show", "a.rs:2-3"]);
    assert!(ok, "{out}");
    assert!(out.contains("a.rs:2-3"), "{out}");
    assert!(
        out.contains("2 // alpha twice") && out.contains("3 pub fn beta"),
        "{out}"
    );
    assert!(!out.contains("pub fn alpha"), "{out}");
    let (out, ok) = run(&["show", "a.rs:9"]);
    assert!(!ok && out.contains("past the end"), "{out}");

    let (out, _) = run(&["grep", "alpha", "-c"]);
    assert_eq!(out, "a.rs:2\nsub/b.rs:1\ntotal: 3 in 2 files\n");
    let (out, _) = run(&["grep", "alpha", "-l"]);
    assert_eq!(out, "a.rs\nsub/b.rs\n");

    let (out, _) = run(&["grep", "beta", "--path", "sub"]);
    assert!(
        out.starts_with(
            "no matches for 'beta' under 'sub' — elsewhere: 1 hit(s) in 1 file(s): a.rs"
        ),
        "{out}"
    );
    // one line, never the hits themselves; --strict-path drops even that
    assert_eq!(out.lines().count(), 1, "{out}");
    let (out, _) = run(&["grep", "beta", "--path", "sub", "--strict-path"]);
    assert_eq!(out, "no matches for 'beta' under 'sub'\n");
    // several scopes: a miss names its scope, never re-lists a sibling's hits
    let (out, _) = run(&["grep", "beta", "--path", "sub", "--path", "a.rs"]);
    assert!(
        out.starts_with("no matches for 'beta' under 'sub'\n") && !out.contains("elsewhere"),
        "{out}"
    );
    // a scope that names nothing is an error, not a quiet repo-wide search
    let (out, ok) = run(&["grep", "beta", "--path", "nope/x.rs"]);
    assert!(
        !ok && out.contains("names no indexed file or directory"),
        "{out}"
    );
    // a file created since the walk is indexed on the spot
    std::fs::write(dir.join("sub/new.rs"), "fn delta() {}\n").unwrap();
    let (out, ok) = run(&["grep", "delta", "--path", "sub/new.rs"]);
    assert!(ok && out.contains("sub/new.rs:1"), "{out}");
    // -w / -o / --no-truncate / total in the truncation line
    let (out, _) = run(&["grep", "-w", "alph"]);
    assert!(out.starts_with("no matches"), "{out}");
    let (out, _) = run(&["grep", "-w", "alpha", "-c"]);
    assert!(out.contains("total: 3 in 2 files"), "{out}");
    let (out, _) = run(&["grep", "-o", "-e", "fn [a-z]+", "--path", "a.rs"]);
    assert_eq!(
        out,
        "a.rs:1 (in alpha): fn alpha\na.rs:3 (in beta): fn beta\n"
    );
    let (out, _) = run(&["grep", "alpha", "--limit", "1"]);
    assert!(out.contains("truncated: 1 of 3 hits shown"), "{out}");
    // code punctuation is not regex intent: no match-everything retry
    let (out, _) = run(&["grep", ".alpha ??"]);
    assert!(
        out.starts_with("no matches") && !out.contains("as a regex"),
        "{out}"
    );
    let (out, ok) = run(&["grep", "-e", "a", "-e", "b"]);
    assert!(!ok && out.contains("a\\|b"), "{out}");
    let (out, _) = run(&["grep", "no such text"]);
    assert!(
        out.contains("anywhere in the repo") && !out.contains("cona find"),
        "{out}"
    );

    let (out, ok) = run(&["show", "sub/b.rs:alpha"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("not defined in 'sub/b.rs'") && out.contains("a.rs:1"),
        "{out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// refs folds test-file sites into one count line: on a hot symbol they
// outnumber production sites and eat the limit. --tests, --path, or a symbol
// used only in tests show them.
#[test]
fn refs_folds_test_sites() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-refs-fold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("a.rs"),
        "pub fn alpha() {}\nfn main() { alpha(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("tests/t.rs"),
        "fn t() { alpha(); }\nfn only_t() {}\nfn u() { only_t(); }\n",
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str]| -> String {
        let o = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .current_dir(&dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).to_string()
    };
    run(&["index"]);

    let out = run(&["refs", "alpha"]);
    assert!(
        out.contains("a.rs\n") && !out.contains("tests/t.rs"),
        "{out}"
    );
    assert!(out.contains("+ 1 test site in 1 file folded"), "{out}");
    let out = run(&["refs", "alpha", "--tests"]);
    assert!(
        out.contains("tests/t.rs") && !out.contains("folded"),
        "{out}"
    );
    let out = run(&["refs", "alpha", "--path", "tests"]);
    assert!(
        out.contains("tests/t.rs") && !out.contains("folded"),
        "{out}"
    );
    // only test sites: they are the answer, not folded away
    let out = run(&["refs", "only_t"]);
    assert!(
        out.contains("tests/t.rs") && !out.contains("folded"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Notes are the project knowledge an agent wrote on purpose: rename must carry
// them along, a same-named sibling must not see them, and a key nothing
// resolves to anymore is flagged rather than silently never shown.
#[test]
fn notes_follow_rename_and_stay_on_their_symbol() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-it-notes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("s.rs"),
        "struct A;\nimpl A {\n    fn run(&self) {}\n}\nstruct B;\nimpl B {\n    fn run(&self) {}\n}\nfn gone() {}\nfn main() { A.run(); }\n",
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str]| -> String {
        let o = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .env("CONA_NO_STATS", "1")
            .current_dir(&dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr)
    };
    run(&["index"]);
    run(&["note", "A.run", "hot path, keep alloc-free"]);
    run(&["note", "gone", "about to be deleted"]);

    assert!(run(&["show", "A.run"]).contains("alloc-free"));
    assert!(
        !run(&["show", "B.run"]).contains("alloc-free"),
        "note leaked to sibling"
    );

    let out = run(&["rename", "A.run", "go"]);
    assert!(out.contains("moved 1 note to A.go"), "{out}");
    assert!(run(&["show", "A.go"]).contains("alloc-free"));

    std::fs::write(
        dir.join("s.rs"),
        std::fs::read_to_string(dir.join("s.rs"))
            .unwrap()
            .replace("fn gone() {}\n", ""),
    )
    .unwrap();
    run(&["index"]);
    let list = run(&["note"]);
    assert!(
        list.lines()
            .any(|l| l.contains("gone") && l.contains("dangling")),
        "{list}"
    );
    assert!(
        list.lines()
            .any(|l| l.contains("A.go") && !l.contains("dangling")),
        "{list}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Recurring agent slips mined from transcripts: each must answer (or name the
// fix) instead of erroring into a dead end.
#[test]
fn agent_slips_recover() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-slip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(
        dir.join("a.rs"),
        "pub fn alpha() {}\npub fn beta() { alpha(); }\n",
    )
    .unwrap();
    std::fs::write(dir.join("sub/b.rs"), "fn gamma() { crate::alpha(); }\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str]| -> (String, bool) {
        let o = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .env("CONA_NO_STATS", "1")
            .current_dir(&dir)
            .output()
            .unwrap();
        let mut s = String::from_utf8_lossy(&o.stdout).to_string();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        (s, o.status.success())
    };
    run(&["index"]);

    // line range on a basename / path suffix
    let (out, ok) = run(&["show", "b.rs:1"]);
    assert!(
        ok && out.contains("sub/b.rs:1-1") && out.contains("gamma"),
        "{out}"
    );
    // wrong directory → did-you-mean, not "symbol '1' not found"
    let (out, ok) = run(&["show", "lib/b.rs:1"]);
    assert!(!ok && out.contains("did you mean sub/b.rs"), "{out}");
    let (out, ok) = run(&["outline", "lib/b.rs"]);
    assert!(!ok && out.contains("did you mean sub/b.rs"), "{out}");

    // symbol misses inline their closest names
    let (out, ok) = run(&["show", "alph"]);
    assert!(
        !ok && out.contains("closest:") && out.contains("alpha  a.rs:1"),
        "{out}"
    );
    let (out, _) = run(&["show", "Nope.gamma"]);
    assert!(out.contains("gamma  sub/b.rs:1"), "{out}");

    // regex syntax under the literal default retries as a regex, and says so
    let (out, ok) = run(&["grep", r"\balpha\b", "-l"]);
    assert!(
        ok && out.contains("as a regex") && out.contains("a.rs"),
        "{out}"
    );
    // grep's positional paths and a no-op `-n`
    let (out, ok) = run(&["grep", "alpha", "sub", "-n"]);
    assert!(
        ok && out.contains("sub/b.rs") && !out.contains("a.rs:1"),
        "{out}"
    );

    // refs takes several names and -l/-c shapes
    let (out, ok) = run(&["refs", "-c", "alpha", "gamma"]);
    assert!(
        ok && out.contains("── alpha") && out.contains("── gamma"),
        "{out}"
    );
    let (out, ok) = run(&["refs", "-l", "alpha"]);
    assert!(ok && out.lines().any(|l| l == "sub/b.rs"), "{out}");

    // a bare file name outlines the one indexed file ending in it
    let (out, ok) = run(&["show", "b.rs"]);
    assert!(
        ok && out.starts_with("sub/b.rs") && out.contains("gamma"),
        "{out}"
    );
    // call / Swift-selector syntax resolves to the bare symbol
    let (out, ok) = run(&["show", "alpha()"]);
    assert!(ok && out.contains("a.rs:1"), "{out}");
    let (out, ok) = run(&["show", "a.rs:beta(x:)"]);
    assert!(ok && out.contains("pub fn beta"), "{out}");
    // `grep a b` with b no path: either name, with a note
    let (out, ok) = run(&["grep", "beta", "gamma"]);
    assert!(
        ok && out.contains("a.rs:2") && out.contains("sub/b.rs:1") && out.contains("note:"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Feedback-log fixes: `find` falls back to file paths, `dead` lists
/// unreferenced symbols, `grep` folds generated files behind a note.
#[test]
fn find_files_dead_code_and_generated_grep() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-dead-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/routes")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn used() {}\nfn orphan() {}\nfn only_tested() {}\n\
         const NAME: &str = \"x\";\nfn caller() -> String { used(); format!(\"{NAME}\") }\n\
         struct S;\nimpl Drop for S { fn drop(&mut self) {} }\n\
         fn main() { caller(); }\n",
    )
    .unwrap();
    std::fs::write(dir.join("tests/t.rs"), "fn t() { only_tested(); }\n").unwrap();
    std::fs::write(
        dir.join("src/openai_oauth.ts"),
        "export function login() { return 1 }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/routes/page.ts"),
        "export const routeName = 'home'\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/routeTree.gen.ts"),
        "export const all = ['routeName']\n",
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str]| -> (String, bool) {
        let o = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .env("CONA_NO_STATS", "1")
            .env("CONA_NO_AUTO_UPDATE", "1")
            .current_dir(&dir)
            .output()
            .unwrap();
        let mut s = String::from_utf8_lossy(&o.stdout).to_string();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        (s, o.status.success())
    };
    run(&["index"]);

    let (out, ok) = run(&["find", "openai-oauth"]);
    assert!(ok && out.contains("file src/openai_oauth.ts"), "{out}");
    let (out, _) = run(&["find", "--kind", "fn", "openai-oauth"]);
    assert!(!out.contains("file src/openai_oauth.ts"), "{out}");

    let (out, ok) = run(&["dead"]);
    assert!(ok, "{out}");
    let (unref, tests_only) = out.split_once("referenced only from tests").expect(&out);
    assert!(unref.contains("fn orphan"), "{out}");
    assert!(tests_only.contains("fn only_tested"), "{out}");
    for alive in [
        "fn used",
        "fn caller",
        "NAME",
        "fn main",
        "S.drop",
        "fn drop",
    ] {
        assert!(!out.contains(alive), "{alive} wrongly dead:\n{out}");
    }

    let (out, ok) = run(&["grep", "routeName", "-l"]);
    assert!(ok && out.contains("src/routes/page.ts"), "{out}");
    assert!(
        out.contains("skipped 1 generated file(s): src/routeTree.gen.ts"),
        "{out}"
    );
    let (out, _) = run(&["grep", "all", "--path", "src/routeTree.gen.ts"]);
    assert!(out.contains("routeTree.gen.ts:1"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hook_lets_single_file_grep_through_even_before_the_file_exists() {
    let dir = std::env::temp_dir().join(format!("cona-hookgrep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn locate_fresh() {}\n").unwrap();
    // macOS temp is behind a symlink (/var -> /private/var); a harness cwd is
    // getcwd()-canonical, so hand the hook the same form `index` hashed.
    // Not on Windows: canonicalize yields a `\\?\` verbatim path there.
    let dir = if cfg!(windows) {
        dir
    } else {
        dir.canonicalize().unwrap()
    };
    // the hook only redirects inside a project root (a `.git`)
    git_in(&dir, &["init", "-q"]);
    run_in(&dir, &["index"]);
    let hook = |cmd: &str| hook_in(&dir, cmd);
    // the diff does not exist yet (`>` runs after the hook): one file, not a tree
    for cmd in [
        "git diff > d.diff && grep -n locate_fresh d.diff",
        "grep -n locate_fresh notes.txt",
    ] {
        assert!(!hook(cmd).contains("\"deny\""), "{cmd}");
    }
    assert!(hook("grep -rn locate_fresh src").contains("\"deny\""));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--path` is a shell path: `.` widens instead of matching nothing, a
/// CWD-relative path resolves from a subdirectory, a bare dir name scopes
/// `show`, a scope that names nothing says so — and `refs -C` prints context.
#[test]
fn path_scopes_resolve_like_shell_paths_and_refs_takes_context() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-path-scope-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join("src/a")).unwrap();
    std::fs::create_dir_all(dir.join("src/b")).unwrap();
    std::fs::write(
        dir.join("src/a/m.rs"),
        "pub fn alpha() {}\n\nfn main() {\n    alpha();\n}\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/b/m.rs"), "pub fn beta() {}\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |cwd: &std::path::Path, args: &[&str]| -> String {
        let o = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .current_dir(cwd)
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    };
    run(&dir, &["index"]);
    let sub = dir.join("src");

    // `--path .` = the root (or the CWD's directory), never "nothing"
    let out = run(&dir, &["show", "alpha", "--path", "."]);
    assert!(out.contains("src/a/m.rs:1"), "{out}");
    let out = run(&sub, &["show", "alpha", "--path", "."]);
    assert!(out.contains("src/a/m.rs:1"), "{out}");
    // a bare directory name scopes show
    let out = run(&dir, &["show", "alpha", "--path", "src"]);
    assert!(out.contains("src/a/m.rs:1"), "{out}");
    // CWD-relative --path and positional path from a subdirectory
    let out = run(&sub, &["grep", "alpha", "--path", "a/m.rs"]);
    assert!(out.contains("src/a/m.rs:1"), "{out}");
    let out = run(&sub, &["grep", "alpha", "a", "-l"]);
    assert_eq!(out.trim(), "src/a/m.rs", "{out}");
    // root-relative habit from a subdirectory still resolves
    let out = run(&sub, &["grep", "alpha", "--path", "src/a"]);
    assert!(out.contains("src/a/m.rs:1"), "{out}");
    // --path narrows an ambiguous bare file name in a line range
    let out = run(&dir, &["show", "m.rs:1-1", "--path", "src/b"]);
    assert!(out.contains("beta") && !out.contains("ambiguous"), "{out}");
    // a scope that names nothing is reported, not passed off as "no matches here"
    let out = run(&dir, &["grep", "alpha", "--path", "src/nope"]);
    assert!(
        out.contains("'src/nope' names no indexed file or directory"),
        "{out}"
    );
    // refs -C: grep's block format, `>` on the site
    let out = run(&dir, &["refs", "alpha", "-C", "1"]);
    // overlapping windows merge under one header
    assert_eq!(out.matches("src/a/m.rs:").count(), 1, "{out}");
    assert!(out.contains("    4>     alpha();"), "{out}");
    assert!(out.contains("    3  fn main() {"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Temp project + a `run` closure over the built binary (ok, stdout+stderr).
fn temp_project(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cona-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, body) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    dir
}

/// `git <args>` in `dir` under a throwaway identity.
fn git_in(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

/// The PreToolUse hook's stdout for a shell `command` run in `dir`.
fn hook_in(dir: &std::path::Path, command: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let payload = serde_json::json!({"session_id": "t", "cwd": dir, "tool_name": "Bash",
        "tool_input": {"command": command}});
    let mut c = Command::new(env!("CARGO_BIN_EXE_cona"))
        .args(["hook", "PreToolUse"])
        .env("CONA_DATA_DIR", dir.join(".cona-data"))
        .env("CONA_NO_STATS", "1")
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    String::from_utf8_lossy(&c.wait_with_output().unwrap().stdout).to_string()
}

fn run_in(dir: &std::path::Path, args: &[&str]) -> (bool, String) {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_cona"))
        .args(args)
        .env("CONA_DATA_DIR", dir.join(".cona-data"))
        .env("CONA_NO_STATS", "1")
        .current_dir(dir)
        .output()
        .unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
    )
}

#[test]
fn show_skips_test_block_labels_and_test_only_twins() {
    let dir = temp_project(
        "testtwins",
        &[
            ("src/foo.js", "export function foo() {\n  return 1;\n}\n"),
            (
                "src/foo.test.js",
                "describe(\"foo\", () => {\n  it(\"works\", () => {});\n});\n",
            ),
            (
                "src/settings.ts",
                "export function getSetting(k: string) {\n  return k;\n}\n",
            ),
            (
                "tests/helpers.ts",
                "export function getSetting(k: string) {\n  return 'mock';\n}\n",
            ),
            ("src/a.ts", "export function dup() {\n  return 1;\n}\n"),
            ("src/b.ts", "export function dup() {\n  return 2;\n}\n"),
        ],
    );
    run_in(&dir, &["index"]);
    // a describe() label is not a second definition
    let (ok, out) = run_in(&dir, &["show", "foo"]);
    assert!(ok, "{out}");
    assert!(out.contains("src/foo.js:1-3"), "{out}");
    assert!(!out.contains("ambiguous"), "{out}");
    // real def + test-only twin → the real one, with every candidate disclosed
    let (ok, out) = run_in(&dir, &["show", "getSetting"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("showing the one non-test definition — 1 more in tests"),
        "{out}"
    );
    assert!(out.contains("src/settings.ts:1-3"), "{out}");
    assert!(
        out.contains("also matched:") && out.contains("tests/helpers.ts"),
        "{out}"
    );
    assert!(!out.contains("'mock'"), "{out}");
    // --all still prints both
    let (ok, out) = run_in(&dir, &["show", "getSetting", "--all"]);
    assert!(ok && out.contains("'mock'"), "{out}");
    // two real definitions stay an ambiguity (inv. 4)
    let (ok, out) = run_in(&dir, &["show", "dup"]);
    assert!(ok, "{out}");
    assert!(out.contains("ambiguous — showing all 2"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn outline_and_show_index_files_created_after_the_walk() {
    let dir = temp_project("unseen", &[("src/a.ts", "export function a() {}\n")]);
    run_in(&dir, &["index"]);
    std::fs::write(
        dir.join("src/fresh.ts"),
        "export function fresh() {\n  return 1;\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/late.ts"),
        "export function late() {\n  return 2;\n}\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("build")).unwrap();
    std::fs::write(dir.join("build/x.js"), "function x() {}\n").unwrap();

    let (ok, out) = run_in(&dir, &["outline", "src/fresh.ts"]);
    assert!(ok, "{out}");
    assert!(out.contains("fresh :1-3"), "{out}");
    let (ok, out) = run_in(&dir, &["show", "src/late.ts:late"]);
    assert!(ok, "{out}");
    assert!(out.contains("return 2"), "{out}");
    // excluded paths say why instead of pointing at `cona index`
    let (ok, out) = run_in(&dir, &["outline", "build/x.js"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("not indexed") && out.contains("build/"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grep_dash_pattern_names_the_double_dash_fix() {
    let dir = temp_project(
        "dashgrep",
        &[("s.css", ".a { -webkit-appearance: none; }\n")],
    );
    // a pattern that is itself a flag name: clap sees the flag twice
    let (ok, out) = run_in(&dir, &["grep", "-e", "--regex"]);
    assert!(
        !ok && out.contains("cona grep [flags] -- '--regex'"),
        "{out}"
    );
    run_in(&dir, &["index"]);
    let (ok, out) = run_in(&dir, &["grep", "-webkit"]);
    assert!(!ok, "{out}");
    assert!(out.contains("cona grep [flags] -- -webkit"), "{out}");
    let (ok, out) = run_in(&dir, &["grep", "--", "-webkit"]);
    assert!(ok && out.contains("s.css:1"), "{out}");
    // known short flags still parse as flags
    let (ok, out) = run_in(&dir, &["grep", "-il", "WEBKIT"]);
    assert!(ok && out.contains("s.css"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

// Feedback-log slips, each answered instead of erroring: a line range on a
// symbol, `--raw` for piping, `edit --file <the target file>`, refs on a
// receiver-qualified name, a barrel outline, scss search, a stale file list.
#[test]
fn feedback_slips_are_answered() {
    let dir = temp_project(
        "slips",
        &[
            (
                "src/a.ts",
                "export function alpha() {\n  return 1;\n}\n\nexport function beta() {\n  const x = alpha();\n  return x;\n}\n",
            ),
            ("src/index.ts", "export { alpha } from './a';\nexport * from './b';\n"),
            ("src/b.ts", "export const svc = { go() { return 1; } };\nexport function useIt() { return svc.go(); }\n"),
            ("src/q.scss", "$brand: red;\n.card { color: $brand; }\n"),
            ("src/gone.ts", "export function vanished() {}\n"),
        ],
    );
    run_in(&dir, &["index"]);

    // Sym:A-B — relative to the symbol when the range lies above it
    let (ok, out) = run_in(&dir, &["show", "beta:1-2"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("lines 1-2 of beta") && out.contains("const x = alpha()"),
        "{out}"
    );
    assert!(!out.contains("return x"), "{out}");
    // absolute file lines when they fall inside the symbol
    let (ok, out) = run_in(&dir, &["show", "beta:7-7"]);
    assert!(
        ok && out.contains("return x") && !out.contains("const x"),
        "{out}"
    );

    // --raw: source only, no header, no line numbers
    let (ok, out) = run_in(&dir, &["show", "alpha", "--raw"]);
    assert!(ok, "{out}");
    assert_eq!(out, "export function alpha() {\n  return 1;\n}\n");

    // edit --file naming the DEFINING file is the classic slip: explain, don't
    // splice the whole file into the symbol
    let before = std::fs::read_to_string(dir.join("src/a.ts")).unwrap();
    let (ok, out) = run_in(&dir, &["edit", "alpha", "--file", "src/a.ts"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("DEFINES alpha") && out.contains("< new-code"),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("src/a.ts")).unwrap(),
        before
    );

    // refs on a receiver-qualified name falls back to the bare name, labeled
    let (ok, out) = run_in(&dir, &["refs", "Nope.go"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("sites of 'go' by name") && out.contains("src/b.ts"),
        "{out}"
    );

    // a barrel has no symbols — print it instead of an empty outline
    let (ok, out) = run_in(&dir, &["outline", "src/index.ts"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("no symbols") && out.contains("export * from './b'"),
        "{out}"
    );

    // scss is searchable
    let (ok, out) = run_in(&dir, &["grep", "$brand", "--path", "src/q.scss"]);
    assert!(
        ok && out.contains("src/q.scss:1") && out.contains("src/q.scss:2"),
        "{out}"
    );

    // a file deleted + another gaining the name since the walk: the empty
    // answer triggers one re-index instead of a false "no matches"
    std::fs::remove_file(dir.join("src/gone.ts")).unwrap();
    std::fs::write(dir.join("src/new.ts"), "export const freshName = 1;\n").unwrap();
    let (ok, out) = run_in(&dir, &["grep", "freshName"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("re-indexed") && out.contains("src/new.ts:1"),
        "{out}"
    );

    // refs and find answer a moved file the same way
    std::fs::remove_file(dir.join("src/new.ts")).unwrap();
    std::fs::write(
        dir.join("src/moved.ts"),
        "export function movedFn() {}\nmovedFn();\n",
    )
    .unwrap();
    let (ok, out) = run_in(&dir, &["refs", "movedFn"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("re-indexed") && out.contains("src/moved.ts"),
        "{out}"
    );
    std::fs::remove_file(dir.join("src/moved.ts")).unwrap();
    std::fs::write(dir.join("src/again.ts"), "export function againFn() {}\n").unwrap();
    let (ok, out) = run_in(&dir, &["find", "againFn"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("re-indexed") && out.contains("src/again.ts"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An extension-less script is indexed by its `#!` line; an extension-less
/// non-script still names why it is skipped.
#[test]
fn shebang_scripts_are_indexed() {
    let dir = temp_project(
        "shebang",
        &[
            (
                "bin/tool",
                "#!/usr/bin/env python3\ndef greet(name):\n    return \"hi \" + name\n\ndef main():\n    print(greet(\"x\"))\n",
            ),
            ("bin/run", "#!/bin/sh\nbuild() {\n  echo build\n}\nbuild\n"),
            ("LICENSE", "just text\n"),
            ("fix.txt", "def greet(name):\n    return \"hello \" + name\n"),
            ("bad.txt", "def greet(name:\n"),
        ],
    );
    run_in(&dir, &["index"]);

    let (ok, out) = run_in(&dir, &["outline", "bin/tool", "bin/run"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("def greet") && out.contains("def main") && out.contains("fn build"),
        "{out}"
    );
    let (ok, out) = run_in(&dir, &["refs", "greet"]);
    assert!(ok && out.contains("print(greet(\"x\"))"), "{out}");

    // edit is syntax-verified with the shebang's grammar
    let (ok, out) = run_in(&dir, &["edit", "greet", "--file", "bad.txt"]);
    assert!(!ok && out.contains("edit rejected"), "{out}");
    let (ok, out) = run_in(&dir, &["edit", "greet", "--file", "fix.txt"]);
    assert!(ok, "{out}");
    let src = std::fs::read_to_string(dir.join("bin/tool")).unwrap();
    assert!(
        src.starts_with("#!/usr/bin/env python3\n") && src.contains("hello"),
        "{src}"
    );

    let (ok, out) = run_in(&dir, &["outline", "LICENSE"]);
    assert!(!ok && out.contains("no supported language"), "{out}");

    // created after the walk: indexed on demand like any other file
    std::fs::write(
        dir.join("bin/new"),
        "#!/usr/bin/env ruby\ndef fresh\n  1\nend\n",
    )
    .unwrap();
    let (ok, out) = run_in(&dir, &["outline", "bin/new"]);
    assert!(ok && out.contains("fresh"), "{out}");

    // show --raw / a deleted script still counts as code in `diff`
    let git = |args: &[&str]| git_in(&dir, args);
    std::fs::write(dir.join(".gitignore"), ".cona-data\n").unwrap();
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    run_in(&dir, &["index"]);
    std::fs::remove_file(dir.join("bin/run")).unwrap();
    let (ok, out) = run_in(&dir, &["diff"]);
    assert!(ok && out.contains("bin/run (deleted)"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Members of an anonymous object (`export default { setup() {} }`) are
/// reached through the object, never by name — `dead` must not list them.
#[test]
fn dead_skips_members_of_anonymous_objects() {
    let dir = temp_project(
        "deadanon",
        &[
            (
                "src/plugin.ts",
                "export default {\n  setup() {\n    return 1;\n  },\n};\n",
            ),
            (
                "src/k.ts",
                "export class K {\n  gone() {\n    return 1;\n  }\n}\nexport const k = new K();\n",
            ),
        ],
    );
    run_in(&dir, &["index"]);
    let (_, out) = run_in(&dir, &["dead"]);
    assert!(out.contains("gone") && !out.contains("setup"), "{out}");

    // a NAMED object's members live while the object is used; an unused
    // object takes its members down with it
    std::fs::write(
        dir.join("src/named.ts"),
        "const cfg = {\n  boot() {\n    return 1;\n  },\n};\nexport default cfg;\nexport const other = {\n  lost() {},\n};\n",
    )
    .unwrap();
    run_in(&dir, &["index"]);
    let (_, out) = run_in(&dir, &["dead"]);
    assert!(
        out.contains("other.lost") && !out.contains("cfg.boot"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The read hook sees an extension-less script as the code file it is.
#[test]
fn hook_redirects_a_large_shebang_script() {
    let mut body = String::from("#!/usr/bin/env python3\n");
    for i in 0..600 {
        body.push_str(&format!("def f{i}():\n    return {i}\n\n"));
    }
    let dir = temp_project("hookshebang", &[("bin/big", &body)]);
    // a harness cwd is getcwd()-canonical (macOS temp sits behind a symlink)
    let dir = if cfg!(windows) {
        dir
    } else {
        dir.canonicalize().unwrap()
    };
    // the hook only redirects inside a project root (a `.git`), and never
    // on a file with uncommitted changes
    for args in [
        &["init", "-q"][..],
        &["add", "bin"],
        &["commit", "-qm", "init"],
    ] {
        git_in(&dir, args);
    }
    run_in(&dir, &["index"]);
    let out = hook_in(&dir, "cat bin/big");
    assert!(out.contains("cona outline bin/big"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}
