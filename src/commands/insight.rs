//! Structure insight commands: entries, tests, shape, deps.

use super::{
    jout, locate_fresh, render_symbol_body, scan_ref_sites, BudgetOut, ENCLOSING_SYMBOL_SQL,
};
use crate::{db, deps, entries, indexer, lang};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub fn cmd_entries(
    conn: &Connection,
    prefix: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<(String, i64)> {
    struct Row {
        path: String,
        lang: String,
        name: String,
        qualified: String,
        kind: String,
        parent: Option<String>,
        start: i64,
        end: i64,
        sig: String,
        size: i64,
    }
    let mut sql = String::from(
        "SELECT f.path, f.lang, s.name, s.qualified, s.kind, s.parent, s.start_line, s.end_line,
                s.signature, f.size
         FROM symbols s JOIN files f ON f.id = s.file_id",
    );
    if prefix.is_some() {
        sql.push_str(" WHERE f.path LIKE ?1 || '%'");
    }
    sql.push_str(" ORDER BY f.path, s.start_line");
    let mut stmt = conn.prepare(&sql)?;
    let params = rusqlite::params_from_iter(prefix.iter());
    let rows: Vec<Row> = stmt
        .query_map(params, |r| {
            Ok(Row {
                path: r.get(0)?,
                lang: r.get(1)?,
                name: r.get(2)?,
                qualified: r.get(3)?,
                kind: r.get(4)?,
                parent: r.get(5)?,
                start: r.get(6)?,
                end: r.get(7)?,
                sig: r.get(8)?,
                size: r.get(9)?,
            })
        })?
        .flatten()
        .collect();
    let mut mains: Vec<&Row> = Vec::new();
    let mut api: Vec<&Row> = Vec::new();
    let mut test_syms = 0usize;
    let mut test_files: HashSet<&str> = HashSet::new();
    let mut involved: HashMap<&str, i64> = HashMap::new();
    for row in &rows {
        match entries::entry_class(
            &row.lang,
            &row.name,
            &row.kind,
            row.parent.as_deref(),
            &row.sig,
            &row.path,
        ) {
            Some(entries::EntryClass::Main) => {
                mains.push(row);
                involved.insert(row.path.as_str(), row.size);
            }
            Some(entries::EntryClass::Api) => {
                api.push(row);
                involved.insert(row.path.as_str(), row.size);
            }
            Some(entries::EntryClass::Test) => {
                test_syms += 1;
                test_files.insert(row.path.as_str());
            }
            None => {}
        }
    }
    let baseline = db::est_tokens(involved.values().sum::<i64>() as usize);
    if json {
        let entry_json = |r: &&Row| {
            serde_json::json!({"kind": r.kind, "symbol": r.qualified, "file": r.path,
                "start": r.start, "end": r.end, "sig": r.sig})
        };
        let obj = serde_json::json!({
            "main": mains.iter().map(entry_json).collect::<Vec<_>>(),
            "api": api.iter().take(limit).map(entry_json).collect::<Vec<_>>(),
            "api_total": api.len(),
            "test_symbols": test_syms,
            "test_files": test_files.len(),
        });
        return jout(&obj, baseline);
    }
    let mut out = String::new();
    if !mains.is_empty() {
        out.push_str(&format!("── entry points ({}) ──\n", mains.len()));
        for r in &mains {
            out.push_str(&format!(
                "  {} {}  {}:{}-{}\n",
                r.kind, r.qualified, r.path, r.start, r.end
            ));
        }
    }
    if !api.is_empty() {
        out.push_str(&format!(
            "── exported/public API ({}{}) ──\n",
            api.len(),
            if api.len() > limit {
                format!(", top {limit}")
            } else {
                String::new()
            }
        ));
        for r in api.iter().take(limit) {
            out.push_str(&format!(
                "  {} {}  {}:{}  {}\n",
                r.kind, r.qualified, r.path, r.start, r.sig
            ));
        }
    }
    out.push_str(&format!(
        "── tests: {} test symbols in {} files ──\n",
        test_syms,
        test_files.len()
    ));
    if mains.is_empty() && api.is_empty() && test_syms == 0 {
        out.push_str("no entry points recognized (heuristics: main fns, pub/export symbols, test conventions)\n");
    }
    Ok((out, baseline))
}

pub fn cmd_tests(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    json: bool,
) -> Result<(String, i64)> {
    // resolve first (invariant 4): a typo errors with candidates instead of
    // silently reporting "no references at all"
    let (def_path, def_s, def_e, q) = locate_fresh(root, conn, symbol, None)?;
    let name = db::name_tail(&q).to_string();
    let mut test_hits: Vec<(String, i64, String)> = Vec::new(); // file, line, enclosing
    let mut other_refs = 0usize;
    let mut bytes = 0usize;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut counted_files: HashSet<String> = HashSet::new();
    scan_ref_sites(root, conn, &name, None, |rel, ln, encl, _, fsrc| {
        if counted_files.insert(rel.to_string()) {
            bytes += fsrc.len();
        }
        // the definition itself is not a test reference — skip by LOCATION;
        // matching the enclosing name would also swallow recursive calls and
        // refs inside same-named symbols elsewhere
        if rel == def_path && ln >= def_s && ln <= def_e && db::name_tail(encl) == name {
            return true;
        }
        if entries::is_test_path(rel) || entries::is_test_symbol(encl) {
            if seen.insert((rel.to_string(), encl.to_string())) {
                test_hits.push((rel.to_string(), ln, encl.to_string()));
            }
        } else {
            other_refs += 1;
        }
        true
    })?;
    let baseline = db::est_tokens(bytes);
    if json {
        let obj = serde_json::json!({
            "symbol": name,
            "tests": test_hits.iter().map(|(f, l, e)| serde_json::json!({"file": f, "line": l, "in": e})).collect::<Vec<_>>(),
            "non_test_refs": other_refs,
        });
        return jout(&obj, baseline);
    }
    let mut out = String::new();
    if test_hits.is_empty() {
        if other_refs == 0 {
            out.push_str(&format!(
                "'{name}' has no references at all (unused or dynamic-only)\n"
            ));
        } else {
            out.push_str(&format!(
                "NO tests reference '{name}' — {other_refs} non-test reference(s) exist. Untested.\n"
            ));
        }
    } else {
        out.push_str(&format!(
            "tests exercising '{name}' ({}):\n",
            test_hits.len()
        ));
        for (f, l, e) in &test_hits {
            if e.is_empty() {
                out.push_str(&format!("  {f}:{l}\n"));
            } else {
                out.push_str(&format!("  {e}  {f}:{l}\n"));
            }
        }
    }
    Ok((out, baseline))
}

pub fn cmd_shape(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    budget: i64,
    kind: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let (path, s, e, q) = locate_fresh(root, conn, symbol, kind)?;
    let src = std::fs::read_to_string(root.join(&path))?;
    let lines: Vec<&str> = src.lines().collect();
    let idents = lang::idents_in_range(lang::detect_lang(&path), &src, s as usize, e as usize);
    let name = db::name_tail(&q).to_string();
    // type-kind list comes from lang.rs — the one place that mints kind labels
    let kind_list = lang::TYPE_KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    let mut type_stmt = conn.prepare(&format!(
        "SELECT s.kind, s.qualified, f.path, s.start_line, s.end_line, s.signature, f.size
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.name = ?1 AND s.kind IN ({kind_list})
         ORDER BY f.path, s.start_line LIMIT 2",
    ))?;
    type TypeRow = (String, String, String, i64, i64, String, i64);
    let mut types: Vec<(TypeRow, bool)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for ident in idents {
        if ident == name {
            continue;
        }
        // invariant 2: type bodies are printed from these line numbers below —
        // refresh stale defining files and re-query before trusting them
        let mut rows: Vec<TypeRow> = Vec::new();
        for pass in 0..2 {
            rows = type_stmt
                .query_map([&ident], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })?
                .flatten()
                .filter(|(_, tq, tp, ..)| !(*tp == path && *tq == q))
                .collect();
            if pass == 1 {
                break;
            }
            let mut any_stale = false;
            for (_, _, tp, ..) in &rows {
                any_stale |= indexer::ensure_fresh(root, conn, tp);
            }
            if !any_stale {
                break;
            }
        }
        let amb = rows.len() > 1;
        for row in rows {
            if seen.insert(row.1.clone()) {
                types.push((row, amb));
            }
        }
        if types.len() >= 16 {
            break;
        }
    }
    let mut bytes = src.len() as i64;
    for ((.., size), _) in &types {
        bytes += size;
    }
    let baseline = db::est_tokens(bytes as usize);

    let mut seed = String::new();
    render_symbol_body(&mut seed, &q, &path, s, e, &lines, &[]);
    let mut bo = BudgetOut::new(seed, budget);
    let mut json_types: Vec<serde_json::Value> = Vec::new();
    if !types.is_empty() {
        bo.push_always(&format!("── referenced types ({}) ──\n", types.len()));
    }
    // related types cluster in the same file — read each defining file once
    let mut src_cache: HashMap<&str, String> = HashMap::new();
    src_cache.insert(path.as_str(), src.clone());
    for ((kind, tq, tp, ts, te, sig, _), amb) in &types {
        let small = te - ts <= 12;
        let mark = if *amb { "  ·ambiguous" } else { "" };
        let mut block = if small {
            format!("  {kind} {tq}  {tp}:{ts}-{te}{mark}\n")
        } else {
            format!("  {kind} {tq}  {tp}:{ts}-{te}  {sig}{mark}\n")
        };
        if small {
            let cached = match src_cache.entry(tp.as_str()) {
                std::collections::hash_map::Entry::Occupied(o) => Some(o.into_mut()),
                std::collections::hash_map::Entry::Vacant(v) => {
                    std::fs::read_to_string(root.join(tp))
                        .ok()
                        .map(|f| v.insert(f))
                }
            };
            if let Some(tsrc) = cached {
                for (i, l) in tsrc
                    .lines()
                    .skip((*ts as usize).saturating_sub(1))
                    .take((*te - *ts + 1) as usize)
                    .enumerate()
                {
                    block.push_str(&format!("{:>7} {}\n", *ts as usize + i, l));
                }
            }
        }
        if !bo.try_push(&block) {
            break;
        }
        json_types.push(serde_json::json!({
            "kind": kind, "symbol": tq, "file": tp, "start": ts, "end": te,
            "sig": sig, "ambiguous": amb,
        }));
    }
    let out = bo.finish("… truncated (raise --budget)\n");
    if json {
        let obj = serde_json::json!({
            "symbol": {"file": path, "symbol": q, "start": s, "end": e},
            "types": json_types,
        });
        return jout(&obj, baseline);
    }
    Ok((out, baseline))
}

