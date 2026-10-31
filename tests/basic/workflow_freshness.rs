//! Workflow evidence must not combine current source with stale graph nodes.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[test]
fn read_only_workflows_refuse_stale_indirect_test_evidence() {
    let dir = std::env::temp_dir().join(format!("cona-ro-workflow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join(".cona-data");
    let cona = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cona"))
            .args(args)
            .env("CONA_DATA_DIR", &data)
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    std::fs::write(
        dir.join("code.rs"),
        "fn target() {}\nfn bridge() { target(); }\n",
    )
    .unwrap();
    std::fs::write(dir.join("tests.rs"), "fn test_old() { bridge(); }\n").unwrap();
    let indexed = cona(&["index"]);
    assert!(indexed.status.success(), "{indexed:?}");

    // Before the external edit, both opt-in workflows use the indexed test.
    for args in [
        vec!["--read-only", "--json", "tests", "target", "--depth", "2"],
        vec!["--read-only", "--json", "context", "target", "--edit"],
    ] {
        let output = cona(&args);
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("test_old"),
            "{output:?}"
        );
    }
    std::fs::write(dir.join("tests.rs"), "fn test_new_longer() { bridge(); }\n").unwrap();
    let before = snapshot(&dir);
    for args in [
        vec!["--read-only", "--json", "tests", "target", "--depth", "2"],
        vec!["--read-only", "--json", "context", "target", "--edit"],
    ] {
        let output = cona(&args);
        assert!(!output.status.success(), "{output:?}");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("tests.rs") && text.contains("cannot refresh indexed candidates"),
            "{text}"
        );
        assert!(
            !text.contains("test_old"),
            "nonexistent indexed test returned: {text}"
        );
        assert_eq!(
            snapshot(&dir),
            before,
            "read-only query changed source, index, or usage files"
        );
    }

    // Writable queries still refresh stale nodes rather than rejecting them.
    let output = cona(&["--json", "tests", "target", "--depth", "2"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("test_new_longer") && !text.contains("test_old"),
        "{text}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn overloaded_test_selectors_show_only_the_intended_definition_and_reject_stale_evidence() {
    let dir = std::env::temp_dir().join(format!("cona-overload-workflow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cona = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cona"))
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    let source = "void target() {}\n\
void test_overload() {\n target(); // zero\n target();\n}\n\
void test_overload(int a) {\n target(); // one\n}\n\
void test_overload(int a, int b) {\n target(); // two\n}\n\
void test_overload(int a, int b, int c) {\n target(); // three\n}\n";
    std::fs::write(dir.join("code.cpp"), source).unwrap();
    assert!(cona(&["index"]).status.success());
    let output = cona(&["--read-only", "--json", "tests", "target"]);
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = value["tests"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "{value}");
    let mut selectors = std::collections::HashSet::new();
    for (row, marker) in rows.iter().zip(["zero", "one", "two", "three"]) {
        let selector = row["selector"].as_str().unwrap();
        assert!(selectors.insert(selector), "{value}");
        assert!(
            row.get("identity").is_none(),
            "private metadata leaked: {value}"
        );
        let shown = cona(&["--read-only", "show", selector]);
        assert!(shown.status.success(), "{shown:?}");
        let text = String::from_utf8_lossy(&shown.stdout);
        assert_eq!(text.matches("void test_overload(").count(), 1, "{text}");
        for other in ["zero", "one", "two", "three"] {
            assert_eq!(
                text.contains(&format!("// {other}")),
                marker == other,
                "{text}"
            );
        }
    }
    // Moving the definitions invalidates the snapshot identity. Read-only test
    // selection must refuse it, not combine current references and old ranges.
    std::fs::write(dir.join("code.cpp"), format!("// moved\n\n{source}")).unwrap();
    let before = snapshot(&dir);
    let stale = cona(&["--read-only", "--json", "tests", "target"]);
    assert!(!stale.status.success(), "{stale:?}");
    let error = String::from_utf8_lossy(&stale.stderr);
    assert!(
        error.contains("code.cpp") && error.contains("cannot refresh indexed candidates"),
        "{error}"
    );
    assert_eq!(snapshot(&dir), before);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn impact_handles_a_whole_deleted_file_still_in_the_index() {
    let dir = std::env::temp_dir().join(format!("cona-deleted-workflow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .env("GIT_EDITOR", "true")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("gone.rs"), "fn vanished() {}\n").unwrap();
    std::fs::write(dir.join("code.rs"), "fn test_remaining() { vanished(); }\n").unwrap();
    git(&["add", "gone.rs", "code.rs"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "-qm",
        "fixture",
    ]);
    let cona = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cona"))
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    assert!(cona(&["index"]).status.success());
    std::fs::remove_file(dir.join("gone.rs")).unwrap();
    let output = cona(&["--json", "diff", "HEAD", "--impact", "--budget", "8000"]);
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["symbol"] == "vanished" && row["status"] == "deleted"),
        "{value}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
