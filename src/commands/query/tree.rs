//! `tree` and `tree --rank`: project structure and fan-in ranking.

use crate::commands::{jout, BudgetOut, PathFilter};
use crate::{db, lang};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub fn cmd_tree(
    root: &Path,
    conn: &Connection,
    budget: i64,
    path_filter: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let pf = PathFilter::new(root, path_filter);
    let mut stmt = conn.prepare(
        "SELECT f.path, s.kind, s.qualified, s.start_line, s.end_line, f.size
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.parent IS NULL
         ORDER BY f.path, s.start_line",
    )?;
    let rows: Vec<(String, String, String, i64, i64, i64)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .flatten()
        .collect();

    // Baseline: sum each included file's size once (rows are path-ordered).
    let mut bytes: i64 = 0;
    if json {
        let mut current = "";
        let mut items = Vec::new();
        for (p, k, q, s, e, size) in &rows {
            if !pf.ok(p) {
                continue;
            }
            if p.as_str() != current {
                current = p;
                bytes += size;
            }
            items
                .push(serde_json::json!({"file": p, "kind": k, "symbol": q, "start": s, "end": e}));
        }
        let out = format!("{}\n", serde_json::to_string(&items)?);
        return Ok((out, db::orient_baseline(bytes as usize)));
    }

    let mut bo = BudgetOut::new(String::new(), budget);
    let mut current = String::new();
    for (path, kind, name, s, e, size) in rows {
        if !pf.ok(&path) {
            continue;
        }
        let mut chunk = String::new();
        let new_file = path != current;
        if new_file {
            chunk.push_str(&format!("{path}\n"));
            current = path.clone();
        }
        chunk.push_str(&format!("  {kind} {name} :{s}-{e}\n"));
        if !bo.try_push(&chunk) {
            break;
        }
        if new_file {
            bytes += size;
        }
    }
    if bo.out.is_empty() {
        bo.push_always("no symbols indexed — run `cona index`\n");
    }
    let out = bo.finish("… truncated (raise --budget or filter with --path)\n");
    Ok((out, db::orient_baseline(bytes as usize)))
}

