//! Stage-five fixtures use a real, private Git repository and in-memory index.
use super::*;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Repository(PathBuf);

impl Repository {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cona-diff-impact-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let repo = Self(path);
        repo.git(&["init", "-q"]);
        repo.git(&["config", "user.email", "fixture@example.invalid"]);
        repo.git(&["config", "user.name", "Fixture"]);
        repo
    }

    fn git(&self, args: &[&str]) {
        let result = Command::new("git")
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    fn write(&self, file: &str, source: &str) {
        std::fs::write(self.0.join(file), source).unwrap();
    }

    fn commit(&self) {
        self.git(&["add", "."]);
        self.git(&["-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture"]);
    }

    fn index(&self) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL,
             mtime INTEGER NOT NULL, size INTEGER NOT NULL, lang TEXT NOT NULL);
             CREATE TABLE symbols(id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL,
             name TEXT NOT NULL, qualified TEXT NOT NULL, kind TEXT NOT NULL,
             parent TEXT, start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
             signature TEXT NOT NULL);
             CREATE TABLE notes(id INTEGER PRIMARY KEY, symbol TEXT, note TEXT, ts INTEGER);",
        )
        .unwrap();
        // Populate through the ordinary single-file extractor without touching
        // the user's global project registry.
        for entry in std::fs::read_dir(&self.0).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().unwrap().is_file() && lang::detect_lang(&name).is_some() {
                indexer::reindex_file(&self.0, &conn, &name).unwrap();
            }
        }
        conn
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn impact_collector_preserves_ordered_definition_pairing_contract() {
    // Explicit serialized rows pin pairing AND source order independently of
    // the production lookup structure. Update these if the identity rule changes.
    let row = |file: &str,
               symbol: Option<&str>,
               status: &str,
               old: Option<(usize, &str)>,
               new: Option<(usize, &str)>| {
        serde_json::json!({
            "file": file, "symbol": symbol, "status": status,
            "old_signature": old.map(|(_, signature)| signature),
            "new_signature": new.map(|(_, signature)| signature),
            "old_start": old.map(|(start, _)| start),
            "new_start": new.map(|(start, _)| start),
        })
    };
    let cpp = |symbol, status, old, new| row("x.cpp", Some(symbol), status, old, new);
    let cases = [
        (
            "unique pairs, reordered new definitions, additions and deletions",
            "x.cpp",
            Some("void z() {}\nvoid removed() {}\nvoid a() {}\n"),
            Some("void added() {}\nvoid a(int x) {}\nvoid z() {}\nvoid last() {}\n"),
            vec![(1, 3)],
            vec![(1, 4)],
            vec![
                cpp(
                    "z",
                    "modified",
                    Some((1, "void z() {}")),
                    Some((3, "void z() {}")),
                ),
                cpp("removed", "deleted", Some((2, "void removed() {}")), None),
                cpp(
                    "a",
                    "modified",
                    Some((3, "void a() {}")),
                    Some((2, "void a(int x) {}")),
                ),
                cpp("added", "added", None, Some((1, "void added() {}"))),
                cpp("last", "added", None, Some((4, "void last() {}"))),
            ],
        ),
        (
            "kind changes never pair",
            "x.rs",
            Some("fn Item() {}\n"),
            Some("struct Item {}\n"),
            vec![(1, 1)],
            vec![(1, 1)],
            vec![
                row(
                    "x.rs",
                    Some("Item"),
                    "deleted",
                    Some((1, "fn Item() {}")),
                    None,
                ),
                row(
                    "x.rs",
                    Some("Item"),
                    "added",
                    None,
                    Some((1, "struct Item {}")),
                ),
            ],
        ),
        (
            "same qualified name with distinct kinds pairs independently",
            "x.rs",
            Some("struct Item {}\nfn Item() {}\n"),
            Some("fn Item(x: i32) {}\nstruct Item {}\n"),
            vec![(1, 2)],
            vec![(1, 2)],
            vec![
                row(
                    "x.rs",
                    Some("Item"),
                    "modified",
                    Some((1, "struct Item {}")),
                    Some((2, "struct Item {}")),
                ),
                row(
                    "x.rs",
                    Some("Item"),
                    "modified",
                    Some((2, "fn Item() {}")),
                    Some((1, "fn Item(x: i32) {}")),
                ),
            ],
        ),
        (
            "one old definition and multiple new definitions",
            "x.cpp",
            Some("void run() {}\n"),
            Some("void run() {}\nvoid run(int x) {}\n"),
            vec![(1, 1)],
            vec![(1, 2)],
            vec![
                cpp("run", "deleted", Some((1, "void run() {}")), None),
                cpp("run", "added", None, Some((1, "void run() {}"))),
                cpp("run", "added", None, Some((2, "void run(int x) {}"))),
            ],
        ),
        (
            "multiple old definitions and one new definition",
            "x.cpp",
            Some("void run() {}\nvoid run(int x) {}\n"),
            Some("void run() {}\n"),
            vec![(1, 2)],
            vec![(1, 1)],
            vec![
                cpp("run", "deleted", Some((1, "void run() {}")), None),
                cpp("run", "deleted", Some((2, "void run(int x) {}")), None),
                cpp("run", "added", None, Some((1, "void run() {}"))),
            ],
        ),
        (
            "unchanged overload neighbors remain untouched",
            "x.cpp",
            Some("void run() {}\nvoid run(int x) {}\n"),
            Some("void run() { int changed = 1; }\nvoid run(int x) {}\n"),
            vec![(1, 1)],
            vec![(1, 1)],
            vec![
                cpp("run", "deleted", Some((1, "void run() {}")), None),
                cpp(
                    "run",
                    "added",
                    None,
                    Some((1, "void run() { int changed = 1; }")),
                ),
            ],
        ),
        (
            "whole-file addition",
            "x.cpp",
            None,
            Some("void z() {}\nvoid a() {}\n"),
            vec![],
            vec![],
            vec![
                cpp("z", "added", None, Some((1, "void z() {}"))),
                cpp("a", "added", None, Some((2, "void a() {}"))),
            ],
        ),
        (
            "whole-file deletion",
            "x.cpp",
            Some("void z() {}\nvoid a() {}\n"),
            None,
            vec![],
            vec![],
            vec![
                cpp("z", "deleted", Some((1, "void z() {}")), None),
                cpp("a", "deleted", Some((2, "void a() {}")), None),
            ],
        ),
        (
            "module header and a changed definition",
            "x.rs",
            Some("use old::Thing;\nfn z() {}\n"),
            Some("use new::Thing;\nfn z(x: i32) {}\n"),
            vec![(1, 2)],
            vec![(1, 2)],
            vec![
                row(
                    "x.rs",
                    Some("z"),
                    "modified",
                    Some((2, "fn z() {}")),
                    Some((2, "fn z(x: i32) {}")),
                ),
                row("x.rs", None, "outside_symbols", None, None),
            ],
        ),
        (
            "no-symbol files retain module evidence",
            "x.rs",
            Some("// old\n"),
            Some("// new\n"),
            vec![(1, 1)],
            vec![(1, 1)],
            vec![row("x.rs", None, "outside_symbols", None, None)],
        ),
    ];
    for (label, path, old, new, old_ranges, new_ranges, expected) in cases {
        let mut actual = Vec::new();
        collect_changes(path, old, new, &old_ranges, &new_ranges, &mut actual).unwrap();
        assert_eq!(
            serde_json::to_string(&actual).unwrap(),
            serde_json::to_string(&expected).unwrap(),
            "{label}"
        );
    }
}

