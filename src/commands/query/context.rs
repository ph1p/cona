//! `context`: one budgeted pack of source + callees + call sites.

use crate::commands::{jout, locate_fresh, render_symbol_body, scan_ref_sites, BudgetOut};
use crate::{db, entries, graph, lang, resolve};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

/// One budgeted context pack for a symbol: its full source, the signatures of
/// indexed symbols its body references (callees), and the sites that call it
/// (callers, deduped per enclosing symbol). Replaces the show → refs → N×show
/// round-trip chain with a single command.
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

    // callees: identifiers in the body that resolve to indexed symbols
    // (semantic with textual fail-open fallback — policy lives in lang.rs).
    // Ordered-unique by name, carrying the call-site arg count (arity signal)
    // and the line of the first occurrence (for the semantic-resolve tier).
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

    // Semantic-resolve tier (fail-open, opt-in on the helper binary): for names
    // still ambiguous after scope/file/dir/arity, ask the out-of-process
    // stack-graphs helper. The candidate definitions for an ambiguous name may
    // live in OTHER files, so we feed those files to the helper as deps and let
    // it resolve cross-file. When it points at exactly ONE of the ambiguous
    // rows (matched by (file, line)), we collapse the rest and clear the mark.
    if !ambiguous_refs.is_empty() {
        // candidate rows = every ambiguous callee, keyed by (bare name, file,
        // line); deps = the distinct non-primary files they live in, so a ref
        // can resolve into them cross-file.
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
            // unmark the keeper FIRST (index still valid), then drop the other
            // ambiguous rows for this name. After this the keeper is
            // c.6 == false, so the retain predicate spares it.
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
            // Well-tested symbols have far more test callers than real ones, and
            // the cap below is first-come — unfiltered, tests crowd out the
            // production call sites that actually answer "who uses this?".
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
