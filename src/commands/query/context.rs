//! `context`: one budgeted pack of source + callees + call sites.

use crate::commands::{jout, locate_fresh, render_symbol_body, scan_ref_sites, BudgetOut};
use crate::{db, entries, graph, lang, resolve};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

/// One budgeted pack for a symbol: full source, signatures of the indexed
/// symbols it calls, and its call sites (deduped per enclosing symbol).
/// Replaces the show → refs → N×show round-trip chain.
pub fn cmd_context(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    budget: i64,
    no_tests: bool,
    json: bool,
) -> Result<(String, i64)> {
    let (path, s, e, q) = locate_fresh(root, conn, symbol, None)?;
    let src = std::fs::read_to_string(root.join(&path))?;
    let lines: Vec<&str> = src.lines().collect();
    let body_end = (e as usize).min(lines.len());
    let body_start = (s as usize).saturating_sub(1).min(body_end);
    let body = lines[body_start..body_end].join("\n");
    let name = db::name_tail(&q).to_string();

    // callees: body identifiers that resolve to indexed symbols (fail-open
    // policy lives in lang.rs). Unique by name, with the call-site arg count
    // (arity signal) and first-occurrence line (for the semantic-resolve tier).
    let mut idents: Vec<(String, Option<usize>, usize)> = Vec::new();
    {
        let mut seen = std::collections::HashSet::new();
        for (n, ln, is_call, argc) in
            lang::ident_occurrences_failopen(lang::detect_lang(&path), &src)
        {
            if !is_call || ln < s as usize || ln > e as usize || n.len() < 2 {
                continue;
            }
            if seen.insert(n.clone()) {
                idents.push((n, argc, ln));
            }
        }
    }
    // kind, qualified, path, start, end, sig, ambiguous
    type Callee = (String, String, String, i64, i64, String, bool);
    let mut by_name = conn.prepare(
        "SELECT s.kind, s.qualified, f.path, s.start_line, s.end_line, s.signature
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.name = ?1 ORDER BY f.path, s.start_line LIMIT 6",
    )?;
    let my_scope = graph::scope_parent(&q).map(str::to_string);
    let mut callees: Vec<Callee> = Vec::new();
    let mut callees_capped = false;
    // names still ambiguous after the heuristics, with their call-site line —
    // handed to the semantic-resolve tier in one batch below
    let mut ambiguous_refs: Vec<resolve::Ref> = Vec::new();
    for (ident, argc, ln) in idents {
        if ident == name {
            continue;
        }
        let rows: Vec<Callee> = by_name
            .query_map([&ident], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    false,
                ))
            })?
            .flatten()
            // a call site never points back into the symbol itself; module
            // declarations carry no useful signature
            .filter(|(k, cq, cp, ..)| k != "mod" && !(*cp == path && *cq == q))
            .collect();
        let mut rows = graph::narrow_by_scope(
            my_scope.as_deref(),
            &path,
            argc,
            rows,
            |(_, cq, cp, _, _, sig, _)| graph::Candidate {
                scope: graph::scope_parent(cq).map(String::from),
                file: cp.clone(),
                params: lang::param_count(sig),
                is_method: lang::first_param_is_receiver(sig),
            },
        );
        // several definitions still share this name → mark, never silently pick
        if rows.len() > 1 {
            rows.truncate(2);
            for r in &mut rows {
                r.6 = true;
            }
            ambiguous_refs.push(resolve::Ref {
                line: ln,
                name: ident.clone(),
            });
        }
        callees.extend(rows);
        if callees.len() >= 24 {
            callees_capped = true;
            break;
        }
    }

    // Semantic-resolve tier (fail-open, opt-in on the helper binary): names
    // still ambiguous after scope/file/dir/arity go to the stack-graphs
    // helper, with the candidates' OTHER files fed as deps so it can resolve
    // cross-file. If it points at exactly ONE ambiguous row (by file, line),
    // the rest collapse and the mark clears.
    if !ambiguous_refs.is_empty() {
        // candidates keyed by (bare name, file, line); deps = their distinct
        // non-primary files
        let candidates: Vec<resolve::Candidate> = callees
            .iter()
            .filter(|c| c.6)
            .map(|c| resolve::Candidate {
                name: db::name_tail(&c.1).to_string(),
                file: c.2.clone(),
                line: c.3,
            })
            .collect();
        let mut dep_files: Vec<String> = candidates
            .iter()
            .filter(|c| c.file != path)
            .map(|c| c.file.clone())
            .collect();
        dep_files.sort();
        dep_files.dedup();
        let deps: Vec<resolve::DepFile> = dep_files
            .into_iter()
            .filter_map(|p| {
                std::fs::read_to_string(root.join(&p))
                    .ok()
                    .map(|source| resolve::DepFile { path: p, source })
            })
            .collect();

        let resolved = resolve::disambiguate(
            lang::detect_lang(&path).unwrap_or(""),
            &path,
            &src,
            &ambiguous_refs,
            &candidates,
            &deps,
        );
        for (r, target) in ambiguous_refs.iter().zip(resolved) {
            let Some((target_file, target_line)) = target else {
                continue; // no clean resolution → leave ambiguous
            };
            let rname = &r.name;
            let keep: Vec<usize> = callees
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.6 && db::name_tail(&c.1) == *rname && c.2 == target_file && c.3 == target_line
                })
                .map(|(i, _)| i)
                .collect();
            if keep.len() != 1 {
                continue;
            }
            // unmark the keeper FIRST (index still valid) so the retain
            // below spares it while dropping the other ambiguous rows
            callees[keep[0]].6 = false;
            callees.retain(|c| !(c.6 && db::name_tail(&c.1) == *rname));
        }
    }

    // callers: identifier occurrences of `name` outside the symbol's own
    // range, deduped by enclosing symbol
    type Caller = (String, String, String, i64); // kind, enclosing qualified, path, line
    let mut callers: Vec<Caller> = Vec::new();
    let mut callers_capped = false;
    let mut tests_hidden = 0usize;
    let mut seen_callers: HashSet<(String, String)> = HashSet::new();
    scan_ref_sites(
        root,
        conn,
        &name,
        Some((&path, &src)),
        None,
        |rel, ln1, encl, kind, _| {
            if rel == path && ln1 >= s && ln1 <= e {
                return true; // inside the symbol itself
            }
            if !seen_callers.insert((rel.to_string(), encl.to_string())) {
                return true; // one entry per enclosing symbol and file
            }
            // the cap below is first-come, so on well-tested symbols test
            // callers would crowd out the production ones
            if no_tests && (entries::is_test_symbol(encl) || entries::is_test_path(rel)) {
                tests_hidden += 1;
                return true;
            }
            callers.push((kind.to_string(), encl.to_string(), rel.to_string(), ln1));
            if callers.len() >= 20 {
                callers_capped = true;
                return false;
            }
            true
        },
    )?;

    // baseline: the files an agent would have opened for the same picture
    let mut involved: HashSet<&str> = HashSet::new();
    involved.insert(&path);
    involved.extend(callees.iter().map(|(_, _, p, ..)| p.as_str()));
    involved.extend(callers.iter().map(|(_, _, p, _)| p.as_str()));
    let mut size_stmt = conn.prepare("SELECT size FROM files WHERE path = ?1")?;
    let mut bytes: i64 = 0;
    for p in involved {
        bytes += size_stmt
            .query_row([p], |r| r.get::<_, i64>(0))
            .unwrap_or(0);
    }
    let baseline = db::est_tokens(bytes as usize);

    let notes = db::notes_for(conn, &q).unwrap_or_default();
    if json {
        let obj = serde_json::json!({
            "symbol": {"file": path, "symbol": q, "start": s, "end": e, "code": body},
            "notes": notes.iter().map(|(_, n, _)| n.clone()).collect::<Vec<_>>(),
            "calls": callees.iter().map(|(k, cq, p, cs, ce, sig, amb)| {
                serde_json::json!({"kind": k, "symbol": cq, "file": p, "start": cs, "end": ce, "sig": sig, "ambiguous": amb})
            }).collect::<Vec<_>>(),
            "calls_capped": callees_capped,
            "called_by": callers.iter().map(|(k, cq, p, l)| {
                serde_json::json!({"kind": k, "symbol": cq, "file": p, "line": l})
            }).collect::<Vec<_>>(),
            "called_by_capped": callers_capped,
            "test_callers_hidden": tests_hidden,
        });
        return jout(&obj, baseline);
    }

    // assemble within budget: source always in full (context without the
    // body is useless) — --budget bounds only the calls/called-by sections
    let mut seed = String::new();
    render_symbol_body(&mut seed, &q, &path, s, e, &lines, &notes);
    let mut bo = BudgetOut::new(seed, budget);
    for (title, capped, entries) in [
        (
            "calls",
            callees_capped,
            callees
                .iter()
                .map(|(k, cq, p, cs, ce, sig, amb)| {
                    let mark = if *amb { "  ·ambiguous" } else { "" };
                    format!("  {k} {cq}  {p}:{cs}-{ce}  {sig}{mark}\n")
                })
                .collect::<Vec<_>>(),
        ),
        (
            "called by",
            callers_capped,
            callers
                .iter()
                .map(|(k, cq, p, l)| {
                    if cq.is_empty() {
                        format!("  {p}:{l}\n")
                    } else {
                        format!("  {k} {cq}  {p}:{l}\n")
                    }
                })
                .collect::<Vec<_>>(),
        ),
    ] {
        if entries.is_empty() {
            continue;
        }
        bo.push_always(&format!(
            "── {title} ({}{}) ──\n",
            entries.len(),
            if capped { "+" } else { "" }
        ));
        for entry in entries {
            if !bo.try_push(&entry) {
                break;
            }
        }
        if capped {
            bo.push_always("  … more (cap hit — use `cona refs` for the full list)\n");
        }
    }
    // never let a filter read as an absence — say what was withheld
    if tests_hidden > 0 {
        bo.push_always(&format!(
            "  ({tests_hidden} test caller{} hidden — drop --no-tests to include)\n",
            if tests_hidden == 1 { "" } else { "s" }
        ));
    }
    let out = bo.finish("… truncated (raise --budget)\n");
    Ok((out, baseline))
}

