//! `dead`: symbols nothing outside their own definition names — dead-code
//! candidates. Name-based like `tree --rank` (same per-file identifier scan),
//! so it errs toward ALIVE: any same-named identifier anywhere keeps a symbol
//! off the list. What it cannot see — dynamic dispatch, reflection, framework
//! callbacks, callers outside the repo — the output says, and `[api]` marks
//! exported items whose callers may live in another repo.

use crate::commands::{clip, jout, par_map, PathFilter, LIMIT_TRAILER};
use crate::{db, entries, lang};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Kinds that name code something must reach to run. Not here: ctor/init/
/// deinit/subscript (called by syntax), var (Swift properties — `body` & co.
/// are read by frameworks), impl/extension/mod/namespace (containers).
fn candidate_kind(kind: &str) -> bool {
    matches!(
        kind,
        "fn" | "fn*"
            | "def"
            | "method"
            | "func"
            | "sub"
            | "class"
            | "struct"
            | "enum"
            | "interface"
            | "trait"
            | "protocol"
            | "type"
            | "record"
            | "object"
            | "actor"
            | "union"
            | "macro"
            | "const"
    )
}

/// Reached without being named in source: overrides, framework/runtime hooks,
/// dunder protocols, entry attributes.
fn implicitly_called(name: &str, signature: &str) -> bool {
    if name.len() > 4 && name.starts_with("__") && name.ends_with("__") {
        return true;
    }
    [
        "override ",
        "@objc",
        "@IBAction",
        "@main",
        "@Test",
        "#[test]",
        "#[no_mangle]",
    ]
    .iter()
    .any(|m| signature.contains(m))
}

/// Rust inline format args (`format!("v{VERSION}")`, `{x:>4}`) sit inside a
/// string token, so the identifier scan never sees them.
fn format_arg_uses(src: &str, names: &HashSet<&str>, counts: &mut HashMap<String, i64>) {
    for (i, _) in src.match_indices('{') {
        let rest = &src[i + 1..];
        let len = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let (ident, after) = rest.split_at(len);
        if len > 0 && after.starts_with(['}', ':']) && names.contains(ident) {
            *counts.entry(ident.to_string()).or_insert(0) += 1;
        }
    }
}

struct Cand {
    path: String,
    kind: String,
    qualified: String,
    name: String,
    start: i64,
    end: i64,
    api: bool,
}

