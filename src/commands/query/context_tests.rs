//! Editing context fixtures use an in-memory index, never the user's data dir.
use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    conn: Connection,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "cona-context-edit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT UNIQUE,mtime INTEGER,size INTEGER,lang TEXT);
             CREATE TABLE symbols(id INTEGER PRIMARY KEY,file_id INTEGER,name TEXT,qualified TEXT,
                kind TEXT,parent TEXT,start_line INTEGER,end_line INTEGER,signature TEXT);
             CREATE TABLE notes(id INTEGER PRIMARY KEY,symbol TEXT,note TEXT,ts INTEGER);",
        ).unwrap();
        let f = Self { root, conn };
        for (path, source) in files {
            f.write(path, source);
            crate::indexer::reindex_file(&f.root, &f.conn, path).unwrap();
        }
        f
    }

    fn write(&self, path: &str, source: &str) {
        let full = self.root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, source).unwrap();
    }

    fn edit(&self, budget: i64, json: bool) -> Result<String> {
        cmd_context_edit(&self.root, &self.conn, "target", budget, false, json).map(|r| r.0)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const SMALL: &str = "struct Config { value: i32 }\nconst LIMIT: i32 = 3;\nfn helper(x: i32) -> i32 { x + LIMIT }\nfn target(c: Config) -> i32 {\n    helper(c.value) + helper(LIMIT)\n}\nfn caller() { target(Config { value: 1 }); }\n#[test]\nfn test_target() { caller(); }\n";

#[test]
fn editing_context_has_atomic_body_types_helpers_callers_and_test_evidence() {
    let f = Fixture::new(&[("lib.rs", SMALL)]);
    let out = f.edit(5000, true).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["target"]["code"],
        "fn target(c: Config) -> i32 {\n    helper(c.value) + helper(LIMIT)\n}"
    );
    assert_eq!(v["body_omitted"], false);
    let rows = v["related"].as_array().unwrap();
    for name in ["Config", "LIMIT", "helper", "caller"] {
        assert_eq!(
            rows.iter().filter(|r| r["symbol"] == name).count(),
            1,
            "{out}"
        );
    }
    assert_eq!(v["tests"].as_array().unwrap().len(), 1, "{out}");
    assert_eq!(v["tests"][0]["depth"], 2);
    let text = f.edit(5000, false).unwrap();
    for name in ["Config", "LIMIT", "helper", "caller", "test_target"] {
        assert!(text.contains(name), "{text}");
    }
    assert!(text.contains("not execution coverage"));
}

#[test]
fn editing_budget_includes_serialized_envelopes_and_never_slices_target() {
    let large = format!(
        "fn target() {{\n{}\n}}\n",
        "    let _ = \"é\\\\quoted\";\n".repeat(100)
    );
    let f = Fixture::new(&[("lib.rs", &large)]);
    for json in [false, true] {
        for budget in [150, 200, 300, 500] {
            let out = f.edit(budget, json).unwrap();
            assert!(db::est_tokens(out.len()) <= budget, "{budget}: {out}");
            if json {
                let v: serde_json::Value = serde_json::from_str(&out).unwrap();
                assert_eq!(v["body_omitted"], true);
                assert!(v["target"].get("code").is_none());
                assert_eq!(v["show"], "lib.rs:target");
            } else {
                assert!(out.contains("Target body omitted; cona show lib.rs:target"));
                assert!(!out.contains("let _"));
            }
        }
        let err = f.edit(1, json).unwrap_err().to_string();
        assert!(
            err.contains("target body omitted") && err.contains("cona show lib.rs:target"),
            "{err}"
        );
        let full = f.edit(5000, json).unwrap();
        assert!(full.contains("let _"));
    }
}

#[test]
fn editing_dependencies_refresh_and_missing_files_fail_open() {
    let f = Fixture::new(&[
        ("lib.rs", "fn target() { helper(); missing(); }\n"),
        ("deps.rs", "fn helper() {}\nfn missing() {}\n"),
    ]);
    f.write(
        "deps.rs",
        "// moved\n// down\nfn helper() { let fresh = 1; }\n",
    );
    let out = f.edit(5000, true).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let helper = v["related"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["symbol"] == "helper")
        .unwrap();
    assert_eq!(helper["start"], 3);
    assert!(helper["code"].as_str().unwrap().contains("fresh"));
    std::fs::remove_file(f.root.join("deps.rs")).unwrap();
    let out = f.edit(5000, false).unwrap();
    assert!(out.contains("fn target()"));
}