/// Editing context is opt-in: unlike `cmd_context`, the complete serialized
/// answer is budgeted (UTF-8 bytes / 4, an estimate, not a tokenizer).
/// Code blocks are atomic. A budget below the smallest actionable omission
/// returns an error rather than silently exceeding the requested budget.
pub fn cmd_context_edit(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    budget: i64,
    no_tests: bool,
    json: bool,
) -> Result<(String, i64)> {
    // Refresh the complete candidate set before narrowing names, including
    // when test selection is disabled. Bodies alone cannot establish identity.
    crate::commands::refresh_candidate_files(root, conn)?;
    let (path, start, end, qualified) = locate_fresh(root, conn, symbol, None)?;
    let source = std::fs::read_to_string(root.join(&path))?;
    let mut target = EditItem {
        role: "target".into(),
        file: path.clone(),
        symbol: qualified.clone(),
        start,
        end,
        ambiguous: false,
        signature: None,
        code: None,
        excerpt: None,
    };
    let selector = format!("{path}:{qualified}");
    let mut pack = EditPack {
        show: selector.clone(),
        target: target.clone(),
        related: Vec::new(),
        tests: Vec::new(),
        notes: Vec::new(),
        body_omitted: true,
        omitted: true,
        evidence: if no_tests {
            "Name-based selection, not execution coverage; tests excluded; related context is bounded."
        } else {
            "Name-based selection, not execution coverage; no match does not mean untested. Related context is bounded."
        },
    };
    let minimal = pack.render(json)?;
    anyhow::ensure!(
        db::est_tokens(minimal.len()) <= budget,
        "target body omitted; use `cona show {selector}`; editing context needs at least --budget {} (byte/4 estimate)",
        db::est_tokens(minimal.len())
    );

    let body = edit_body(&source, start, end);
    target.code = body;
    pack.target = target;
    pack.body_omitted = pack.target.code.is_none();
    if db::est_tokens(pack.render(json)?.len()) > budget {
        pack.target.code = None;
        pack.body_omitted = true;
    }
    // Notes and every related block pay for their headings/JSON envelope too.
    for (_, note, _) in db::notes_for(conn, &qualified).unwrap_or_default() {
        pack.notes.push(note);
        if db::est_tokens(pack.render(json)?.len()) > budget {
            pack.notes.pop();
        }
    }

    let mut related = edit_dependencies(root, conn, &path, &qualified, start, end, &source)?;
    let name = db::name_tail(&qualified);
    let mut seen_callers = HashSet::new();
    scan_ref_sites(
        root,
        conn,
        name,
        Some((&path, &source)),
        None,
        |file, line, enclosing, _, src| {
            if (file == path && line >= start && line <= end)
                || enclosing.is_empty()
                || entries::is_test_symbol(enclosing)
                || entries::is_test_path(file)
                || !seen_callers.insert((file.to_string(), enclosing.to_string()))
            {
                return true;
            }
            // A representative reference line is explicitly an excerpt, never
            // presented as the caller's complete body or a proven call edge.
            related.push(EditItem {
                role: "caller reference excerpt".into(),
                file: file.into(),
                symbol: enclosing.into(),
                start: line,
                end: line,
                ambiguous: true,
                signature: None,
                code: None,
                excerpt: src
                    .lines()
                    .nth(line.saturating_sub(1) as usize)
                    .map(str::to_string),
            });
            seen_callers.len() < 8
        },
    )?;

    let mut seen = HashSet::from([(path.clone(), qualified)]);
    let mut files = HashSet::from([path]);
    for item in related {
        if !seen.insert((item.file.clone(), item.symbol.clone())) {
            continue;
        }
        pack.related.push(item);
        if db::est_tokens(pack.render(json)?.len()) > budget {
            pack.related.pop();
        } else if let Some(item) = pack.related.last() {
            files.insert(item.file.clone());
        }
    }
    if !no_tests {
        let selection = crate::commands::test_selection::select_tests(root, conn, symbol, 2)?;
        for evidence in selection.tests {
            files.insert(evidence.file.clone());
            pack.tests.push(evidence);
            if db::est_tokens(pack.render(json)?.len()) > budget {
                pack.tests.pop();
            }
        }
    }
    let mut bytes = 0usize;
    for file in files {
        bytes += std::fs::metadata(root.join(file))
            .map(|m| m.len() as usize)
            .unwrap_or(0);
    }
    Ok((pack.render(json)?, db::est_tokens(bytes)))
}

