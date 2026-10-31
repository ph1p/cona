use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    root: std::path::PathBuf,
    conn: Connection,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "cona-selection-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT UNIQUE,mtime INTEGER,size INTEGER,lang TEXT);
             CREATE TABLE symbols(id INTEGER PRIMARY KEY,file_id INTEGER,name TEXT,qualified TEXT,kind TEXT,
                parent TEXT,start_line INTEGER,end_line INTEGER,signature TEXT);"
        ).unwrap();
        for (file, source) in files {
            std::fs::write(root.join(file), source).unwrap();
            crate::indexer::reindex_file(&root, &conn, file).unwrap();
        }
        Self { root, conn }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn overloaded_tests_keep_definition_identity_and_reference_lines() {
    let f = Fixture::new(&[(
        "code.cpp",
        "void target() {}\n\
void test_overload() {\n target();\n target();\n}\n\
void test_overload(int a) {\n target();\n}\n\
void test_overload(int a, int b) {\n target();\n}\n\
void test_overload(int a, int b, int c) {\n target();\n}\n",
    )]);
    for depth in [1, 2] {
        let selection = select_tests(&f.root, &f.conn, "target", depth).unwrap();
        assert_eq!(selection.tests.len(), 4, "{selection:?}");
        for (test, (start, end, reference)) in
            selection
                .tests
                .iter()
                .zip([(2, 5, 3), (6, 8, 7), (9, 11, 10), (12, 14, 13)])
        {
            assert_eq!(test.selector, format!("code.cpp:{start}-{end}"));
            assert_eq!(test.line, reference);
            assert_eq!(test.path[0].line, reference);
            assert!(
                !test.ambiguous,
                "selector ambiguity is not target ambiguity"
            );
        }
    }
}

#[test]
fn indirect_overloaded_tests_keep_distinct_definition_ranges() {
    let f = Fixture::new(&[(
        "code.cpp",
        "void target() {}\n\
void bridge() { target(); }\n\
void test_overload() {\n bridge();\n bridge();\n}\n\
void test_overload(int a) {\n bridge();\n}\n\
void test_overload(int a, int b) {\n bridge();\n}\n\
void test_overload(int a, int b, int c) {\n bridge();\n}\n",
    )]);
    let selection = select_tests(&f.root, &f.conn, "target", 2).unwrap();
    assert_eq!(selection.tests.len(), 4, "{selection:?}");
    for (test, (start, end, reference)) in
        selection
            .tests
            .iter()
            .zip([(3, 6, 4), (7, 9, 8), (10, 12, 11), (13, 15, 14)])
    {
        assert_eq!(test.selector, format!("code.cpp:{start}-{end}"));
        assert_eq!(test.line, reference);
        assert_eq!(test.path[1].line, reference);
        assert_eq!(test.depth, 2);
        assert!(!test.ambiguous);
    }
}

#[test]
fn unenclosed_test_references_have_explicit_line_identity() {
    let f = Fixture::new(&[
        ("code.py", "def target():\n    pass\n"),
        ("test_calls.py", "target()\ntarget()\n"),
    ]);
    let selection = select_tests(&f.root, &f.conn, "target", 2).unwrap();
    assert_eq!(selection.tests.len(), 2, "{selection:?}");
    for (test, line) in selection.tests.iter().zip([1, 2]) {
        assert!(test.enclosing.is_empty());
        assert_eq!(test.line, line);
        assert_eq!(test.selector, format!("test_calls.py:{line}-{line}"));
    }
}

#[test]
fn overloaded_bridge_seeds_follow_the_referencing_definition() {
    let f = Fixture::new(&[(
        "code.cpp",
        "void target() {}\n\
void bridge() {}\n\
void bridge(int a) { target(); }\n\
void test_wrong() { bridge(); }\n\
void test_right() { bridge(1); }\n",
    )]);
    let selection = select_tests(&f.root, &f.conn, "target", 2).unwrap();
    // Reverse references are deliberately name-based, not arity resolution.
    // Both uncertain paths must go through the actual target-referencing bridge.
    assert_eq!(selection.tests.len(), 2, "{selection:?}");
    for test in &selection.tests {
        assert_eq!(test.depth, 2);
        assert_eq!(test.path[0].from, "code.cpp:3-3");
        assert_eq!(test.path[1].to, "code.cpp:3-3");
        assert!(test.ambiguous);
    }
}

