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
            .map(|(stem, _)| stem.ends_with("_test") || stem.ends_with("_spec"))
            .unwrap_or(false)
}

/// True when a symbol (by its qualified name / name) is a test by naming
/// convention — e.g. inside a `tests` module or named `test_*`.
pub fn is_test_symbol(qualified: &str) -> bool {
    qualified.split('.').any(|seg| {
        seg == "tests" || seg == "test" || seg.starts_with("test_") || seg.ends_with("_test")
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
}