#[derive(Clone, serde::Serialize)]
struct EditItem {
    role: String,
    file: String,
    symbol: String,
    start: i64,
    end: i64,
    ambiguous: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    excerpt: Option<String>,
}

#[derive(serde::Serialize)]
struct EditPack {
    show: String,
    target: EditItem,
    related: Vec<EditItem>,
    tests: Vec<crate::commands::test_selection::TestEvidence>,
    notes: Vec<String>,
    body_omitted: bool,
    omitted: bool,
    evidence: &'static str,
}

impl EditPack {
    fn render(&self, json: bool) -> Result<String> {
        if json {
            return Ok(format!("{}\n", serde_json::to_string(self)?));
        }
        let mut out = format!("Editing context (byte/4 estimate)\n{}\n", self.evidence);
        if self.body_omitted {
            out.push_str(&format!("Target body omitted; cona show {}\n", self.show));
        }
        for item in std::iter::once(&self.target).chain(&self.related) {
            out.push_str(&format!(
                "── {}: {} {}:{}-{}{} ──\n",
                item.role,
                item.symbol,
                item.file,
                item.start,
                item.end,
                if item.ambiguous { " ·ambiguous" } else { "" }
            ));
            if let Some(signature) = &item.signature {
                out.push_str(signature);
                out.push('\n');
            }
            if let Some(code) = &item.code {
                out.push_str(code);
                out.push('\n');
            }
            if let Some(excerpt) = &item.excerpt {
                out.push_str(&format!("Reference excerpt: {excerpt}\n"));
            }
        }
        for note in &self.notes {
            out.push_str(&format!("Note: {note}\n"));
        }
        for test in &self.tests {
            out.push_str(&format!(
                "── test evidence: {} {}:{} (depth {}){} ──\n",
                test.enclosing,
                test.file,
                test.line,
                test.depth,
                if test.ambiguous { " ·ambiguous" } else { "" }
            ));
            for edge in &test.path {
                out.push_str(&format!(
                    "  {} → {} {}:{}{}\n",
                    edge.from,
                    edge.to,
                    edge.file,
                    edge.line,
                    if edge.ambiguous { " ·ambiguous" } else { "" }
                ));
            }
        }
        if self.omitted {
            out.push_str("Related context may be omitted; raise --budget or use refs/tests.\n");
        }
        Ok(out)
    }
}