pub fn cmd_dead(
    root: &Path,
    conn: &Connection,
    path_filter: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<(String, i64)> {
    let pf = PathFilter::new(root, path_filter);
    // Trait impls (`impl Drop for X`) and protocol extensions (`extension X:
    // P`): their methods are called through the trait, never by this name.
    let mut conforming: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT f.path, s.start_line, s.end_line FROM symbols s JOIN files f ON f.id = s.file_id
             WHERE (s.kind = 'impl' AND s.signature LIKE '% for %')
                OR (s.kind = 'extension' AND s.signature LIKE '%:%')",
        )?;
        for (p, a, b) in stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?)))?
            .flatten()
        {
            conforming.entry(p).or_default().push((a, b));
        }
    }
    let inside_conforming = |path: &str, s: i64, e: i64| {
        conforming
            .get(path)
            .is_some_and(|v| v.iter().any(|&(a, b)| a <= s && e <= b))
    };

    // Every symbol row: candidates, plus per-file definition counts so a
    // definition's own name occurrence is not taken for a use.
    let mut stmt = conn.prepare(
        "SELECT f.path, COALESCE(f.lang, ''), s.kind, s.qualified, s.name, s.parent,
                s.start_line, s.end_line, COALESCE(s.signature, '')
         FROM symbols s JOIN files f ON f.id = s.file_id ORDER BY f.path, s.start_line",
    )?;
    let mut defs: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let mut cands: Vec<Cand> = Vec::new();
    for row in stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, String>(8)?,
            ))
        })?
        .flatten()
    {
        let (path, lng, kind, qualified, name, parent, start, end, sig) = row;
        if !lang::has_callable_symbols(&lng) {
            continue;
        }
        // shell calls are `word` nodes, not identifiers: every fn would look dead
        let unscanned = lng == "bash";
        *defs
            .entry(path.clone())
            .or_default()
            .entry(name.clone())
            .or_insert(0) += 1;
        let class = entries::entry_class(&lng, &name, &kind, parent.as_deref(), &sig, &path);
        if unscanned
            || !candidate_kind(&kind)
            || !pf.ok(&path)
            || entries::is_test_path(&path)
            || entries::is_generated_path(&path)
            || entries::is_test_symbol(&qualified)
            || matches!(
                class,
                Some(entries::EntryClass::Main | entries::EntryClass::Test)
            )
            || implicitly_called(&name, &sig)
            || (parent.is_some() && inside_conforming(&path, start, end))
        {
            continue;
        }
        cands.push(Cand {
            api: matches!(class, Some(entries::EntryClass::Api)),
            path,
            kind,
            qualified,
            name,
            start,
            end,
        });
    }
    let names: HashSet<&str> = cands.iter().map(|c| c.name.as_str()).collect();

    let mut files_stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let all_files: Vec<String> = files_stmt.query_map([], |r| r.get(0))?.flatten().collect();
    // uses per name, split by where they sit: production code vs tests
    let mut prod: HashMap<&str, i64> = HashMap::new();
    let mut test: HashMap<&str, i64> = HashMap::new();
    let mut bytes = 0usize;
    let scans = par_map(&all_files, 8, |rel| {
        let src = std::fs::read_to_string(root.join(rel)).ok()?;
        let lng = lang::detect_lang(rel);
        let mut counts = lang::ident_counts_members(lng, &src, &names);
        if lng == Some("rust") {
            format_arg_uses(&src, &names, &mut counts);
        }
        Some((src.len(), counts))
    });
    for (rel, scan) in all_files.iter().zip(scans) {
        let Some((b, counts)) = scan else { continue };
        bytes += b;
        let own = defs.get(rel);
        let bucket = if entries::is_test_path(rel) {
            &mut test
        } else {
            &mut prod
        };
        for (n, c) in counts {
            let d = own.and_then(|m| m.get(&n)).copied().unwrap_or(0);
            if c > d {
                if let Some(&key) = names.get(n.as_str()) {
                    *bucket.entry(key).or_insert(0) += c - d;
                }
            }
        }
    }
    // A symbol's name is a use only outside its definition; `mod tests`
    // inside a production file is test code but scans as that file — the
    // name-based model accepts that (errs alive).
    let (mut unused, mut tests_only): (Vec<&Cand>, Vec<&Cand>) = cands
        .iter()
        .filter(|c| prod.get(c.name.as_str()).copied().unwrap_or(0) == 0)
        .partition(|c| test.get(c.name.as_str()).copied().unwrap_or(0) == 0);
    let baseline = db::orient_baseline(bytes);

    if json {
        let item = |c: &Cand, tests_only: bool| {
            serde_json::json!({"file": c.path, "kind": c.kind, "symbol": c.qualified,
                "start": c.start, "end": c.end, "api": c.api, "tests_only": tests_only})
        };
        let mut items: Vec<_> = unused.iter().map(|c| item(c, false)).collect();
        items.extend(tests_only.iter().map(|c| item(c, true)));
        return jout(&items, baseline);
    }
    let mut out = String::new();
    let section = |out: &mut String, title: &str, rows: &mut Vec<&Cand>| {
        let total = rows.len();
        if total == 0 {
            return;
        }
        let truncated = clip(rows, limit);
        out.push_str(&format!("{title} ({total}):\n"));
        let mut last = "";
        for c in rows.iter() {
            if c.path != last {
                out.push_str(&format!("{}\n", c.path));
                last = &c.path;
            }
            let tag = if c.api { "  [api]" } else { "" };
            out.push_str(&format!(
                "  {} {} :{}-{}{tag}\n",
                c.kind, c.qualified, c.start, c.end
            ));
        }
        if truncated {
            out.push_str(LIMIT_TRAILER);
        }
    };
    section(&mut out, "unreferenced", &mut unused);
    section(&mut out, "referenced only from tests", &mut tests_only);
    if out.is_empty() {
        out.push_str("no unreferenced symbols found\n");
    } else {
        out.push_str(
            "name-based: dynamic dispatch, reflection, framework callbacks and callers outside \
             the repo are invisible ([api] = exported) — confirm with `cona refs <Name>`\n",
        );
    }
    Ok((out, baseline))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_entry_points_are_not_candidates() {
        assert!(implicitly_called("__init__", "def __init__(self):"));
        assert!(implicitly_called(
            "viewDidLoad",
            "override func viewDidLoad()"
        ));
        assert!(implicitly_called("tap", "@IBAction func tap(_ s: Any)"));
        assert!(!implicitly_called("helper", "fn helper()"));
        assert!(!implicitly_called("__x", "def __x(self):"));
    }

    #[test]
    fn rust_format_args_count_as_uses() {
        let names: HashSet<&str> = ["VERSION", "w", "gone"].into_iter().collect();
        let mut c = HashMap::new();
        format_arg_uses(
            r#"format!("v{VERSION} {w:>4} {{gone}} {gone.x}")"#,
            &names,
            &mut c,
        );
        assert_eq!(c.get("VERSION"), Some(&1));
        assert_eq!(c.get("w"), Some(&1));
        // `{{gone}}` is an escaped brace: inner `{gone}` still matches — errs alive
        assert_eq!(c.get("gone"), Some(&1));
    }

    #[test]
    fn containers_and_syntax_called_kinds_are_skipped() {
        for k in ["fn", "method", "func", "struct", "class", "const"] {
            assert!(candidate_kind(k), "{k}");
        }
        for k in ["impl", "mod", "init", "ctor", "var", "extension", "heading"] {
            assert!(!candidate_kind(k), "{k}");
        }
    }
}
