//! Pure classification heuristics behind `cona entries` and
//! `cona tests` — where execution starts, what is exported, what is a test.

/// True when a path is test code by convention (any supported ecosystem).
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let fname = lower.rsplit('/').next().unwrap_or(&lower);
    let in_test_dir = lower
        .split('/')
        .any(|seg| matches!(seg, "tests" | "test" | "__tests__" | "spec" | "specs"));
    in_test_dir
        || fname.starts_with("test_")
        || fname.contains(".test.")
        || fname.contains(".spec.")
        || fname
            .rsplit_once('.')
            // `tests.rs` / `x_tests.rs`: Rust's out-of-line `#[cfg(test)] mod tests;`
            .map(|(stem, _)| {
                stem == "tests"
                    || stem.ends_with("_test")
                    || stem.ends_with("_tests")
                    || stem.ends_with("_spec")
            })
            .unwrap_or(false)
}

/// True when a symbol's (qualified) name marks it a test, e.g. `tests::…`/`test_*`.
/// Machine-written files: codegen output, minified bundles, lockfiles. Their
/// hits are real but rarely what a search is after — `grep` folds them into
/// one trailer line when hand-written files match too.
pub fn is_generated_path(path: &str) -> bool {
    let fname = path.rsplit('/').next().unwrap_or(path);
    let lower = fname.to_ascii_lowercase();
    [
        ".gen.",
        ".generated.",
        "_generated.",
        ".g.dart",
        ".freezed.dart",
        ".pb.go",
        "_pb2.py",
        "_pb2_grpc.py",
        ".min.js",
        ".min.css",
    ]
    .iter()
    .any(|m| lower.contains(m))
        || matches!(
            lower.as_str(),
            "package-lock.json"
                | "pnpm-lock.yaml"
                | "yarn.lock"
                | "bun.lock"
                | "cargo.lock"
                | "composer.lock"
                | "gemfile.lock"
                | "poetry.lock"
                | "uv.lock"
                | "go.sum"
        )
}

pub fn is_test_symbol(qualified: &str) -> bool {
    qualified.split('.').any(|seg| {
        seg == "tests"
            || seg == "test"
            || seg.starts_with("test_")
            || seg.ends_with("_test")
            || seg.ends_with("_tests")
    })
}

/// What kind of entry point a symbol is, if any.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum EntryClass {
    Main,
    Api,
    Test,
}

/// `parent` = enclosing symbol (None = top level). Test symbols count only
/// when callable (a struct in a test file is neither a test nor API).
/// A file the framework loads by NAME, so nothing in the repo imports it:
/// Next.js App/Pages Router files, middleware, service workers. Every export
/// of such a file is an entry point, not dead code.
pub fn is_framework_entry_path(path: &str) -> bool {
    let Some((stem, ext)) = path.rsplit('/').next().and_then(|f| f.split_once('.')) else {
        return false;
    };
    if !matches!(ext, "ts" | "tsx" | "js" | "jsx" | "mjs" | "mts") {
        return false;
    }
    let under =
        |dir: &str| path.starts_with(&format!("{dir}/")) || path.contains(&format!("/{dir}/"));
    if matches!(
        stem,
        "middleware" | "proxy" | "instrumentation" | "sw" | "service-worker"
    ) {
        return true;
    }
    if under("pages") {
        return true;
    }
    under("app")
        && matches!(
            stem,
            "page"
                | "layout"
                | "route"
                | "loading"
                | "error"
                | "not-found"
                | "template"
                | "default"
                | "global-error"
                | "sitemap"
                | "robots"
                | "manifest"
                | "opengraph-image"
                | "twitter-image"
                | "icon"
                | "apple-icon"
        )
}

/// `import("./apps/Mail")` / `import('./x/index')` → the module name a lazy
/// loader refers to (`Mail`, `x`). Name-based like the rest of `dead`.
pub fn dynamic_import_stems(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find("import(") {
        rest = &rest[i + 7..];
        let t = rest.trim_start();
        let Some(q) = t.chars().next().filter(|c| matches!(c, '"' | '\'' | '`')) else {
            continue;
        };
        let Some(end) = t[1..].find(q) else { continue };
        if let Some(stem) = module_stem(&t[1..1 + end]) {
            out.push(stem.to_string());
        }
    }
    out
}

/// Last path segment without extension; a trailing `index` names its directory.
pub fn module_stem(path: &str) -> Option<&str> {
    let mut segs = path.rsplit('/').filter(|s| !s.is_empty());
    let last = segs.next()?;
    let stem = last
        .split('.')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(last);
    if stem == "index" {
        segs.next().filter(|s| *s != "." && *s != "..")
    } else {
        Some(stem)
    }
}