pub fn cmd_deps(
    root: &Path,
    conn: &Connection,
    prefix: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let mut stmt = conn.prepare("SELECT path, lang, size FROM files ORDER BY path")?;
    let files: Vec<(String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .flatten()
        .collect();
    let all: HashSet<String> = files.iter().map(|(p, ..)| p.clone()).collect();
    // own crate names so `use mycrate::…` resolves like `crate::…` — only
    // needed when the project has Rust files at all
    let self_crates = if files.iter().any(|(_, l, _)| l == "rust") {
        deps::self_crate_names(root)
    } else {
        HashSet::new()
    };
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut ext_count: HashMap<String, usize> = HashMap::new();
    // local module names (`mod foo;`) so a bare `use foo::…` of a sibling
    // module is never mistaken for an external crate
    let mut local_mods: HashSet<String> = HashSet::new();
    let mut bytes = 0usize;
    for (rel, flang, _) in &files {
        if let Some(p) = prefix {
            if !rel.starts_with(p) {
                continue;
            }
        }
        let Ok(src) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        bytes += src.len();
        let mut seen: HashSet<String> = HashSet::new();
        let mut seen_ext: HashSet<String> = HashSet::new();
        for spec in deps::extract_imports(flang, &src) {
            if let Some(name) = spec.strip_prefix("mod:") {
                local_mods.insert(name.to_string());
            }
            if let Some(dst) = deps::resolve_import(flang, &spec, rel, &all, &self_crates) {
                if dst != *rel && seen.insert(dst.clone()) {
                    edges.push((rel.clone(), dst));
                }
            } else if let Some(name) = deps::external_name(flang, &spec, &self_crates) {
                // count each external package once per importing file
                if seen_ext.insert(name.clone()) {
                    *ext_count.entry(name).or_insert(0) += 1;
                }
            }
        }
    }
    // rust file stems are implicit module names too (foo.rs → `mod foo`)
    for (p, l, _) in &files {
        if l == "rust" {
            if let Some(stem) = p.rsplit('/').next().and_then(|f| f.strip_suffix(".rs")) {
                local_mods.insert(stem.to_string());
            }
        }
    }
    ext_count.retain(|name, _| !local_mods.contains(name));
    let mut externals: Vec<(String, usize)> = ext_count.into_iter().collect();
    externals.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let baseline = db::est_tokens(bytes);
    let cycles = deps::mutual_pairs(&edges);
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    for (_, dst) in &edges {
        *in_degree.entry(dst.as_str()).or_insert(0) += 1;
    }
    let mut popular: Vec<(&str, usize)> = in_degree.into_iter().collect();
    popular.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    if json {
        let obj = serde_json::json!({
            "edges": edges.iter().map(|(a, b)| serde_json::json!([a, b])).collect::<Vec<_>>(),
            "cycles": cycles.iter().map(|(a, b)| serde_json::json!([a, b])).collect::<Vec<_>>(),
            "most_imported": popular.iter().take(10).map(|(p, n)| serde_json::json!({"file": p, "imported_by": n})).collect::<Vec<_>>(),
            "external": externals.iter().take(20).map(|(p, n)| serde_json::json!({"package": p, "imported_by": n})).collect::<Vec<_>>(),
        });
        return jout(&obj, baseline);
    }
    let mut out = String::new();
    let mut by_src: HashMap<&str, Vec<&str>> = HashMap::new();
    for (a, b) in &edges {
        by_src.entry(a.as_str()).or_default().push(b.as_str());
    }
    let mut srcs: Vec<&&str> = by_src.keys().collect();
    srcs.sort();
    for s in srcs {
        let mut ds = by_src[*s].clone();
        ds.sort();
        out.push_str(&format!("{s} → {}\n", ds.join(", ")));
    }
    if edges.is_empty() {
        out.push_str("no resolvable internal imports (externals/std are not shown)\n");
    }
    if !popular.is_empty() {
        out.push_str("── most imported ──\n");
        for (p, n) in popular.iter().take(10) {
            out.push_str(&format!("  {n:>3}×  {p}\n"));
        }
    }
    if !cycles.is_empty() {
        out.push_str("── cycles (mutual imports) ──\n");
        for (a, b) in &cycles {
            out.push_str(&format!("  {a} ⇄ {b}\n"));
        }
    }
    if !externals.is_empty() {
        out.push_str("── external deps (files importing) ──\n");
        for (p, n) in externals.iter().take(20) {
            out.push_str(&format!("  {n:>3}×  {p}\n"));
        }
    }
    Ok((out, baseline))
}

/// `check` — tree-sitter parse diagnostics for a file (NOT a compiler; catches
/// syntactic breakage only). This is the same gate `edit` runs internally,
/// exposed as a standalone command so an agent can confirm a file still parses
/// after a manual edit without shelling a full build. With no file, checks every
/// file changed vs HEAD (uncommitted + untracked).
pub fn cmd_check(
    root: &Path,
    conn: &Connection,
    file: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let files: Vec<String> = match file {
        Some(f) => vec![f.to_string()],
        None => changed_files(root)?,
    };
    let mut enc = conn.prepare(ENCLOSING_SYMBOL_SQL)?;
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut out = String::new();
    let mut baseline = 0i64;
    let mut total_errors = 0usize;
    for path in &files {
        let abs = root.join(path);
        let Some(language) = lang::detect_lang(path) else {
            continue; // unknown language — nothing to parse
        };
        let Ok(src) = std::fs::read_to_string(&abs) else {
            continue;
        };
        baseline += db::est_tokens(src.len());
        let errors = lang::syntax_errors(language, &src)?;
        total_errors += errors.len();
        if !json {
            if errors.is_empty() {
                out.push_str(&format!("{path}: ok\n"));
            } else {
                out.push_str(&format!("{path}: {} syntax error(s)\n", errors.len()));
                for line in &errors {
                    // name the enclosing symbol so the agent jumps straight there
                    let sym: Option<String> = enc
                        .query_row(rusqlite::params![path, *line as i64], |r| {
                            r.get::<_, String>(0)
                        })
                        .ok();
                    match sym {
                        Some(q) => out.push_str(&format!("  L{line} (in {q})\n")),
                        None => out.push_str(&format!("  L{line}\n")),
                    }
                }
            }
        } else {
            results.push(serde_json::json!({ "file": path, "errors": errors }));
        }
    }
    if files.is_empty() && !json {
        out.push_str("no changed files to check\n");
    }
    if json {
        return jout(
            &serde_json::json!({ "files": results, "errors": total_errors }),
            baseline,
        );
    }
    Ok((out, baseline))
}

/// Files changed vs HEAD (tracked-modified + untracked), used by `check` with no
/// argument. Mirrors the `diff` command's "includes uncommitted + untracked" scope.
fn changed_files(root: &Path) -> Result<Vec<String>> {
    let run = |args: &[&str]| -> Option<String> {
        std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let mut set: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        for l in s.lines() {
            let p = l.trim();
            if !p.is_empty() && !set.iter().any(|x| x == p) {
                set.push(p.to_string());
            }
        }
    };
    if let Some(s) = run(&["diff", "--name-only", "HEAD"]) {
        push(&s);
    }
    if let Some(s) = run(&["ls-files", "--others", "--exclude-standard"]) {
        push(&s);
    }
    Ok(set)
}

/// `impact` — pre-edit blast radius for a symbol, fusing the pieces an agent
/// would otherwise gather in four calls: references, immediate callers, tests
/// that exercise it, and recent git history. Answers "is it safe to change?".
pub fn cmd_impact(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    json: bool,
) -> Result<(String, i64)> {
    use super::super::commands::{callgraph, history, query};
    // resolve once so a typo errors with candidates (invariant 4) before work
    let (path, _, _, q) = locate_fresh(root, conn, symbol, None)?;
    // refs match identifier tokens — a qualified `Parent.name` never occurs in
    // source, so scan for the bare name (q stays for calls/tests/blame)
    let (refs, b_refs) = query::cmd_refs(root, conn, db::name_tail(&q), 100, false)?;
    let (callers, _) = callgraph::cmd_calls(root, conn, &q, 1, true, false)
        .unwrap_or_else(|_| ("(not in call graph)\n".to_string(), 0));
    let (tests, _) = cmd_tests(root, conn, &q, false)?;
    let (blame, _) = history::cmd_blame(root, conn, &q, 5, false)
        .unwrap_or_else(|_| ("(no git history)\n".to_string(), 0));

    if json {
        return jout(
            &serde_json::json!({
                "symbol": q, "file": path,
                "refs": refs, "callers": callers, "tests": tests, "blame": blame,
            }),
            b_refs,
        );
    }
    let mut out = format!("impact of {q}  ({path})\n");
    out.push_str("── references ──\n");
    out.push_str(&refs);
    out.push_str("── immediate callers ──\n");
    out.push_str(&callers);
    out.push_str("── tests ──\n");
    out.push_str(&tests);
    out.push_str("── recent history ──\n");
    out.push_str(&blame);
    Ok((out, b_refs))
}