/// Rank top-level symbols by fan-in: occurrences of the name in files OTHER
/// than the defining one (same rules as `refs`). The "what is load-bearing"
/// view, à la Aider's repo map.
pub fn cmd_tree_rank(
    root: &Path,
    conn: &Connection,
    budget: i64,
    path_filter: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let pf = PathFilter::new(root, path_filter);
    struct RankSym {
        path: String,
        kind: String,
        qualified: String,
        name: String,
        start: i64,
        end: i64,
        lang: String,
        signature: String,
    }
    let mut stmt = conn.prepare(
        "SELECT f.path, s.kind, s.qualified, s.name, s.start_line, s.end_line,
                COALESCE(f.lang, ''), COALESCE(s.signature, '')
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.parent IS NULL ORDER BY f.path, s.start_line",
    )?;
    let syms: Vec<RankSym> = stmt
        .query_map([], |r| {
            Ok(RankSym {
                path: r.get(0)?,
                kind: r.get(1)?,
                qualified: r.get(2)?,
                name: r.get(3)?,
                start: r.get(4)?,
                end: r.get(5)?,
                lang: r.get(6)?,
                signature: r.get(7)?,
            })
        })?
        .flatten()
        // Fan-in counts names, not resolved references, so a symbol nothing
        // outside its file can reach only collects other files' same-named
        // locals (`x`, `run`, `wd` in every test). Rank only what can be
        // imported: no test code, and in js/ts/rust only exported items.
        .filter(|s| !crate::entries::is_test_path(&s.path) && lang::has_callable_symbols(&s.lang))
        .collect();
    let names: HashSet<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    let defining: HashSet<&str> = syms.iter().map(|s| s.path.as_str()).collect();

    // One pass: global totals per name plus each defining file's own counts,
    // so fan-in = total − self.
    let mut files_stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let all_files: Vec<String> = files_stmt.query_map([], |r| r.get(0))?.flatten().collect();
    let mut total: HashMap<String, i64> = HashMap::new();
    // js/ts names count only where a file imports them — the same short name
    // (`at`, `step`) is a local in half the files that never import it
    let mut imported_total: HashMap<String, i64> = HashMap::new();
    let mut self_counts: HashMap<String, HashMap<String, i64>> = HashMap::new();
    // js/ts names exported by list (`export { a, b }`, `export default a`)
    let mut listed: HashMap<String, HashSet<String>> = HashMap::new();
    let mut bytes: usize = 0;
    for rel in &all_files {
        let Ok(src) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        bytes += src.len();
        let counts = lang::ident_counts(lang::detect_lang(rel), &src, &names);
        for (n, c) in &counts {
            *total.entry(n.clone()).or_insert(0) += c;
        }
        if matches!(
            lang::detect_lang(rel),
            Some("javascript" | "typescript" | "tsx")
        ) {
            let imports = imported_names(&src);
            for (n, c) in &counts {
                if imports.as_ref().is_none_or(|i| i.contains(n)) {
                    *imported_total.entry(n.clone()).or_insert(0) += c;
                }
            }
        }
        if defining.contains(rel.as_str()) && !counts.is_empty() {
            self_counts.insert(rel.clone(), counts);
        }
        if defining.contains(rel.as_str()) && src.contains("export") {
            listed.insert(rel.clone(), export_list_names(&src));
        }
    }

    let mut ranked: Vec<(i64, &RankSym)> = syms
        .iter()
        .filter(|sym| pf.ok(&sym.path))
        // `mod x;` soaks up its module's fan-in but points at no code — drop it
        .filter(|sym| !(sym.kind == "mod" && sym.start == sym.end))
        .filter(|sym| {
            importable(&sym.lang, &sym.signature)
                || listed.get(&sym.path).is_some_and(|l| l.contains(&sym.name))
        })
        .map(|sym| {
            if matches!(sym.lang.as_str(), "javascript" | "typescript" | "tsx") {
                return (imported_total.get(&sym.name).copied().unwrap_or(0), sym);
            }
            let own = self_counts
                .get(&sym.path)
                .and_then(|c| c.get(&sym.name))
                .copied()
                .unwrap_or(0);
            (total.get(&sym.name).copied().unwrap_or(0) - own, sym)
        })
        .collect();
    // name before path: same-named symbols form one run for the collapse below
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.name.cmp(&b.1.name))
            .then_with(|| a.1.path.cmp(&b.1.path))
    });

    // Baseline: ranking required reading every indexed file once — capped,
    // nobody reads a whole large repo to orient.
    let baseline = db::orient_baseline(bytes);
    if json {
        let items: Vec<_> = ranked
            .iter()
            .map(|(fan, sym)| {
                serde_json::json!({"fan_in": fan, "kind": sym.kind, "symbol": sym.qualified,
                    "file": sym.path, "start": sym.start, "end": sym.end})
            })
            .collect();
        return jout(&items, baseline);
    }
    let mut bo = BudgetOut::new(String::new(), budget);
    let mut i = 0;
    while i < ranked.len() {
        let (fan, sym) = &ranked[i];
        // fan-in is per NAME: same-named runs (16× `mod tests`) collapse to one row
        let run = ranked[i..]
            .iter()
            .take_while(|(f, s2)| f == fan && s2.name == sym.name && s2.kind == sym.kind)
            .count();
        let line = if run >= 3 {
            format!("{fan:>4}×  {} {}  (×{run} files)\n", sym.kind, sym.name)
        } else {
            let RankSym {
                path: p,
                kind: k,
                qualified: q,
                start: s,
                end: e,
                ..
            } = sym;
            format!("{fan:>4}×  {k} {q}  {p}:{s}-{e}\n")
        };
        if !bo.try_push(&line) {
            break;
        }
        i += if run >= 3 { run } else { 1 };
    }
    if bo.out.is_empty() {
        bo.push_always("no symbols indexed — run `cona index`\n");
    }
    let out = bo.finish("… truncated (raise --budget or filter with --path)\n");
    Ok((out, baseline))
}