pub fn entry_class(
    lang: &str,
    name: &str,
    kind: &str,
    parent: Option<&str>,
    signature: &str,
    path: &str,
) -> Option<EntryClass> {
    let callable = crate::lang::is_callable_kind(kind);
    if is_test_path(path) || parent.map(is_test_symbol).unwrap_or(false) || is_test_symbol(name) {
        return callable.then_some(EntryClass::Test);
    }
    if callable && name == "main" && parent.is_none() {
        return Some(EntryClass::Main);
    }
    // `python -m pkg` runs pkg/__main__.py — its top-level defs are entries
    if lang == "python"
        && parent.is_none()
        && callable
        && path.rsplit('/').next() == Some("__main__.py")
    {
        return Some(EntryClass::Main);
    }
    match lang {
        "rust"
            // exported surface: top-level `pub` items (methods inside a pub impl
            // are reachable too, but top-level keeps the list orienting-sized)
            if parent.is_none() && signature.trim_start().starts_with("pub ") => {
                return Some(EntryClass::Api);
            }
        "javascript" | "typescript" | "tsx"
            if signature.trim_start().starts_with("export ") => {
                return Some(EntryClass::Api);
            }
        "python"
            // no export keyword: top-level defs/classes not starting with `_`
            if parent.is_none() && matches!(kind, "def" | "class") && !name.starts_with('_') => {
                return Some(EntryClass::Api);
            }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths_by_convention() {
        assert!(is_test_path("tests/basic.rs"));
        assert!(is_test_path("src/__tests__/x.ts"));
        assert!(is_test_path("app/lib/api.test.ts"));
        assert!(is_test_path("pkg/test_utils.py"));
        assert!(is_test_path("src/foo_test.go"));
        assert!(!is_test_path("src/main.rs"));
        assert!(!is_test_path("src/contest.rs"));
    }

    #[test]
    fn test_symbols_by_convention() {
        assert!(is_test_symbol("tests.maintenance_cmds_classified"));
        assert!(is_test_symbol("test_login"));
        assert!(!is_test_symbol("Totals.baseline"));
    }

    #[test]
    fn classification() {
        assert_eq!(
            entry_class("rust", "main", "fn", None, "fn main() {", "src/main.rs"),
            Some(EntryClass::Main)
        );
        assert_eq!(
            entry_class(
                "rust",
                "open_project_db",
                "fn",
                None,
                "pub fn open_project_db(",
                "src/db.rs"
            ),
            Some(EntryClass::Api)
        );
        assert_eq!(
            entry_class("rust", "helper", "fn", None, "fn helper() {", "src/db.rs"),
            None
        );
        assert_eq!(
            entry_class(
                "typescript",
                "loadUser",
                "fn",
                None,
                "export function loadUser(",
                "app/api.ts"
            ),
            Some(EntryClass::Api)
        );
        assert_eq!(
            entry_class(
                "rust",
                "anything",
                "fn",
                None,
                "pub fn x(",
                "tests/basic.rs"
            ),
            Some(EntryClass::Test)
        );
        assert_eq!(
            entry_class(
                "python",
                "handler",
                "def",
                None,
                "def handler():",
                "app/views.py"
            ),
            Some(EntryClass::Api)
        );
        assert_eq!(
            entry_class(
                "python",
                "_private",
                "def",
                None,
                "def _private():",
                "app/views.py"
            ),
            None
        );
        assert_eq!(
            entry_class(
                "python",
                "run",
                "def",
                None,
                "def run():",
                "pkg/__main__.py"
            ),
            Some(EntryClass::Main)
        );
    }

    #[test]
    fn framework_entry_files_are_reached_by_convention() {
        for p in [
            "app/page.tsx",
            "src/app/blog/[slug]/layout.tsx",
            "app/api/users/route.ts",
            "pages/index.tsx",
            "src/pages/api/hello.js",
            "middleware.ts",
            "src/instrumentation.ts",
        ] {
            assert!(is_framework_entry_path(p), "{p}");
        }
        for p in [
            "app/components/Button.tsx",
            "src/lib/page.tsx",
            "app/page.test.tsx",
            "pages/readme.md",
            "src/util.ts",
        ] {
            assert!(!is_framework_entry_path(p), "{p}");
        }
    }

    #[test]
    fn dynamic_imports_name_the_module_they_load() {
        let src = "const A = lazy(() => import('./views/Chart'));\n\
                   await import(\"../feature/index.js\");\n\
                   import(`./x.mjs`); import(name); import('./index')";
        assert_eq!(dynamic_import_stems(src), ["Chart", "feature", "x"]);
        assert_eq!(module_stem("./a/b.test.ts"), Some("b"));
        assert_eq!(module_stem("pkg"), Some("pkg"));
        assert_eq!(module_stem("../index"), None);
    }
}
