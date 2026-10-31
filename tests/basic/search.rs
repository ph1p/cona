//! Offline workflow evaluation: real indexed fixtures, never model judgments.
//! The indirect chains, ambiguity and signature variants are deliberately inert
//! groundwork for later workflow stages, not assertions about execution coverage.

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const ENGINE: &str = r#"pub fn reconcile(records: &[u64]) -> Vec<u64> {
    let mut unique = records.to_vec();
    unique.sort_unstable();
    unique.dedup();
    unique
}

/// Rotate authentication credentials after expiry.
pub fn renew(secret: &str) -> String {
    secret.to_owned()
}

pub fn decode_packet(frame: &[u8], strict: bool) -> usize {
    if strict { frame.len() } else { 0 }
}

pub fn café() -> &'static str {
    "crème brûlée"
}

pub fn orchestrate(records: &[u64]) -> Vec<u64> {
    reconcile(records)
}

pub fn duplicate() {}
"#;

struct Fixture {
    root: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "cona-search-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let fixture = Self {
            data: root.with_extension("data"),
            root,
        };
        fixture.write("src/engine.rs", ENGINE);
        fixture.write(
            "src/distractors.rs",
            r#"pub fn sort_labels(labels: &mut [String]) { labels.sort(); }
pub fn dedup_adjacent(records: &mut Vec<u64>) { records.dedup(); }
/// Credentials for a static authentication example; never rotates them.
pub fn credential_example() {}
pub fn duplicate() {}
pub fn decode_packet(frame: &[u8]) -> usize { frame.len() }
"#,
        );
        fixture.write(
            "src/nested.rs",
            "pub fn outer() {\n    fn inner() { let _ = \"nestedneedle\"; }\n    inner();\n}\n",
        );
        fixture.write(
            "src_old/outside.rs",
            "pub fn outside() { let _ = \"scopeprobe\"; }\n",
        );
        fixture.write(
            "src/scoped.rs",
            "pub fn inside() { let _ = \"scopeprobe\"; }\n",
        );
        fixture.write(
            "tests/flow.rs",
            "#[test]\nfn test_flow() { orchestrate(&[2, 1, 2]); }\n",
        );
        for n in 0..14 {
            fixture.write(
                &format!("ties/f{n:02}.rs"),
                "pub fn candidate() { let _ = \"tieprobe\"; }\n",
            );
        }
        fixture.ok(&["index"]);
        fixture
    }

    fn write(&self, path: &str, source: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cona"))
            .args(args)
            .env("CONA_DATA_DIR", &self.data)
            .env("NO_COLOR", "1")
            .env("CONA_NO_FETCH_HELPER", "1")
            .current_dir(&self.root)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn search(&self, query: &str, options: &[&str]) -> Value {
        let mut args = vec!["--json", "search", query];
        args.extend_from_slice(options);
        let text = self.ok(&args);
        let value: Value = serde_json::from_str(&text).expect("search JSON");
        assert_eq!(value["query"], query);
        assert_eq!(value["lexical"], true);
        assert!(value["truncated"].is_boolean(), "{value}");
        assert!(value["results"].is_array(), "{value}");
        value
    }

    fn assert_baseline(&self, query: &str, limit: usize) {
        let root = self.root.canonicalize().unwrap();
        // Use the index the CLI created, not a hash of a differently spelled
        // root: Windows canonicalization can add an extended-length prefix.
        let databases: Vec<_> = std::fs::read_dir(self.data.join("projects"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "db"))
            .collect();
        assert_eq!(databases.len(), 1, "fixture project indexes: {databases:?}");
        let conn = rusqlite::Connection::open_with_flags(
            &databases[0],
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let (text, baseline) =
            cona::commands::cmd_search(&conn, &root, query, limit, None, true).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let mut sites = std::collections::BTreeMap::<&str, Vec<usize>>::new();
        for row in rows(&value) {
            sites
                .entry(row["file"].as_str().unwrap())
                .or_default()
                .push(row["excerpt"]["line"].as_u64().unwrap() as usize);
        }
        // Reference the old baseline rule: source lengths grouped by final
        // displayed file/site, not one independent window per candidate.
        let expected: i64 = sites
            .iter()
            .map(|(file, lines)| {
                let source = std::fs::read_to_string(root.join(file)).unwrap();
                let lengths: Vec<_> = source.lines().map(str::len).collect();
                cona::db::baseline_tokens(&lengths, lines)
            })
            .sum();
        assert_eq!(baseline, expected, "query={query:?}, limit={limit}");
        if rows(&value).is_empty() {
            assert_eq!(baseline, 0);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

fn rows(value: &Value) -> &[Value] {
    value["results"].as_array().unwrap()
}

fn first_symbol(value: &Value) -> &str {
    rows(value).first().expect("expected a hit")["symbol"]
        .as_str()
        .unwrap()
}

#[test]
fn fixture_baseline_name_lookup_does_not_discover_undocumented_behavior() {
    let fixture = Fixture::new();
    // Characterize the old name-only discovery limitation without a failing
    // acceptance test: the implementation's name says nothing about its body.
    let found: Value = serde_json::from_str(&fixture.ok(&["--json", "find", "reconcile"])).unwrap();
    assert!(found.to_string().contains("reconcile"));
    let missed = fixture.ok(&["find", "sort_unstable dedup"]);
    assert!(!missed.contains("src/engine.rs"), "{missed}");
}

#[test]
fn behavior_ranking_uses_bodies_docs_identifiers_and_signatures() {
    let fixture = Fixture::new();
    let behavior = fixture.search("sort unstable dedup", &[]);
    assert_eq!(first_symbol(&behavior), "reconcile", "{behavior}");
    let docs = fixture.search("rotate authentication credentials expiry", &[]);
    assert_eq!(first_symbol(&docs), "renew", "{docs}");
    let exact = fixture.search("decode_packet", &[]);
    assert_eq!(first_symbol(&exact), "decode_packet", "{exact}");
    let signature = fixture.search("decode packet strict bool", &[]);
    assert_eq!(rows(&signature)[0]["file"], "src/engine.rs", "{signature}");
    let split = fixture.search("decode packet", &[]);
    assert_eq!(first_symbol(&split), "decode_packet", "{split}");

    for result in rows(&behavior) {
        assert!(result["score"].as_f64().unwrap() > 0.0, "{result}");
        assert!(
            !result["reasons"].as_array().unwrap().is_empty(),
            "{result}"
        );
        assert!(result["start"].as_u64().unwrap() > 0, "{result}");
        assert!(result["end"].as_u64() >= result["start"].as_u64());
        let file = result["file"].as_str().unwrap();
        let source = std::fs::read_to_string(fixture.root.join(file)).unwrap();
        let line = result["excerpt"]["line"].as_u64().unwrap() as usize;
        let excerpt = result["excerpt"]["text"].as_str().unwrap();
        assert!(!excerpt.is_empty(), "{result}");
        assert!(line > 0 && line <= source.lines().count(), "{result}");
        // Evidence is compact rather than a full source-file dump.
        assert!(excerpt.chars().count() <= 500, "{result}");
    }
}

#[test]
fn search_cli_forms_and_returned_selectors_are_reusable() {
    let fixture = Fixture::new();
    let query = "sort unstable dedup";
    assert_eq!(
        fixture.ok(&["search", query]),
        fixture.ok(&["nav", "search", query])
    );
    let result = fixture.search(query, &["--limit", "1"]);
    let selector = rows(&result)[0]["selector"].as_str().unwrap();
    assert!(selector.starts_with("src/engine.rs:"), "{selector}");
    let shown = fixture.ok(&["show", selector]);
    assert!(shown.contains("fn reconcile"), "{shown}");
    assert!(shown.contains("unique.dedup()"), "{shown}");

    // Same names in different files must not force callers to guess which one.
    let duplicates = fixture.search("duplicate", &[]);
    let matches: Vec<_> = rows(&duplicates)
        .iter()
        .filter(|row| row["symbol"] == "duplicate")
        .collect();
    assert_eq!(matches.len(), 2, "{duplicates}");
    assert_ne!(matches[0]["selector"], matches[1]["selector"]);
    for row in matches {
        let shown = fixture.ok(&["show", row["selector"].as_str().unwrap()]);
        assert!(shown.contains("fn duplicate"), "{shown}");
    }
}

#[test]
fn search_limits_scope_and_stable_ties() {
    let fixture = Fixture::new();
    let default = fixture.search("tieprobe", &[]);
    assert_eq!(rows(&default).len(), 10, "{default}");
    assert_eq!(default["truncated"], true);
    let all = fixture.search("tieprobe", &["--limit", "30"]);
    assert_eq!(rows(&all).len(), 14, "{all}");
    assert_eq!(all["truncated"], false);
    for _ in 0..3 {
        assert_eq!(fixture.search("tieprobe", &["--limit", "30"]), all);
    }
    let limited = fixture.search("tieprobe", &["--limit", "2"]);
    assert_eq!(rows(&limited), &rows(&all)[..2]);
    assert_eq!(limited["truncated"], true);
    let zero = fixture.search("tieprobe", &["--limit", "0"]);
    assert!(rows(&zero).is_empty());
    assert_eq!(zero["truncated"], true);
    for limit in [1, 10, 13, 14, 15, 100, 101] {
        let result = fixture.search("tieprobe", &["--limit", &limit.to_string()]);
        assert_eq!(rows(&result), &rows(&all)[..limit.min(14)]);
        assert_eq!(result["truncated"], limit < 14);
    }
    let text = fixture.ok(&["search", "tieprobe", "--limit", "14"]);
    let positions: Vec<_> = rows(&all)
        .iter()
        .map(|row| text.find(row["selector"].as_str().unwrap()).unwrap())
        .collect();
    assert!(positions.windows(2).all(|p| p[0] < p[1]), "{text}");

    let scoped = fixture.search("scopeprobe", &["--path", "src"]);
    assert_eq!(rows(&scoped).len(), 1, "{scoped}");
    assert_eq!(rows(&scoped)[0]["file"], "src/scoped.rs");
    let prefix = fixture.search("scopeprobe", &["--path", "src_"]);
    assert_eq!(rows(&prefix).len(), 1, "{prefix}");
    assert_eq!(rows(&prefix)[0]["file"], "src_old/outside.rs");
    let file = fixture.search("duplicate", &["--path", "src/engine.rs"]);
    assert_eq!(rows(&file).len(), 1, "{file}");
    assert!(rows(&fixture.search("scopeprobe", &["--path", "missing"])).is_empty());
}

#[test]
fn search_empty_unicode_and_nested_hits() {
    let fixture = Fixture::new();
    for query in ["", "   \t\n", "the and of", "unfindablequux987"] {
        let result = fixture.search(query, &[]);
        assert!(rows(&result).is_empty(), "{query:?}: {result}");
        assert_eq!(result["truncated"], false);
        let zero = fixture.search(query, &["--limit", "0"]);
        assert!(rows(&zero).is_empty(), "{query:?}: {zero}");
        assert_eq!(zero["truncated"], false);
    }
    assert_eq!(first_symbol(&fixture.search("café", &[])), "café");
    assert_eq!(first_symbol(&fixture.search("crème brûlée", &[])), "café");
    let nested = fixture.search("nestedneedle", &[]);
    assert!(!rows(&nested).is_empty(), "{nested}");
    let mut identities = std::collections::HashSet::new();
    for row in rows(&nested) {
        assert!(
            identities.insert((
                row["file"].to_string(),
                row["start"].to_string(),
                row["kind"].to_string()
            )),
            "duplicate symbol row: {nested}"
        );
    }
    assert!(
        rows(&nested).iter().any(|row| {
            let symbol = row["symbol"].as_str().unwrap();
            symbol == "inner" || symbol.ends_with(".inner")
        }),
        "{nested}"
    );
    fixture.write("stopword.rs", "fn the() {}\n");
    fixture.ok(&["index"]);
    let stopword = fixture.search("the", &["--limit", "1"]);
    assert_eq!(first_symbol(&stopword), "the");
    assert_eq!(stopword["truncated"], false);
    let zero = fixture.search("the", &["--limit", "0"]);
    assert!(rows(&zero).is_empty());
    assert_eq!(zero["truncated"], true);
}

#[test]
fn search_refreshes_changed_bodies_signatures_and_ranges() {
    let fixture = Fixture::new();
    assert_eq!(
        first_symbol(&fixture.search("sort unstable dedup", &[])),
        "reconcile"
    );
    // No explicit reindex: search must score live bodies and return live ranges,
    // not merely update the evidence attached to an obsolete ranking.
    fixture.write(
        "src/engine.rs",
        "// shifted\n// shifted again\npub fn replacement(payload: &str, retries: u32) {\n    let _ = (payload, retries, \"freshneedle\");\n}\n",
    );
    let stale_zero = fixture.run(&["--read-only", "search", "freshneedle", "--limit", "0"]);
    assert!(!stale_zero.status.success());
    assert!(
        String::from_utf8_lossy(&stale_zero.stderr).contains("changed since it was indexed"),
        "{stale_zero:?}"
    );
    let zero = fixture.search("freshneedle", &["--limit", "0"]);
    assert!(rows(&zero).is_empty());
    assert_eq!(zero["truncated"], true);
    let fresh = fixture.search("freshneedle", &[]);
    assert_eq!(first_symbol(&fresh), "replacement", "{fresh}");
    assert_eq!(rows(&fresh)[0]["start"], 3, "{fresh}");
    assert!(
        rows(&fresh)[0]["sig"].as_str().unwrap().contains("retries"),
        "{fresh}"
    );
    let stale = fixture.search("reconcile", &[]);
    assert!(
        !rows(&stale).iter().any(|row| row["symbol"] == "reconcile"),
        "{stale}"
    );
    let shown = fixture.ok(&["show", rows(&fresh)[0]["selector"].as_str().unwrap()]);
    assert!(shown.contains("freshneedle"), "{shown}");
}

#[test]
fn overloaded_selectors_show_each_intended_body() {
    let fixture = Fixture::new();
    fixture.write(
        "overloads.cpp",
        "int convert(int value) { return value + 1; }\n\
         double convert(double value) { return value + 2.5; }\n",
    );
    fixture.ok(&["index"]);
    let result = fixture.search("convert", &["--path", "overloads.cpp"]);
    assert_eq!(rows(&result).len(), 2, "{result}");
    let one = fixture.search("convert", &["--path", "overloads.cpp", "--limit", "1"]);
    assert_eq!(rows(&one), &rows(&result)[..1]);
    assert_eq!(one["truncated"], true);
    // A nonmatching overload still makes the surviving name ambiguous.
    let filtered = fixture.search("double", &["--path", "overloads.cpp", "--limit", "1"]);
    assert_eq!(rows(&filtered).len(), 1);
    assert_eq!(rows(&filtered)[0]["selector"], "overloads.cpp:2-2");
    let mut selectors = std::collections::HashSet::new();
    for row in rows(&result) {
        assert_eq!(row["symbol"], "convert");
        let selector = row["selector"].as_str().unwrap();
        assert!(selectors.insert(selector), "{result}");
        let start = row["start"].as_u64().unwrap();
        let end = row["end"].as_u64().unwrap();
        assert_eq!(selector, format!("overloads.cpp:{start}-{end}"));
        let shown = fixture.ok(&["show", selector]);
        let (own_body, other_body) = if start == 1 {
            ("value + 1", "value + 2.5")
        } else {
            ("value + 2.5", "value + 1")
        };
        assert!(shown.contains(own_body), "{selector}: {shown}");
        assert!(!shown.contains(other_body), "{selector}: {shown}");
    }
    let text = fixture.ok(&["search", "convert", "--path", "overloads.cpp"]);
    for selector in selectors {
        assert!(text.contains(selector), "{text}");
    }
}

#[test]
fn search_baseline_opens_the_existing_fixture_index() {
    let fixture = Fixture::new();
    let projects = fixture.data.join("projects");
    let database = std::fs::read_dir(&projects)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "db"))
        .unwrap();
    // Model an index whose filename differs from a canonicalized-root hash,
    // as happens when Windows adds its extended-length path prefix.
    std::fs::rename(database, projects.join("fixture-index.db")).unwrap();
    fixture.assert_baseline("reconcile", 10);
    assert_eq!(
        std::fs::read_dir(projects)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "db"))
            .count(),
        1,
        "baseline verification must not create a second database"
    );
}