fn edit_body(source: &str, start: i64, end: i64) -> Option<String> {
    let lines: Vec<_> = source.lines().collect();
    if start < 1 || end < start || end as usize > lines.len() {
        return None;
    }
    Some(lines[start as usize - 1..end as usize].join("\n"))
}

fn edit_dependencies(
    root: &Path,
    conn: &Connection,
    path: &str,
    qualified: &str,
    start: i64,
    end: i64,
    source: &str,
) -> Result<Vec<EditItem>> {
    let mut names = HashSet::new();
    let mut result = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT s.kind, s.qualified, f.path, s.start_line, s.end_line, s.signature
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.name = ?1 ORDER BY f.path, s.start_line LIMIT 7",
    )?;
    for (name, line, call, argc) in
        lang::ident_occurrences_failopen(lang::detect_lang(path), source)
    {
        if line < start as usize || line > end as usize || !names.insert((name.clone(), call)) {
            continue;
        }
        if names.len() > 96 {
            break;
        }
        let rows = stmt
            .query_map([&name], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let capped = rows.len() > 6;
        let rows = graph::narrow_by_scope(
            graph::scope_parent(qualified),
            path,
            if call { argc } else { None },
            rows,
            |(_, q, p, _, _, sig)| graph::Candidate {
                scope: graph::scope_parent(q).map(str::to_string),
                file: p.clone(),
                params: lang::param_count(sig),
                is_method: lang::first_param_is_receiver(sig),
            },
        );
        let ambiguous = capped || rows.len() > 1;
        for (kind, q, file, _, _, _) in rows.into_iter().take(2) {
            if (file == path && q == qualified)
                || !(call
                    || lang::TYPE_KINDS.contains(&kind.as_str())
                    || matches!(kind.as_str(), "const" | "constant" | "static"))
            {
                continue;
            }
            // Never trust indexed ranges for a dependency body. Missing or
            // ambiguous definitions fail open: retain the rest of the pack.
            let Ok((file, s, e, q)) = locate_fresh(root, conn, &format!("{file}:{q}"), Some(&kind))
            else {
                continue;
            };
            if file == path && s <= end && e >= start {
                continue; // the target already contains this nested definition
            }
            let code = if e - s < 12 {
                std::fs::read_to_string(root.join(&file))
                    .ok()
                    .and_then(|src| edit_body(&src, s, e))
            } else {
                None
            };
            let signature = if code.is_none() {
                conn.query_row(
                    "SELECT s.signature FROM symbols s JOIN files f ON f.id=s.file_id
                     WHERE f.path=?1 AND s.qualified=?2 AND s.start_line=?3",
                    rusqlite::params![file, q, s],
                    |r| r.get(0),
                )
                .ok()
            } else {
                None
            };
            result.push(EditItem {
                role: if call { "helper" } else { "type/constant" }.into(),
                file,
                symbol: q,
                start: s,
                end: e,
                ambiguous,
                signature,
                code,
                excerpt: None,
            });
            if result.len() >= 16 {
                return Ok(result);
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod edit_tests;
