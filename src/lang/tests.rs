use super::{extract_symbols, param_count};

#[test]
fn xml_elements_are_named_by_tag_and_identifying_child() {
    let src = r#"<project>
  <artifactId>demo</artifactId>
  <profiles>
<profile>
  <id>with-frontend-build</id>
  <build>
    <plugins>
      <plugin>
        <artifactId>frontend-maven-plugin</artifactId>
      </plugin>
    </plugins>
  </build>
</profile>
  </profiles>
</project>
"#;
    let syms = extract_symbols("xml", src).unwrap();
    let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    // the identifying child qualifies the repeated tag
    assert!(names.contains(&"profile#with-frontend-build"), "{names:?}");
    assert!(names.contains(&"plugin#frontend-maven-plugin"), "{names:?}");
    // a grandchild's id must not name an ancestor: <profiles> stays bare
    assert!(names.contains(&"profiles"), "{names:?}");
    // an element with no identifying child keeps the bare tag
    assert!(names.contains(&"build"), "{names:?}");
    // nesting is walked (is_container), not just top level
    assert!(names.contains(&"plugins"), "{names:?}");
}

#[test]
fn deeply_nested_source_does_not_overflow_the_stack() {
    // Minified files nest arbitrarily deep; a recursive walk aborted the process.
    let depth = 200_000;
    let src = format!(
        "let x = {}1{};\nfunction real() {{}}\n",
        "(".repeat(depth),
        ")".repeat(depth)
    );
    let syms = extract_symbols("javascript", &src).unwrap();
    assert!(syms.iter().any(|s| s.name == "real"));
    // Every AST walker (refs/grep/rename/edit) must survive the same depth.
    let idents = super::ident_occurrences("javascript", &src).unwrap();
    assert!(idents.iter().any(|(n, _)| n == "real"));
    assert_eq!(
        super::ident_positions(Some("javascript"), &src, "real"),
        (vec![(2, 9)], true)
    );
    assert_eq!(
        super::syntax_errors("javascript", &src).unwrap(),
        Vec::<usize>::new()
    );
}

#[test]
fn nested_js_bindings_keep_preorder_and_qualified_names() {
    // Pre-order: parent before child, child qualified.
    let src = "const outer = () => {\n  const inner = () => {};\n};\nconst after = () => {};\n";
    let syms = extract_symbols("javascript", src).unwrap();
    let got: Vec<(&str, &str)> = syms
        .iter()
        .map(|s| (s.qualified.as_str(), s.kind))
        .collect();
    assert_eq!(
        got,
        vec![("outer", "fn"), ("outer.inner", "fn"), ("after", "fn")]
    );
}

#[test]
fn param_count_basic_and_langs() {
    assert_eq!(param_count("fn low() {}"), Some(0));
    assert_eq!(param_count("fn f(a, b)"), Some(2));
    assert_eq!(param_count("def foo(a, b, c):"), Some(3));
    assert_eq!(param_count("function go(x)"), Some(1));
    // method with one explicit param — the real BudgetOut.finish case
    assert_eq!(
        param_count("fn finish(mut self, trailer: &str) -> String"),
        Some(2)
    );
    // no parens at all → no arity signal, not zero
    assert_eq!(param_count("const X: i32 = 3"), None);
}

#[test]
fn param_count_ignores_nested_commas() {
    // generic type args must not inflate the count
    assert_eq!(
        param_count("fn f(m: HashMap<String, i64>, n: i32)"),
        Some(2)
    );
    // nested tuple / array in a type
    assert_eq!(param_count("fn g(a: (i32, i32), b: [u8; 4])"), Some(2));
    // nested call in a python default
    assert_eq!(param_count("def h(a, b=foo(1, 2)):"), Some(2));
    // string literal containing a comma
    assert_eq!(param_count(r#"fn s(a: &str = "x, y")"#), Some(1));
}

#[test]
fn param_count_edge_cases() {
    // whitespace-only inside parens still reads as zero
    assert_eq!(param_count("fn f(   )"), Some(0));
    // comparison operator in a default doesn't unbalance angle brackets
    assert_eq!(param_count("fn f(a: bool = 1 > 0, b: i32)"), Some(2));
}

/// Regression guard for a silent link-time hijack.
///
/// `vendor/vue/scanner.cc` bundles a COPY of the html scanner. When it exported
/// `tree_sitter_html_external_scanner_*`, the linker silently kept one of the
/// colliding definitions, html parsed with the wrong scanner state, and EVERY
/// document became one ERROR node — zero symbols, no error. Re-exporting those
/// names fails this test.
#[test]
fn html_scanner_exports_do_not_collide() {
    let src = "<html><head><meta charset=\"UTF-8\"></head><body><p>hi</p></body></html>";
    let tree = super::parse("html", src).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "html parsed with errors: {}",
        tree.root_node().to_sexp()
    );
    assert!(!super::extract_symbols("html", src).unwrap().is_empty());
}

// Rank counts uses of top-level items: in Rust `x.ok()` is a method, never the
// free `fn ok`; in Go `pkg.Func()` IS the use, so only Rust skips fields.
#[test]
fn ident_counts_skip_rust_method_names() {
    let names: std::collections::HashSet<&str> = ["ok", "Run"].into();
    let rs = super::ident_counts(Some("rust"), "fn f() { ok(); x.ok(); y.ok }", &names);
    assert_eq!(rs.get("ok"), Some(&1));
    let go = super::ident_counts(Some("go"), "package p\nfunc f() { pkg.Run() }", &names);
    assert_eq!(go.get("Run"), Some(&1));
}