/// Whether another file can reach a top-level symbol, judged by its signature:
/// js/ts need `export`, rust `pub`; other languages have no file-private
/// marker to read, so everything counts.
fn importable(lang: &str, signature: &str) -> bool {
    let sig = signature.trim_start();
    match lang {
        "javascript" | "typescript" | "tsx" => sig.starts_with("export "),
        "rust" => sig.starts_with("pub ") || sig.starts_with("pub("),
        _ => true,
    }
}

/// Names a js/ts file exports by list rather than at the declaration:
/// `export { a, b as c }` (the local `a`, `b`) and `export default a;`.
fn export_list_names(src: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut rest = src;
    while let Some(i) = rest.find("export") {
        rest = rest[i + "export".len()..].trim_start();
        if let Some(body) = rest.strip_prefix('{') {
            let Some(end) = body.find('}') else { break };
            for item in body[..end].split(',') {
                let local = item.split_whitespace().next().unwrap_or("");
                if !local.is_empty() && local != "type" {
                    out.insert(local.to_string());
                }
            }
            rest = &body[end..];
        } else if let Some(def) = rest.strip_prefix("default ") {
            let name: String = def
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect();
            if !name.is_empty() {
                out.insert(name);
            }
        }
    }
    out
}

/// Names a js/ts file brings in by import or re-export (`import { a, b as c }`,
/// `import d from`, `export { e } from`). `None` when a namespace import
/// (`import * as ns`) can reach any name — then every occurrence counts.
fn imported_names(src: &str) -> Option<HashSet<String>> {
    const SKIP: &[&str] = &[
        "import", "export", "type", "typeof", "as", "default", "from",
    ];
    let mut out = HashSet::new();
    let mut stmt = String::new();
    for raw in src.lines() {
        let line = raw.trim();
        let opens = line.starts_with("import ")
            || line.starts_with("import{")
            || (line.starts_with("export ") && line.contains(" from "))
            || (line.starts_with("export {") && !line.contains('}'));
        if stmt.is_empty() && !opens {
            continue;
        }
        stmt.push(' ');
        stmt.push_str(line);
        let done = stmt.contains(" from ") || line.ends_with(';') || line.starts_with("} from");
        if !done {
            continue;
        }
        let (head, spec) = stmt.split_once(" from ").unwrap_or((&stmt, ""));
        // a bare package (`vitest`, `react`) is not this repo's code; scoped,
        // relative and aliased specifiers may be
        let spec = spec.trim().trim_start_matches(['\'', '"', '`']);
        if !spec.is_empty() && !spec.starts_with(['.', '/', '@', '#', '~']) && !spec.contains('/') {
            stmt.clear();
            continue;
        }
        if head.contains('*') {
            return None;
        }
        let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
        for word in head.split(|c: char| !ident(c)) {
            if !word.is_empty() && !SKIP.contains(&word) {
                out.insert(word.to_string());
            }
        }
        stmt.clear();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exported_items_are_importable() {
        assert!(importable("typescript", "export function at(tx: number)"));
        assert!(!importable(
            "typescript",
            "const wd = floorWorld(map).world;"
        ));
        assert!(importable("rust", "pub(crate) fn first_line_sig("));
        assert!(!importable("rust", "fn helper() {"));
        assert!(importable("python", "def run():"));
    }

    #[test]
    fn imports_name_what_they_bring_in() {
        let src = "import { a, b as c } from './x';\nimport d from './y';\nimport {\n  type E,\n  f,\n} from '@s/z';\nexport { g } from './g';\nconst at = 1;\n";
        let names = imported_names(src).unwrap();
        for n in ["a", "b", "c", "d", "E", "f", "g"] {
            assert!(names.contains(n), "{n}");
        }
        assert!(!names.contains("at"));
        let ext =
            imported_names("import { describe } from 'vitest';\nimport { x } from '@idle/sim';\n")
                .unwrap();
        assert!(!ext.contains("describe") && ext.contains("x"));
        assert!(imported_names("import * as ns from './x';\n").is_none());
    }

    #[test]
    fn export_lists_name_their_locals() {
        let names =
            export_list_names("const a = 1;\nexport { a, b as c, type T };\nexport default run;\n");
        for n in ["a", "b", "run"] {
            assert!(names.contains(n), "{n}");
        }
        assert!(!names.contains("c"));
    }
}
