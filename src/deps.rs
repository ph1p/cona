use std::collections::HashSet;

/// Extract raw import specifiers from source, line-based on purpose:
/// imports are line-shaped in every supported language, and a textual scan
/// stays fail-open for files tree-sitter can't parse. Returned specs are
/// resolved (or dropped) by `resolve_import`.
pub fn extract_imports(lang: &str, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None; // multi-line rust `use … ;`
    for raw in src.lines() {
        let line = raw.trim_start();
        match lang {
            "rust" => {
                let rest = if let Some(acc) = pending.as_mut() {
                    acc.push(' ');
                    acc.push_str(line.trim_end());
                    if !line.contains(';') {
                        continue;
                    }
                    let done = pending.take().unwrap();
                    let spec = done
                        .strip_prefix("use ")
                        .unwrap_or(&done)
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim();
                    out.extend(expand_use_tree(spec));
                    continue;
                } else {
                    line.strip_prefix("pub use ")
                        .or_else(|| line.strip_prefix("use "))
                };
                if let Some(rest) = rest {
                    if !line.contains(';') {
                        pending = Some(format!("use {}", rest.trim_end()));
                        continue;
                    }
                    let spec = rest.split(';').next().unwrap_or("").trim();
                    out.extend(expand_use_tree(spec));
                } else if let Some(rest) = line
                    .strip_prefix("pub mod ")
                    .or_else(|| line.strip_prefix("mod "))
                {
                    if let Some(name) = rest.split(';').next() {
                        if rest.contains(';') && !name.trim().is_empty() {
                            out.push(format!("mod:{}", name.trim()));
                        }
                    }
                }
            }
            "python" => {
                if let Some(rest) = line.strip_prefix("import ") {
                    for part in rest.split(',') {
                        let m = part.split_whitespace().next().unwrap_or("");
                        if !m.is_empty() {
                            out.push(m.to_string());
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("from ") {
                    if let Some(m) = rest.split(" import").next() {
                        let m = m.trim();
                        if !m.is_empty() {
                            out.push(m.to_string());
                        }
                    }
                }
            }
            // javascript / typescript / tsx
            _ => {
                for marker in [
                    "from \"",
                    "from '",
                    "require(\"",
                    "require('",
                    "import(\"",
                    "import('",
                ] {
                    if let Some(pos) = line.find(marker) {
                        let rest = &line[pos + marker.len()..];
                        let quote = marker.chars().last().unwrap();
                        if let Some(end) = rest.find(quote) {
                            out.push(rest[..end].to_string());
                        }
                    }
                }
                // side-effect import: import "./x"
                if let Some(rest) = line
                    .strip_prefix("import \"")
                    .or_else(|| line.strip_prefix("import '"))
                {
                    if let Some(end) = rest.find(['"', '\'']) {
                        out.push(rest[..end].to_string());
                    }
                }
            }
        }
    }
    out
}

/// Expand a rust use-tree into leaf paths at any nesting depth:
/// `a::{b::{c, d}, e}` → [a::b::c, a::b::d, a::e]. `self`/`*` in a group
/// keep the base; `as` aliases are stripped. Pure, tested.
fn expand_use_tree(spec: &str) -> Vec<String> {
    let spec = spec.trim();
    if let Some(open) = spec.find('{') {
        let base = spec[..open].trim().trim_end_matches("::").to_string();
        // matching close brace (the tail after it is junk/whitespace).
        // Scan the tail from `open` and add the offset back, so byte indices
        // stay in one space: `find` returns a BYTE offset, and a
        // `char_indices().skip(open)` would skip that many CHARS instead —
        // any non-ASCII earlier in the line then overshot the `{`, leaving
        // `close` at `spec.len()` so the `}` and `;` were parsed as part of
        // the imported name (`use ä::{b};` yielded `b};`).
        let mut depth = 0usize;
        let mut close = spec.len();
        for (i, c) in spec[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let inner = &spec[open + 1..close];
        let mut out = Vec::new();
        // split on top-level commas only
        let mut item_depth = 0usize;
        let mut start = 0usize;
        let mut items: Vec<&str> = Vec::new();
        for (i, c) in inner.char_indices() {
            match c {
                '{' => item_depth += 1,
                '}' => item_depth = item_depth.saturating_sub(1),
                ',' if item_depth == 0 => {
                    items.push(&inner[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        items.push(&inner[start..]);
        for item in items {
            let item = item.trim();
            if item.is_empty() || item == "self" || item == "*" {
                if !base.is_empty() {
                    out.push(base.clone());
                }
                continue;
            }
            for leaf in expand_use_tree(item) {
                out.push(if base.is_empty() {
                    leaf
                } else {
                    format!("{base}::{leaf}")
                });
            }
        }
        out
    } else {
        // strip `as alias`
        let s = spec
            .split(" as ")
            .next()
            .unwrap_or(spec)
            .trim()
            .trim_end_matches("::");
        if s.is_empty() {
            Vec::new()
        } else {
            vec![s.to_string()]
        }
    }
}

/// Resolve a raw specifier to an indexed file path. Returns None for external
/// packages / std — the deps graph only maps files the index knows about.
/// `self_crates`: this project's own crate names (a binary importing its lib
/// writes `use mycrate::…`, which resolves like `crate::…`).
pub fn resolve_import(
    lang: &str,
    spec: &str,
    importer: &str,
    files: &HashSet<String>,
    self_crates: &HashSet<String>,
) -> Option<String> {
    match lang {
        "rust" => resolve_rust(spec, importer, files, self_crates),
        "python" => resolve_python(spec, importer, files),
        _ => resolve_js(spec, importer, files),
    }
}

/// Name of the external package an unresolved import pulls in, or `None` when
/// the spec is relative/internal/language-builtin (std, own crate, `./`, `.`).
/// Mirrors the skip logic of the `resolve_*` fns so an import is counted as
/// external exactly when resolution would have returned `None` for that reason.
pub fn external_name(lang: &str, spec: &str, self_crates: &HashSet<String>) -> Option<String> {
    match lang {
        "rust" => {
            if spec.starts_with("mod:") {
                return None; // local module declaration, never external
            }
            let first = spec.split("::").next()?.trim();
            if matches!(first, "crate" | "self" | "super" | "std" | "core" | "alloc")
                || first.is_empty()
                || self_crates.contains(first)
            {
                return None;
            }
            Some(first.to_string())
        }
        "python" => {
            if spec.starts_with('.') {
                return None; // relative import
            }
            let top = spec.split('.').next()?.trim();
            (!top.is_empty()).then(|| top.to_string())
        }
        _ => {
            // js/ts: bare specifier = external; scoped pkg keeps `@scope/name`
            if spec.starts_with('.') || spec.starts_with('/') {
                return None;
            }
            if let Some(rest) = spec.strip_prefix('@') {
                let (scope, tail) = rest.split_once('/')?;
                let name = tail.split('/').next()?;
                if scope.is_empty() || name.is_empty() {
                    return None;
                }
                return Some(format!("@{scope}/{name}"));
            }
            let top = spec.split('/').next()?.trim();
            (!top.is_empty()).then(|| top.to_string())
        }
    }
}

/// Own crate names of the project at `root` — walks for ALL Cargo.tomls
/// (workspace members import each other by crate name), pruning the same
/// heavy directories the indexer prunes so vendored manifests don't leak in.
pub fn self_crate_names(root: &std::path::Path) -> HashSet<String> {
    let manifests: Vec<String> = ignore::WalkBuilder::new(root)
        .max_depth(Some(4))
        .filter_entry(|e| !crate::indexer::is_excluded_dir(e.file_name().to_str().unwrap_or("")))
        .build()
        .flatten()
        .filter(|e| e.file_name() == "Cargo.toml")
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect();
    crate_names_from_manifests(&manifests)
}

/// Crate names declared in Cargo.toml files (`name = "…"`, dashes → underscores).
pub fn crate_names_from_manifests(manifests: &[String]) -> HashSet<String> {
    let mut out = HashSet::new();
    for m in manifests {
        for line in m.lines() {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix("name") {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix('=') {
                    let v = rest.trim().trim_matches('"');
                    if !v.is_empty() {
                        out.insert(v.replace('-', "_"));
                    }
                }
            }
        }
    }
    out
}

fn dir_of(path: &str) -> &str {
    path.rfind('/').map(|i| &path[..i]).unwrap_or("")
}

/// Lexically normalize `a/b/../c` → `a/c`.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn resolve_rust(
    spec: &str,
    importer: &str,
    files: &HashSet<String>,
    self_crates: &HashSet<String>,
) -> Option<String> {
    if let Some(name) = spec.strip_prefix("mod:") {
        let dir = dir_of(importer);
        for cand in [format!("{dir}/{name}.rs"), format!("{dir}/{name}/mod.rs")] {
            let c = normalize(&cand);
            if files.contains(&c) {
                return Some(c);
            }
        }
        // mod decl in main.rs/lib.rs at crate root
        return None;
    }
    let mut segs: Vec<&str> = spec.split("::").map(str::trim).collect();
    let rel_base = match segs.first().copied() {
        Some("crate") => {
            segs.remove(0);
            None
        }
        Some(first) if self_crates.contains(first) => {
            segs.remove(0);
            None
        }
        Some("self") => {
            segs.remove(0);
            Some(dir_of(importer).to_string())
        }
        Some("super") => {
            let mut dir = dir_of(importer).to_string();
            while segs.first() == Some(&"super") {
                segs.remove(0);
                dir = dir_of(&dir).to_string();
            }
            Some(dir)
        }
        _ => return None, // external crate or std
    };
    // longest prefix of segments that names a file (the tail is an item)
    for take in (1..=segs.len()).rev() {
        let joined = segs[..take].join("/");
        let cands: Vec<String> = match &rel_base {
            Some(dir) => vec![
                format!("{dir}/{joined}.rs"),
                format!("{dir}/{joined}/mod.rs"),
            ],
            None => files
                .iter()
                .filter(|f| {
                    f.ends_with(&format!("/{joined}.rs"))
                        || f.ends_with(&format!("/{joined}/mod.rs"))
                        || **f == format!("{joined}.rs")
                })
                .cloned()
                .collect(),
        };
        for cand in cands {
            let c = normalize(&cand);
            if files.contains(&c) {
                return Some(c);
            }
        }
    }
    None
}

fn resolve_python(spec: &str, importer: &str, files: &HashSet<String>) -> Option<String> {
    let (base, rest) = if let Some(stripped) = spec.strip_prefix('.') {
        // relative: each extra leading dot walks one dir up
        let mut dir = dir_of(importer).to_string();
        let mut s = stripped;
        while let Some(more) = s.strip_prefix('.') {
            dir = dir_of(&dir).to_string();
            s = more;
        }
        (Some(dir), s.to_string())
    } else {
        (None, spec.to_string())
    };
    let joined = rest.replace('.', "/");
    for take_full in [true, false] {
        let j = if take_full || !joined.contains('/') {
            joined.clone()
        } else {
            joined
                .rsplit_once('/')
                .map(|(h, _)| h.to_string())
                .unwrap_or_default()
        };
        if j.is_empty() {
            continue;
        }
        let cands: Vec<String> = match &base {
            Some(dir) => vec![format!("{dir}/{j}.py"), format!("{dir}/{j}/__init__.py")],
            None => files
                .iter()
                .filter(|f| {
                    f.ends_with(&format!("/{j}.py"))
                        || f.ends_with(&format!("/{j}/__init__.py"))
                        || **f == format!("{j}.py")
                })
                .cloned()
                .collect(),
        };
        for cand in cands {
            let c = normalize(&cand);
            if files.contains(&c) {
                return Some(c);
            }
        }
    }
    None
}

fn resolve_js(spec: &str, importer: &str, files: &HashSet<String>) -> Option<String> {
    if !spec.starts_with('.') {
        return None; // bare package specifier
    }
    let base = normalize(&format!("{}/{}", dir_of(importer), spec));
    const EXTS: [&str; 8] = crate::lang::JS_TS_EXTS;
    if files.contains(&base) {
        return Some(base);
    }
    // "./x.js" written in TS actually means x.ts on disk
    let stem = EXTS
        .iter()
        .find_map(|e| base.strip_suffix(e))
        .map(str::to_string)
        .unwrap_or_else(|| base.clone());
    for ext in EXTS {
        let cand = format!("{stem}{ext}");
        if files.contains(&cand) {
            return Some(cand);
        }
    }
    for ext in EXTS {
        let cand = format!("{base}/index{ext}");
        if files.contains(&cand) {
            return Some(cand);
        }
    }
    None
}

/// Mutual edges (a→b and b→a) — the cheapest honest cycle signal.
pub fn mutual_pairs(edges: &[(String, String)]) -> Vec<(String, String)> {
    let set: HashSet<(&str, &str)> = edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let mut out = Vec::new();
    for (a, b) in edges {
        if a < b && set.contains(&(b.as_str(), a.as_str())) {
            out.push((a.clone(), b.clone()));
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fset(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn no_crates() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn use_tree_with_non_ascii_finds_the_closing_brace() {
        // `find('{')` is a byte offset; scanning with `char_indices().skip(open)`
        // skipped that many chars instead and overshot the brace, so `close`
        // stayed at `spec.len()` and the `}`/`;` landed inside the last name.
        assert_eq!(expand_use_tree("ä::{b}"), vec!["ä::b".to_string()]);
        assert_eq!(
            expand_use_tree("äö::{b, c}"),
            vec!["äö::b".to_string(), "äö::c".to_string()]
        );
        assert_eq!(expand_use_tree("ä::{b::{c}}"), vec!["ä::b::c".to_string()]);
        // and the whole extract path, semicolon included
        assert_eq!(
            extract_imports("rust", "use ä::{b, c};\n"),
            vec!["ä::b".to_string(), "ä::c".to_string()]
        );
    }

    #[test]
    fn nested_use_tree_expands_fully() {
        let specs = extract_imports("rust", "use a::{b::{c, d}, e, f as g, self};\n");
        assert_eq!(
            specs,
            vec![
                "a::b::c".to_string(),
                "a::b::d".to_string(),
                "a::e".to_string(),
                "a::f".to_string(),
                "a".to_string(),
            ]
        );
    }

    #[test]
    fn multiline_use_and_brace_expansion() {
        let src = "use cona::{\n    dashboard, db,\n    lang,\n};\nuse crate::a::{b, c};\n";
        let specs = extract_imports("rust", src);
        assert!(specs.contains(&"cona::db".to_string()), "{specs:?}");
        assert!(specs.contains(&"cona::lang".to_string()), "{specs:?}");
        assert!(specs.contains(&"crate::a::b".to_string()), "{specs:?}");
        assert!(specs.contains(&"crate::a::c".to_string()), "{specs:?}");
    }

    #[test]
    fn self_crate_name_resolves_like_crate() {
        let files = fset(&["src/db.rs", "src/main.rs"]);
        let crates = crate_names_from_manifests(&[
            "[package]\nname = \"code-map\"\nversion = \"1\"".to_string(),
        ]);
        assert!(crates.contains("code_map"));
        assert_eq!(
            resolve_import("rust", "code_map::db", "src/main.rs", &files, &crates),
            Some("src/db.rs".into())
        );
        assert_eq!(
            resolve_import("rust", "serde_json::json", "src/main.rs", &files, &crates),
            None
        );
    }

    #[test]
    fn rust_use_and_mod_resolution() {
        let files = fset(&["src/db.rs", "src/lang.rs", "src/sub/mod.rs", "src/main.rs"]);
        let specs = extract_imports(
            "rust",
            "use crate::db::project_hash;\nmod sub;\nuse std::io::Read;\n",
        );
        assert!(specs.contains(&"crate::db::project_hash".to_string()));
        assert_eq!(
            resolve_import(
                "rust",
                "crate::db::project_hash",
                "src/main.rs",
                &files,
                &no_crates()
            ),
            Some("src/db.rs".into())
        );
        assert_eq!(
            resolve_import("rust", "mod:sub", "src/main.rs", &files, &no_crates()),
            Some("src/sub/mod.rs".into())
        );
        assert_eq!(
            resolve_import("rust", "std::io::Read", "src/main.rs", &files, &no_crates()),
            None
        );
    }

    #[test]
    fn python_absolute_and_relative() {
        let files = fset(&["pkg/util.py", "pkg/sub/mod.py", "pkg/__init__.py"]);
        assert_eq!(
            resolve_import("python", "pkg.util", "main.py", &files, &no_crates()),
            Some("pkg/util.py".into())
        );
        assert_eq!(
            resolve_import("python", ".util", "pkg/sub/mod.py", &files, &no_crates()),
            None // one dot = same dir (pkg/sub), util.py not there
        );
        assert_eq!(
            resolve_import("python", "..util", "pkg/sub/mod.py", &files, &no_crates()),
            Some("pkg/util.py".into())
        );
        let specs = extract_imports("python", "from pkg.util import x\nimport os, pkg.sub.mod\n");
        assert!(specs.contains(&"pkg.util".to_string()));
        assert!(specs.contains(&"pkg.sub.mod".to_string()));
    }

    #[test]
    fn js_relative_with_extension_swap_and_index() {
        let files = fset(&["app/lib/api.ts", "app/lib/index.ts", "app/main.ts"]);
        assert_eq!(
            resolve_import(
                "typescript",
                "./lib/api.js",
                "app/main.ts",
                &files,
                &no_crates()
            ),
            Some("app/lib/api.ts".into())
        );
        assert_eq!(
            resolve_import("typescript", "./lib", "app/main.ts", &files, &no_crates()),
            Some("app/lib/index.ts".into())
        );
        assert_eq!(
            resolve_import("typescript", "react", "app/main.ts", &files, &no_crates()),
            None
        );
        let specs = extract_imports(
            "typescript",
            "import { x } from './lib/api'\nconst y = require('./lib')\n",
        );
        assert_eq!(specs, vec!["./lib/api".to_string(), "./lib".to_string()]);
    }

    #[test]
    fn external_name_rust_skips_builtins_and_self() {
        let sc = fset(&["mycrate"]);
        assert_eq!(
            external_name("rust", "serde::Serialize", &sc).as_deref(),
            Some("serde")
        );
        assert_eq!(external_name("rust", "std::io::Read", &sc), None);
        assert_eq!(external_name("rust", "core::mem", &sc), None);
        assert_eq!(external_name("rust", "crate::db", &sc), None);
        assert_eq!(external_name("rust", "self::x", &sc), None);
        assert_eq!(external_name("rust", "super::y", &sc), None);
        assert_eq!(external_name("rust", "mycrate::z", &sc), None);
        assert_eq!(external_name("rust", "mod:foo", &sc), None);
    }

    #[test]
    fn external_name_python_and_js() {
        let e = no_crates();
        assert_eq!(
            external_name("python", "numpy.linalg", &e).as_deref(),
            Some("numpy")
        );
        assert_eq!(external_name("python", ".sibling", &e), None);
        assert_eq!(external_name("python", "..pkg.mod", &e), None);
        assert_eq!(external_name("js", "react", &e).as_deref(), Some("react"));
        assert_eq!(
            external_name("js", "lodash/debounce", &e).as_deref(),
            Some("lodash")
        );
        assert_eq!(
            external_name("js", "@tanstack/react-query", &e).as_deref(),
            Some("@tanstack/react-query")
        );
        assert_eq!(external_name("js", "./local", &e), None);
        assert_eq!(external_name("js", "../up", &e), None);
    }

    #[test]
    fn mutual_pairs_detect_cycles() {
        let edges = vec![
            ("a.rs".to_string(), "b.rs".to_string()),
            ("b.rs".to_string(), "a.rs".to_string()),
            ("a.rs".to_string(), "c.rs".to_string()),
        ];
        assert_eq!(
            mutual_pairs(&edges),
            vec![("a.rs".to_string(), "b.rs".to_string())]
        );
    }
}
