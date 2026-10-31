//! `diff`: changed symbols instead of changed lines.

use crate::commands::jout;
use crate::{db, diffmap, gitmap, indexer, lang};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Maps `git diff --unified=0` new-side ranges onto the (fresh) symbol index
/// and reports the innermost symbols touched. Untracked code files appear as
/// whole new files.
pub fn cmd_diff(root: &Path, conn: &Connection, gitref: &str, json: bool) -> Result<(String, i64)> {
    let diff_out = gitmap::run_git(root, &["diff", "--unified=0", gitref])?;
    let mut changes = diffmap::parse_unified(&diff_out);

    // untracked (but not ignored) code files = entirely new
    let status = gitmap::run_git(root, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    let mut untracked: HashSet<String> = HashSet::new();
    for line in status.lines() {
        if let Some(p) = line.strip_prefix("?? ") {
            untracked.insert(p.to_string());
        }
    }
    changes.retain(|c| lang::detect_lang(&c.path).is_some());
    let untracked_code: Vec<String> = {
        let mut v: Vec<String> = untracked
            .into_iter()
            .filter(|p| lang::detect_lang(p).is_some())
            .collect();
        v.sort();
        v
    };

    // (file, status, [symbols]) — symbols as (kind, qualified, start, end)
    type SymRow = (String, String, i64, i64);
    let mut report: Vec<(String, &'static str, Vec<SymRow>)> = Vec::new();
    let mut bytes: i64 = 0;
    let mut sym_stmt = conn.prepare(
        "SELECT s.kind, s.qualified, s.start_line, s.end_line
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE f.path = ?1 ORDER BY s.start_line, s.end_line DESC",
    )?;
    let mut size_stmt = conn.prepare("SELECT size FROM files WHERE path = ?1")?;

    let mut handle_file = |path: &str,
                           ranges: Option<&[(i64, i64)]>,
                           sym_stmt: &mut rusqlite::Statement,
                           size_stmt: &mut rusqlite::Statement|
     -> Result<Vec<SymRow>> {
        // symbol ranges come from the index — refresh before mapping
        indexer::ensure_fresh(root, conn, path);
        bytes += size_stmt
            .query_row([path], |r| r.get::<_, i64>(0))
            .unwrap_or(0);
        let all: Vec<SymRow> = sym_stmt
            .query_map([path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .flatten()
            .collect();
        let hit: Vec<SymRow> = match ranges {
            // new file: top-level symbols are the summary
            None => all.iter().filter(|s| !s.1.contains('.')).cloned().collect(),
            Some(ranges) => {
                let touched: Vec<SymRow> = all
                    .iter()
                    .filter(|(_, _, s, e)| diffmap::overlaps(*s, *e, ranges))
                    .cloned()
                    .collect();
                // drop a container only when ALL its changed lines lie inside
                // nested hit symbols (else the container itself changed)
                touched
                    .iter()
                    .filter(|(_, q, s, e)| {
                        let nested: Vec<(i64, i64)> = touched
                            .iter()
                            .filter(|(_, q2, s2, e2)| {
                                q2 != q && *s2 >= *s && *e2 <= *e && (*s2 > *s || *e2 < *e)
                            })
                            .map(|(_, _, s2, e2)| (*s2, *e2))
                            .collect();
                        nested.is_empty() || diffmap::has_uncovered(*s, *e, ranges, &nested)
                    })
                    .cloned()
                    .collect()
            }
        };
        Ok(hit)
    };

    for c in &changes {
        if c.deleted {
            report.push((c.path.clone(), "deleted", Vec::new()));
            continue;
        }
        let syms = handle_file(&c.path, Some(&c.ranges), &mut sym_stmt, &mut size_stmt)?;
        report.push((c.path.clone(), "modified", syms));
    }
    for p in &untracked_code {
        let syms = handle_file(p, None, &mut sym_stmt, &mut size_stmt)?;
        report.push((p.clone(), "new", syms));
    }
    let baseline = db::est_tokens(bytes as usize);

    if json {
        let items: Vec<_> = report
            .iter()
            .flat_map(|(file, status, syms)| {
                if syms.is_empty() {
                    vec![serde_json::json!({"file": file, "status": status})]
                } else {
                    syms.iter()
                        .map(|(k, q, s, e)| {
                            serde_json::json!({"file": file, "status": status, "kind": k, "symbol": q, "start": s, "end": e})
                        })
                        .collect()
                }
            })
            .collect();
        return jout(&items, baseline);
    }
    if report.is_empty() {
        return Ok((format!("no code changes vs {gitref}\n"), baseline));
    }
    let mut out = String::new();
    for (file, status, syms) in &report {
        match *status {
            "modified" => out.push_str(&format!("{file}\n")),
            s => out.push_str(&format!("{file} ({s})\n")),
        }
        if syms.is_empty() && *status == "modified" {
            out.push_str("  (changes outside indexed symbols)\n");
        }
        for (k, q, s, e) in syms {
            out.push_str(&format!("  {k} {q} :{s}-{e}\n"));
        }
    }
    Ok((out, baseline))
}

/// Opt-in changed-code review. The legacy renderer above deliberately remains
/// independent: its output and defaults are a compatibility surface.
pub fn cmd_diff_impact(
    root: &Path,
    conn: &Connection,
    gitref: &str,
    budget: usize,
    json: bool,
) -> Result<(String, i64)> {
    // Resolve once: every old blob and hunk must describe the same revision.
    let revision = gitmap::run_git(
        root,
        &["rev-parse", "--verify", &format!("{gitref}^{{commit}}")],
    )?;
    let revision = revision.trim();
    let names = gitmap::run_git(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-renames",
            "--name-only",
            "-z",
            revision,
            "--",
        ],
    )?;
    let untracked = gitmap::run_git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let paths: std::collections::BTreeSet<_> = names
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|p| !p.is_empty() && lang::detect_lang(p).is_some())
        .collect();
    let mut changes = Vec::new();
    let mut bytes = 0;
    for path in paths {
        indexer::ensure_fresh(root, conn, path);
        let old = gitmap::run_git(root, &["show", &format!("{revision}:{path}")]).ok();
        let new = match std::fs::read_to_string(root.join(path)) {
            Ok(source) => Some(source),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        bytes += old.as_ref().map_or(0, String::len) + new.as_ref().map_or(0, String::len);
        let patch = gitmap::run_git(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--unified=0",
                revision,
                "--",
                path,
            ],
        )?;
        let (old_ranges, new_ranges) = impact_ranges(&patch);
        collect_changes(
            path,
            old.as_deref(),
            new.as_deref(),
            &old_ranges,
            &new_ranges,
            &mut changes,
        )?;
    }
    let report = assemble_impact(root, conn, changes)?;
    Ok((render_impact(&report, budget, json)?, db::est_tokens(bytes)))
}

#[derive(Debug, serde::Serialize)]
struct ChangedDefinition {
    file: String,
    symbol: Option<String>,
    status: &'static str,
    old_signature: Option<String>,
    new_signature: Option<String>,
    old_start: Option<usize>,
    new_start: Option<usize>,
    /// Needed for deleted definitions, which have no current graph node.
    #[serde(skip)]
    name: Option<String>,
}

type LineRanges = Vec<(i64, i64)>;

/// Unlike the default mapper, keep empty-side hunks empty. Anchoring a deletion
/// on the next definition would falsely claim that unrelated definition changed.
fn impact_ranges(patch: &str) -> (LineRanges, LineRanges) {
    let mut old = Vec::new();
    let mut new = Vec::new();
    for line in patch.lines().filter_map(|s| s.strip_prefix("@@ ")) {
        for (prefix, ranges) in [('-', &mut old), ('+', &mut new)] {
            if let Some(token) = line.split_whitespace().find_map(|s| s.strip_prefix(prefix)) {
                let (start, count) = token.split_once(',').unwrap_or((token, "1"));
                if let (Ok(start), Ok(count)) = (start.parse::<i64>(), count.parse::<i64>()) {
                    if count > 0 {
                        ranges.push((start, start + count - 1));
                    }
                }
            }
        }
    }
    (old, new)
}

fn collect_changes(
    path: &str,
    old: Option<&str>,
    new: Option<&str>,
    old_ranges: &[(i64, i64)],
    new_ranges: &[(i64, i64)],
    out: &mut Vec<ChangedDefinition>,
) -> Result<()> {
    let language = lang::detect_lang(path).expect("filtered language");
    let before = old
        .map(|s| lang::extract_symbols(language, s))
        .transpose()?
        .unwrap_or_default();
    let after = new
        .map(|s| lang::extract_symbols(language, s))
        .transpose()?
        .unwrap_or_default();
    let touched = |s: &lang::Sym, ranges: &[(i64, i64)], whole: bool| {
        whole || diffmap::overlaps(s.start_line as i64, s.end_line as i64, ranges)
    };
    let mut old_counts = HashMap::new();
    for symbol in &before {
        *old_counts
            .entry((symbol.qualified.as_str(), symbol.kind))
            .or_insert(0usize) += 1;
    }
    let mut new_matches = HashMap::new();
    for (index, symbol) in after.iter().enumerate() {
        let (count, _) = new_matches
            .entry((symbol.qualified.as_str(), symbol.kind))
            .or_insert((0usize, index));
        *count += 1;
    }
    let mut paired = HashSet::new();
    for previous in &before {
        // A qualified name is only paired when unique on BOTH sides. No
        // rename/move inference, and overloads remain explicit delete/add rows.
        // Maps only provide lookups; both output walks retain source order.
        let key = (previous.qualified.as_str(), previous.kind);
        let current = new_matches
            .get(&key)
            .filter(|(count, _)| *count == 1 && old_counts[&key] == 1)
            .map(|(_, index)| (*index, &after[*index]));
        if let Some((index, current)) = current {
            paired.insert(index);
            if touched(previous, old_ranges, new.is_none())
                || touched(current, new_ranges, old.is_none())
            {
                out.push(definition_change(path, Some(previous), Some(current)));
            }
        } else if touched(previous, old_ranges, new.is_none()) {
            out.push(definition_change(path, Some(previous), None));
        }
    }
    for (index, current) in after.iter().enumerate() {
        if !paired.contains(&index) && touched(current, new_ranges, old.is_none()) {
            out.push(definition_change(path, None, Some(current)));
        }
    }
    let outside =
        |source: Option<&str>, symbols: &[lang::Sym], ranges: &[(i64, i64)], whole: bool| {
            let covers: Vec<_> = symbols
                .iter()
                .map(|s| (s.start_line as i64, s.end_line as i64))
                .collect();
            source.is_some_and(|s| {
                let all = [(1, s.lines().count() as i64)];
                diffmap::has_uncovered(
                    1,
                    s.lines().count() as i64,
                    if whole { &all } else { ranges },
                    &covers,
                )
            })
        };
    if outside(old, &before, old_ranges, new.is_none())
        || outside(new, &after, new_ranges, old.is_none())
        || (before.is_empty() && after.is_empty())
    {
        out.push(ChangedDefinition {
            file: path.into(),
            symbol: None,
            status: "outside_symbols",
            old_signature: None,
            new_signature: None,
            old_start: None,
            new_start: None,
            name: None,
        });
    }
    Ok(())
}

fn definition_change(
    path: &str,
    old: Option<&lang::Sym>,
    new: Option<&lang::Sym>,
) -> ChangedDefinition {
    let symbol = new.or(old).expect("one side of a change");
    ChangedDefinition {
        file: path.into(),
        symbol: Some(symbol.qualified.clone()),
        status: match (old, new) {
            (None, _) => "added",
            (_, None) => "deleted",
            _ => "modified",
        },
        old_signature: old.map(|s| s.signature.clone()),
        new_signature: new.map(|s| s.signature.clone()),
        old_start: old.map(|s| s.start_line),
        new_start: new.map(|s| s.start_line),
        name: Some(symbol.name.clone()),
    }
}

const IMPACT_NOTE: &str = "Static name-based impact, not actual failures or execution coverage; no match does not mean untested. Relationships: depth 2, at most 32 changed targets and 100 callers/tests. Renames/moves are not inferred.";

#[derive(serde::Serialize)]
struct ImpactReport {
    note: &'static str,
    changes: Vec<ChangedDefinition>,
    callers: Vec<ImpactCaller>,
    tests: Vec<serde_json::Value>,
    omitted: bool,
}

#[derive(serde::Serialize)]
struct ImpactCaller {
    file: String,
    line: i64,
    symbol: String,
    selector: String,
    changed_symbols: Vec<String>,
    uncertain: bool,
}

fn assemble_impact(
    root: &Path,
    conn: &Connection,
    changes: Vec<ChangedDefinition>,
) -> Result<ImpactReport> {
    use crate::commands::test_selection::TestSelector;
    use std::collections::BTreeMap;
    if changes.is_empty() {
        return Ok(ImpactReport {
            note: IMPACT_NOTE,
            changes,
            callers: Vec::new(),
            tests: Vec::new(),
            omitted: false,
        });
    }
    let selection = TestSelector::new(root, conn, 2)?;
    let graph = selection.graph().expect("depth two constructs a graph");
    let mut callers = BTreeMap::new();
    let mut tests = BTreeMap::new();
    let mut omitted = false;
    let mut seen_targets = HashSet::new();
    for change in &changes {
        let Some(name) = &change.name else { continue };
        let target = format!(
            "{}:{}",
            change.file,
            change.symbol.as_deref().unwrap_or(name)
        );
        if !seen_targets.insert(target.clone()) {
            continue;
        }
        if seen_targets.len() > 32 {
            omitted = true;
            continue;
        }
        for (id, line) in graph.callers_of(name, &HashSet::new()) {
            let node = &graph.syms[id];
            let key = crate::commands::DefinitionIdentity::from(node);
            let selector = key.selector(conn)?;
            if !callers.contains_key(&key) && callers.len() >= 100 {
                omitted = true;
                continue;
            }
            let row = callers.entry(key).or_insert_with(|| ImpactCaller {
                file: node.file.clone(),
                line,
                symbol: node.qualified.clone(),
                selector,
                changed_symbols: Vec::new(),
                uncertain: true,
            });
            if !row.changed_symbols.contains(&target) {
                row.changed_symbols.push(target.clone());
            }
        }
        let evidence = if change.new_start.is_some() {
            selection
                .select(&target)
                .or_else(|_| selection.select_name(name))?
        } else {
            selection.select_name(name)?
        };
        for test in evidence.tests {
            let key = test.identity.clone();
            if !tests.contains_key(&key) && tests.len() >= 100 {
                omitted = true;
                continue;
            }
            let path = serde_json::to_value(&test.path)?;
            let ambiguous = test.ambiguous || test.path.iter().any(|edge| edge.ambiguous);
            let row = tests.entry(key).or_insert(serde_json::to_value(test)?);
            let object = row.as_object_mut().expect("serialized test evidence");
            let ambiguous = ambiguous
                || object
                    .get("ambiguous")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
            object.insert("ambiguous".into(), ambiguous.into());
            let targets = object
                .entry("changed_symbols")
                .or_insert_with(|| serde_json::json!([]));
            let targets = targets.as_array_mut().expect("target array");
            if !targets.iter().any(|value| value.as_str() == Some(&target)) {
                targets.push(target.clone().into());
                object
                    .entry("supporting_paths")
                    .or_insert_with(|| serde_json::json!([]))
                    .as_array_mut()
                    .expect("supporting paths array")
                    .push(serde_json::json!({"changed_symbol": target, "path": path}));
            }
        }
    }
    Ok(ImpactReport {
        note: IMPACT_NOTE,
        changes,
        callers: callers.into_values().collect(),
        tests: tests.into_values().collect(),
        omitted,
    })
}

/// Budget the actual serialized representation, not just source bodies. Rows
/// are atomic: signatures and evidence paths are never silently clipped.
fn render_impact(report: &ImpactReport, budget: usize, json: bool) -> Result<String> {
    let mut value = serde_json::to_value(report)?;
    loop {
        let output = if json {
            format!("{}\n", serde_json::to_string(&value)?)
        } else {
            impact_text(&value)
        };
        if db::est_tokens(output.len()) as usize <= budget {
            return Ok(output);
        }
        value["omitted"] = true.into();
        // Preserve changed definitions before secondary relationship evidence.
        let removed = ["tests", "callers", "changes"].into_iter().any(|key| {
            value[key]
                .as_array_mut()
                .is_some_and(|rows| rows.pop().is_some())
        });
        if !removed {
            anyhow::bail!(
                "--budget too small for diff impact metadata; increase --budget (whole-output estimate, not tokenizer-exact)"
            );
        }
    }
}

fn impact_text(value: &serde_json::Value) -> String {
    let mut output = format!("{}\n", IMPACT_NOTE);
    for row in value["changes"].as_array().into_iter().flatten() {
        let file = row["file"].as_str().unwrap_or("");
        let status = row["status"].as_str().unwrap_or("");
        if let Some(symbol) = row["symbol"].as_str() {
            output.push_str(&format!("{file}:{symbol} ({status})\n"));
            for (key, label) in [("old_signature", "old"), ("new_signature", "new")] {
                if let Some(signature) = row[key].as_str() {
                    output.push_str(&format!("  {label}: {signature}\n"));
                }
            }
        } else {
            output.push_str(&format!(
                "{file} (changes outside indexed symbols / module-level)\n"
            ));
        }
    }
    for row in value["callers"].as_array().into_iter().flatten() {
        output.push_str(&format!(
            "caller? {}:{} {} <- {}\n",
            row["file"].as_str().unwrap_or(""),
            row["line"],
            row["symbol"].as_str().unwrap_or(""),
            row["changed_symbols"]
        ));
    }
    for row in value["tests"].as_array().into_iter().flatten() {
        output.push_str(&format!("test reference (not coverage): {}\n", row));
    }
    if value["omitted"] == true {
        output.push_str("Evidence omitted (budget or relationship limits); increase --budget or inspect individual symbols.\n");
    }
    output
}

#[cfg(test)]
#[path = "diff_tests.rs"]
mod impact_tests;