#[test]
fn impact_keeps_overloaded_rows_while_aggregating_changed_targets() {
    let repo = Repository::new();
    let old = "void a() {}\nvoid b() {}\n\
void bridge() { a(); b(); }\n\
void bridge(int x) { a(); b(); }\n\
void test_overload() { a(); b(); a(); }\n\
void test_overload(int x) { a(); b(); }\n";
    repo.write("code.cpp", old);
    repo.commit();
    repo.write("code.cpp", &old.replace("() {}", "() { int changed = 1; }"));
    let conn = repo.index();
    let (json, _) = cmd_diff_impact(&repo.0, &conn, "HEAD", 8000, true).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let callers = value["callers"].as_array().unwrap();
    assert_eq!(callers.len(), 4, "{json}");
    let tests = value["tests"].as_array().unwrap();
    assert_eq!(tests.len(), 2, "{json}");
    for row in callers.iter().chain(tests) {
        assert_eq!(
            row["changed_symbols"].as_array().unwrap().len(),
            2,
            "{json}"
        );
    }
    let selectors: HashSet<_> = callers
        .iter()
        .map(|r| r["selector"].as_str().unwrap())
        .collect();
    assert_eq!(selectors.len(), 4);
}

#[test]
fn impact_git_old_signatures_deletions_and_module_changes() {
    let repo = Repository::new();
    repo.write(
        "code.rs",
        "use old::Thing;\nfn changed(x: i32) {}\nfn removed() {}\nfn stable() {}\n",
    );
    repo.write("gone.rs", "fn vanished() {}\n");
    repo.commit();
    repo.write(
        "code.rs",
        "use new::Thing;\nfn changed(x: i32, y: i32) {}\nfn stable() {}\n",
    );
    std::fs::remove_file(repo.0.join("gone.rs")).unwrap();
    repo.write("added.rs", "fn added() {}\n");
    let conn = repo.index();
    let (json, _) = cmd_diff_impact(&repo.0, &conn, "HEAD", 8000, true).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let rows = value["changes"].as_array().unwrap();
    let modified = rows.iter().find(|r| r["symbol"] == "changed").unwrap();
    assert_eq!(modified["status"], "modified");
    assert!(modified["old_signature"]
        .as_str()
        .unwrap()
        .contains("x: i32"));
    assert!(!modified["old_signature"]
        .as_str()
        .unwrap()
        .contains("y: i32"));
    assert!(modified["new_signature"]
        .as_str()
        .unwrap()
        .contains("y: i32"));
    for name in ["removed", "vanished"] {
        assert!(
            rows.iter()
                .any(|r| r["symbol"] == name && r["status"] == "deleted"),
            "{json}"
        );
    }
    assert!(rows
        .iter()
        .any(|r| r["symbol"] == "added" && r["status"] == "added"));
    assert!(rows
        .iter()
        .any(|r| r["file"] == "code.rs" && r["status"] == "outside_symbols"));
    assert!(!rows.iter().any(|r| r["symbol"] == "stable"), "{json}");
    let (text, _) = cmd_diff_impact(&repo.0, &conn, "HEAD", 8000, false).unwrap();
    assert!(text.contains("old:") && text.contains("new:") && text.contains("module-level"));
    // The default API still has its original JSON array shape and no signatures.
    let (default, _) = cmd_diff(&repo.0, &conn, "HEAD", true).unwrap();
    let default: serde_json::Value = serde_json::from_str(&default).unwrap();
    assert!(default.is_array());
    assert!(!default.to_string().contains("old_signature"));
}