#[test]
fn search_display_guard_follows_capped_prefix_without_backfill() {
    let fixture = Fixture::new();
    for n in (0..120).rev() {
        fixture.write(
            &format!("many/f{n:03}.rs"),
            "fn candidate() { let _ = \"manyneedle\"; }\n",
        );
    }
    fixture.ok(&["index"]);
    let ten = fixture.search("manyneedle", &["--limit", "10"]);
    assert_eq!(rows(&ten).len(), 10);
    for (n, row) in rows(&ten).iter().enumerate() {
        assert_eq!(row["file"], format!("many/f{n:03}.rs"));
    }
    let hundred = fixture.search("manyneedle", &["--limit", "100"]);
    assert_eq!(hundred["truncated"], true);
    let mut bytes = 0;
    let keep = (0..100)
        .take_while(|n| {
            bytes += (format!("many/f{n:03}.rs").len()
                + "candidate".len()
                + rows(&ten)[0]["kind"].as_str().unwrap().len())
                * 12
                + 16_000;
            bytes <= 512_000
        })
        .count();
    assert_eq!(rows(&hundred).len(), keep);
    assert_eq!(rows(&ten), &rows(&hundred)[..10]);
    assert_eq!(fixture.search("manyneedle", &["--limit", "1000"]), hundred);
    for (n, row) in rows(&hundred).iter().enumerate() {
        assert_eq!(row["file"], format!("many/f{n:03}.rs"));
    }
    for query in [
        "manyneedle",
        "duplicate",
        "convert",
        "",
        "the and of",
        "absentneedle",
    ] {
        for limit in [0, 1, 10, 100, 1000] {
            fixture.assert_baseline(query, limit);
        }
    }
}