#[test]
fn editing_same_named_dependencies_remain_explicitly_uncertain() {
    let f = Fixture::new(&[
        ("main/lib.rs", "fn target() { helper(); }\n"),
        ("a/lib.rs", "fn helper() {}\n"),
        ("b/lib.rs", "fn helper() {}\n"),
    ]);
    let out = f.edit(5000, true).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let rows = v["related"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{out}");
    assert!(rows.iter().all(|r| r["ambiguous"] == true));
    assert!(f.edit(5000, false).unwrap().contains("·ambiguous"));
}

#[test]
fn editing_refreshes_new_candidates_before_selecting_dependencies() {
    for no_tests in [false, true] {
        let f = Fixture::new(&[
            ("main/a.rs", "fn target() { helper(); }\n"),
            ("one/b.rs", "fn helper() {}\n"),
            ("two/c.rs", "fn other() {}\n"),
        ]);
        f.write("two/c.rs", "fn helper() { let _ = 42; }\n");
        let run = || {
            cmd_context_edit(&f.root, &f.conn, "target", 5000, no_tests, true)
                .unwrap()
                .0
        };
        let first = run();
        assert_eq!(first, run(), "first call used stale candidate names");
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        let helpers: Vec<_> = value["related"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["symbol"] == "helper")
            .collect();
        assert_eq!(helpers.len(), 2, "{first}");
        assert!(helpers.iter().all(|r| r["ambiguous"] == true), "{first}");
        assert!(first.contains("42"), "{first}");
    }
}

#[test]
fn editing_read_only_refuses_stale_candidate_files() {
    // Read-only mode is process-global. Isolate it from parallel fixture tests.
    const CHILD: &str = "CONA_CONTEXT_READ_ONLY_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::query::context::edit_tests::editing_read_only_refuses_stale_candidate_files",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    let f = Fixture::new(&[
        ("main/a.rs", "fn target() { helper(); }\n"),
        ("one/b.rs", "fn helper() {}\n"),
        ("two/c.rs", "fn other() {}\n"),
    ]);
    f.write("two/c.rs", "fn helper() { let _ = 42; }\n");
    db::set_read_only(true);
    for no_tests in [false, true] {
        let error = cmd_context_edit(&f.root, &f.conn, "target", 5000, no_tests, true)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("cannot refresh") && error.contains("two/c.rs"),
            "{error}"
        );
    }
}

#[test]
fn editing_no_tests_filters_evidence_not_target_and_budget_remains_bounded() {
    let f = Fixture::new(&[("lib.rs", SMALL)]);
    let (out, _) = cmd_context_edit(&f.root, &f.conn, "target", 5000, true, true).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["tests"], serde_json::json!([]));
    assert!(v["target"]["code"].is_string());
}

#[test]
fn editing_budget_pays_for_notes_and_related_code_as_atomic_blocks() {
    let f = Fixture::new(&[("lib.rs", SMALL)]);
    f.conn
        .execute(
            "INSERT INTO notes(symbol,note,ts) VALUES('target',?1,0)",
            ["a very long note ".repeat(1000)],
        )
        .unwrap();
    for json in [false, true] {
        for budget in [150, 200, 300, 500, 1000] {
            let out = f.edit(budget, json).unwrap();
            assert!(db::est_tokens(out.len()) <= budget);
            assert!(!out.contains("a very long note"));
            if json {
                let value: serde_json::Value = serde_json::from_str(&out).unwrap();
                for item in value["related"].as_array().unwrap() {
                    if item["symbol"] == "helper" {
                        assert_eq!(item["code"], "fn helper(x: i32) -> i32 { x + LIMIT }");
                    }
                }
            } else if out.contains("fn helper(") {
                assert!(out.contains("fn helper(x: i32) -> i32 { x + LIMIT }"));
            }
        }
    }
}

#[test]
fn editing_nested_definitions_are_not_repeated_outside_target() {
    let f = Fixture::new(&[(
        "lib.rs",
        "fn target() {\n    fn nested() {}\n    nested();\n}\n",
    )]);
    let out = f.edit(5000, true).unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(value["related"].as_array().unwrap().is_empty(), "{out}");
    assert!(value["target"]["code"]
        .as_str()
        .unwrap()
        .contains("fn nested()"));
}

#[test]
fn original_context_keeps_full_target_even_below_body_budget() {
    let f = Fixture::new(&[("lib.rs", "fn target() {\n    let untouched = 1;\n}\n")]);
    let (text, _) = cmd_context(&f.root, &f.conn, "target", 1, false, false).unwrap();
    assert!(text.contains("let untouched = 1;"));
    let (json, _) = cmd_context(&f.root, &f.conn, "target", 1, false, true).unwrap();
    assert_eq!(
        json,
        "{\"symbol\":{\"file\":\"lib.rs\",\"symbol\":\"target\",\"start\":1,\"end\":3,\"code\":\"fn target() {\\n    let untouched = 1;\\n}\"},\"notes\":[],\"calls\":[],\"calls_capped\":false,\"called_by\":[],\"called_by_capped\":false,\"test_callers_hidden\":0}\n"
    );
}