#[test]
fn impact_deduplicates_shared_callers_and_tests_and_bounds_output() {
    let repo = Repository::new();
    repo.write(
        "lib.rs",
        "fn a() {}\nfn b() {}\nfn bridge() { a(); b(); }\nfn test_shared() { bridge(); }\n",
    );
    repo.commit();
    repo.write("lib.rs", "fn a(x: i32) {}\nfn b(y: i32) {}\nfn bridge() { a(); b(); }\nfn test_shared() { bridge(); }\n");
    let conn = repo.index();
    let (json, _) = cmd_diff_impact(&repo.0, &conn, "HEAD", 8000, true).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let callers = value["callers"].as_array().unwrap();
    assert_eq!(
        callers.iter().filter(|r| r["symbol"] == "bridge").count(),
        1,
        "{json}"
    );
    let bridge = callers.iter().find(|r| r["symbol"] == "bridge").unwrap();
    assert_eq!(bridge["changed_symbols"].as_array().unwrap().len(), 2);
    let tests = value["tests"].as_array().unwrap();
    assert_eq!(
        tests.iter().filter(|r| r["in"] == "test_shared").count(),
        1,
        "{json}"
    );
    for json in [true, false] {
        let (output, _) = cmd_diff_impact(&repo.0, &conn, "HEAD", 150, json).unwrap();
        assert!(db::est_tokens(output.len()) <= 150, "{output}");
        assert!(
            output.contains("omitted") || output.contains("Evidence omitted"),
            "{output}"
        );
        if json {
            assert!(serde_json::from_str::<serde_json::Value>(&output).is_ok());
        }
        let error = cmd_diff_impact(&repo.0, &conn, "HEAD", 1, json).unwrap_err();
        assert!(error.to_string().contains("--budget too small"));
    }
}

#[test]
fn impact_empty_hunk_sides_do_not_touch_neighbor_definitions() {
    let (old, new) = impact_ranges("@@ -2 +1,0 @@\n-gone\n@@ -4,0 +4,2 @@\n+a\n+b\n");
    assert_eq!(old, [(2, 2)]);
    assert_eq!(new, [(4, 5)]);
}

#[test]
fn impact_does_not_infer_renames() {
    let mut rows = Vec::new();
    collect_changes(
        "x.rs",
        Some("fn old_name() {}\n"),
        Some("fn new_name() {}\n"),
        &[(1, 1)],
        &[(1, 1)],
        &mut rows,
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].status, "deleted");
    assert_eq!(rows[1].status, "added");
}

