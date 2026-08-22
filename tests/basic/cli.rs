//! Drive the real binary against real temp repos (CLI contract tests).

// Drive the real binary against a real temp repo — matches the recovery-bug
// test philosophy (real IO). Covers edit --range, insert --after, and the
// syntax-verify rollback shared by both.
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

// `show --all` renders every candidate of an ambiguous name — including the
// same-file enum + impl pair, where the `file:Name` escape hatch cannot
// disambiguate. Pins the guide/skill/MCP promise ("--all prints every
// definition instead of erroring") and the honest ambiguity message: hatches
// that cannot separate the pool (file/Parent.Name for a same-file pair) are
// not suggested.
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
    // without --all a SMALL ambiguity (≤3 candidates, ≤400 lines total)
    // auto-renders every definition instead of erroring — a dead-end error
    // that --all immediately fixes was pure friction. The banner still names
    // the narrowing hatches.
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

// Grouped subcommands (`nav show`) and their flat aliases (`show`) dispatch to
// the same operation and produce identical output. Pins the CLI grouping
// contract: flat forms stay backward-compatible forever.
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
        Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
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
/// numbers as if they were current (invariant 2). `show` fails with a message
/// naming the file and the fix — NOT rusqlite's "attempt to write a readonly
/// database" — and `outline`, which still prints its indexed ranges, labels
/// them stale. The writable run afterwards proves the refusal is scoped to
/// read-only mode and normal use still self-heals.
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

    // `show` reports per-symbol failures on stdout so one bad name cannot abort
    // a multi-symbol batch — the message is what matters, not the stream.
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

/// The SessionStart hook fires unattended in whatever directory the harness
/// happens to be in. When that is $HOME, walking the tree is never what anyone
/// asked for — several agent sessions launched from the home directory each
/// started a multi-hundred-MB walk of the whole home tree. `--session-start`
/// must bail out there, and quietly: the hook is fail-open, so it exits 0 and
/// emits no context rather than failing a session over a missing index.
///
/// Unix-only because of the harness, not the behaviour: faking a home directory
/// means overriding `dirs::home_dir`, which reads `$HOME` on unix but calls the
/// Win32 known-folder API on Windows — no environment variable can redirect it,
/// so the child would compare the temp dir against the runner's real profile
/// and index happily. The guard itself is platform-neutral.
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

    // A typed `cona index` in $HOME stays allowed — that is a deliberate act,
    // and it warns rather than refusing.
    let out = run(&["index"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("home/filesystem root"),
        "explicit index in $HOME should warn: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&home);
}