#[test]
fn direct_indirect_cycle_and_depth_boundary() {
    let f = Fixture::new(&[(
        "lib.rs",
        "\
fn target() {
    helper();
}
fn helper() {
    target();
}
fn wrapper() {
    helper();
}
fn test_direct() {
    target();
    target();
}
fn test_indirect() {
    wrapper();
}
fn untouched() {}
",
    )]);
    let one = select_tests(&f.root, &f.conn, "target", 1).unwrap();
    assert_eq!(one.tests.len(), 1);
    assert_eq!(one.tests[0].enclosing, "test_direct");
    assert_eq!(one.tests[0].depth, 1);
    assert!(!one.tests[0].ambiguous);
    let two = select_tests(&f.root, &f.conn, "target", 2).unwrap();
    assert_eq!(two.tests.len(), 1);
    let selector = TestSelector::new(&f.root, &f.conn, 3).unwrap();
    let three = selector.select("target").unwrap();
    assert_eq!(three.tests.len(), 2);
    let indirect = three
        .tests
        .iter()
        .find(|e| e.enclosing == "test_indirect")
        .unwrap();
    assert_eq!(indirect.depth, 3);
    assert_eq!(indirect.path.len(), 3);
    assert_eq!(indirect.path[0].to, "lib.rs:target");
    assert_eq!(indirect.path[2].from, "lib.rs:test_indirect");
    assert!(selector.graph().is_some());
    assert_eq!(selector.select("target").unwrap().tests.len(), 2);
    assert!(select_tests(&f.root, &f.conn, "untouched", 8)
        .unwrap()
        .tests
        .is_empty());
    assert!(select_tests(&f.root, &f.conn, "missing", 1).is_err());
    assert!(TestSelector::new(&f.root, &f.conn, 0).is_err());
    assert_eq!(
        TestSelector::new(&f.root, &f.conn, 99).unwrap().depth,
        MAX_TEST_DEPTH
    );
}

#[test]
fn ambiguous_paths_and_deleted_name_are_explicit() {
    let f = Fixture::new(&[
        ("a.rs", "fn helper() {\n target();\n}\nfn target() {}\n"),
        ("b.rs", "fn helper() {}\n"),
        (
            "c.rs",
            "fn test_bridge() {\n helper();\n}\nfn test_deleted() {\n removed();\n}\n",
        ),
    ]);
    let selector = TestSelector::new(&f.root, &f.conn, 2).unwrap();
    let evidence = selector.select("target").unwrap();
    assert_eq!(evidence.tests.len(), 1);
    assert!(evidence.tests[0].ambiguous);
    assert!(evidence.tests[0].path[1].ambiguous);
    assert!(selector.select("helper").is_err());
    let deleted = selector.select_name("removed").unwrap();
    assert_eq!(deleted.tests.len(), 1);
    assert!(deleted.tests[0].ambiguous);
}

#[test]
fn failed_graph_refresh_does_not_silently_omit_a_file() {
    let f = Fixture::new(&[
        ("code.rs", "fn target() {}\nfn bridge() { target(); }\n"),
        ("tests.rs", "fn test_old() { bridge(); }\n"),
    ]);
    std::fs::write(f.root.join("tests.rs"), [0xff, 0xfe]).unwrap();
    let error = match TestSelector::new(&f.root, &f.conn, 2) {
        Ok(_) => panic!("a failed refresh must not yield a partial graph"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("tests.rs") && error.contains("cannot refresh indexed candidates"),
        "{error}"
    );
}

#[test]
fn wrapper_and_json_preserve_rows_and_disclaim_coverage() {
    let f = Fixture::new(&[(
        "lib.rs",
        "\
fn target() {}
fn test_reference() {
    let callback = target;
}
fn unused() {}
",
    )]);
    let default = crate::commands::cmd_tests(&f.root, &f.conn, "target", false).unwrap();
    let explicit =
        crate::commands::cmd_tests_options(&f.root, &f.conn, "target", 1, false, false).unwrap();
    assert_eq!(default, explicit);
    assert!(default.0.contains("direct reference"));
    assert!(default.0.contains("not proven coverage"));
    let json = crate::commands::cmd_tests_options(&f.root, &f.conn, "target", 1, true, true)
        .unwrap()
        .0;
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["tests"][0]["in"], "test_reference");
    assert_eq!(value["tests"][0]["path"][0]["to"], "lib.rs:target");
    let no_match = crate::commands::cmd_tests(&f.root, &f.conn, "unused", false)
        .unwrap()
        .0;
    assert!(no_match.contains("no match does not mean untested"));
    assert!(!no_match.contains("Untested."));
}

#[test]
fn first_selection_refreshes_all_candidates_before_ambiguity() {
    let f = Fixture::new(&[("a.rs", "fn target() {}\n"), ("b.rs", "fn other() {}\n")]);
    std::fs::write(
        f.root.join("b.rs"),
        "fn target() {}\nfn test_local() { target(); }\n",
    )
    .unwrap();
    let first = crate::commands::cmd_tests(&f.root, &f.conn, "a.rs:target", true)
        .unwrap()
        .0;
    let second = crate::commands::cmd_tests(&f.root, &f.conn, "a.rs:target", true)
        .unwrap()
        .0;
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    let second: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(first, second);
    assert_eq!(first["tests"][0]["in"], "test_local");
    assert_eq!(first["tests"][0]["ambiguous"], true);
}

#[test]
fn writable_graph_refresh_prunes_confirmed_deleted_indexed_files() {
    let f = Fixture::new(&[
        ("code.rs", "fn target() {}\nfn bridge() { target(); }\n"),
        ("gone.rs", "fn test_old() { bridge(); }\n"),
    ]);
    std::fs::remove_file(f.root.join("gone.rs")).unwrap();
    let selector = TestSelector::new(&f.root, &f.conn, 2).unwrap();
    assert!(selector.select("target").unwrap().tests.is_empty());
    assert!(selector
        .graph()
        .unwrap()
        .syms
        .iter()
        .all(|s| s.file != "gone.rs"));
    let count: i64 = f
        .conn
        .query_row(
            "SELECT count(*) FROM files WHERE path = 'gone.rs'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}