#[test]
fn impact_deleted_definition_retains_uncertain_callers_and_test_paths() {
    let repo = Repository::new();
    repo.write(
        "lib.rs",
        "fn removed() {}\nfn bridge() { removed(); }\nfn test_deleted() { bridge(); }\n",
    );
    repo.commit();
    repo.write(
        "lib.rs",
        "fn bridge() { removed(); }\nfn test_deleted() { bridge(); }\n",
    );
    let conn = repo.index();
    let (json, _) = cmd_diff_impact(&repo.0, &conn, "HEAD", 8000, true).unwrap();
    let report: serde_json::Value = serde_json::from_str(&json).unwrap();
    let caller = report["callers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["symbol"] == "bridge")
        .unwrap();
    assert_eq!(caller["uncertain"], true);
    let test = report["tests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["in"] == "test_deleted")
        .unwrap();
    assert_eq!(test["ambiguous"], true, "{json}");
    assert_eq!(test["depth"], 2);
    assert_eq!(test["path"].as_array().unwrap().len(), 2);
    assert_eq!(test["changed_symbols"][0], "lib.rs:removed");
}

#[test]
fn impact_preserves_container_header_changes_and_does_not_guess_overloads() {
    let mut rows = Vec::new();
    collect_changes(
        "x.rs",
        Some("mod old_module {\nfn inner() {}\n}\n"),
        Some("mod new_module {\nfn inner() {}\n}\n"),
        &[(1, 1)],
        &[(1, 1)],
        &mut rows,
    )
    .unwrap();
    assert!(rows
        .iter()
        .any(|r| r.symbol.as_deref() == Some("old_module") && r.status == "deleted"));
    assert!(rows
        .iter()
        .any(|r| r.symbol.as_deref() == Some("new_module") && r.status == "added"));

    rows.clear();
    collect_changes(
        "x.cpp",
        Some("int overloaded(int a) { return a; }\nint overloaded() { return 0; }\n"),
        Some("int overloaded(double a) { return a; }\nint overloaded() { return 0; }\n"),
        &[(1, 1)],
        &[(1, 1)],
        &mut rows,
    )
    .unwrap();
    assert_eq!(rows.iter().filter(|r| r.status == "modified").count(), 0);
    assert!(rows.iter().any(|r| r.status == "added"));
    assert!(rows.iter().any(|r| r.status == "deleted"));
}

#[test]
fn impact_shared_test_aggregates_deleted_target_ambiguity() {
    let repo = Repository::new();
    // The live definition comes first, so its unambiguous evidence seeds the
    // deduplicated row before the deleted target contributes an uncertain path.
    repo.write(
        "lib.rs",
        "fn live() {}\nfn removed() {}\nfn test_shared() { live(); removed(); }\n",
    );
    repo.commit();
    repo.write(
        "lib.rs",
        "fn live(x: i32) {}\nfn test_shared() { live(); removed(); }\n",
    );
    let conn = repo.index();
    let (json, _) = cmd_diff_impact(&repo.0, &conn, "HEAD", 8000, true).unwrap();
    let report: serde_json::Value = serde_json::from_str(&json).unwrap();
    let tests = report["tests"].as_array().unwrap();
    assert_eq!(tests.len(), 1, "{json}");
    let test = &tests[0];
    assert_eq!(test["ambiguous"], true, "{json}");
    let paths = test["supporting_paths"].as_array().unwrap();
    assert_eq!(paths.len(), 2, "{json}");
    for (target, ambiguous) in [("lib.rs:live", false), ("lib.rs:removed", true)] {
        let evidence = paths
            .iter()
            .find(|row| row["changed_symbol"] == target)
            .unwrap();
        assert_eq!(evidence["path"][0]["ambiguous"], ambiguous, "{json}");
    }
}

#[test]
fn impact_clean_repository_returns_empty_evidence() {
    let repo = Repository::new();
    repo.write("lib.rs", "fn stable() {}\n");
    repo.commit();
    let conn = repo.index();
    let (json, _) = cmd_diff_impact(&repo.0, &conn, "HEAD", 8000, true).unwrap();
    let report: serde_json::Value = serde_json::from_str(&json).unwrap();
    for key in ["changes", "callers", "tests"] {
        assert!(report[key].as_array().unwrap().is_empty());
    }
    assert_eq!(report["omitted"], false);
    assert!(report["note"]
        .as_str()
        .unwrap()
        .contains("not actual failures"));
}
