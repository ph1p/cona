//! Call-graph commands: callers/callees (cmd_calls) and path.

use super::{jout, locate_symbol};
use crate::{db, graph, indexer, lang, resolve};
use anyhow::{bail, Result};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Semantic tier for callees: an ambiguous call `name` at `call_line` inside
/// caller `cur`'s body. Build candidates from the ambiguous def indices, feed
/// their files as deps (cross-file), and ask the resolver to pick one. Returns
/// the single resolved def index, or `None` (fail-open → stay ambiguous).
fn resolve_callee(
    root: &Path,
    g: &graph::Graph,
    cur: usize,
    name: &str,
    call_line: i64,
    defs: &[usize],
) -> Option<usize> {
    let caller = &g.syms[cur];
    let lang = lang::detect_lang(&caller.file)?;
    if !resolve::lang_supported(lang) {
        return None;
    }
    let src = std::fs::read_to_string(root.join(&caller.file)).ok()?;
    let refs = [resolve::Ref {
        line: call_line as usize,
        name: name.to_string(),
    }];
    // candidate def sites, keyed by (name, file, start line)
    let candidates: Vec<resolve::Candidate> = defs
        .iter()
        .map(|&d| resolve::Candidate {
            name: g.syms[d].name.clone(),
            file: g.syms[d].file.clone(),
            line: g.syms[d].start,
        })
        .collect();
    // deps = distinct candidate files other than the caller's own
    let mut dep_paths: Vec<String> = candidates
        .iter()
        .map(|c| c.file.clone())
        .filter(|f| *f != caller.file)
        .collect();
    dep_paths.sort();
    dep_paths.dedup();
    let deps: Vec<resolve::DepFile> = dep_paths
        .into_iter()
        .filter_map(|p| {
            std::fs::read_to_string(root.join(&p))
                .ok()
                .map(|source| resolve::DepFile { path: p, source })
        })
        .collect();

    let (tf, tl) = resolve::disambiguate(lang, &caller.file, &src, &refs, &candidates, &deps)
        .into_iter()
        .next()
        .flatten()?;
    // map the resolved (file, line) back to exactly one def index
    let mut hit = defs
        .iter()
        .copied()
        .filter(|&d| g.syms[d].file == tf && g.syms[d].start == tl);
    let one = hit.next()?;
    if hit.next().is_some() {
        return None; // two defs at same site — shouldn't happen, be safe
    }
    Some(one)
}

/// Load every indexed file + its symbol rows and build the in-memory call
/// graph. Stale files are reindexed first (invariant 2: line numbers are
/// never used blindly).
fn build_graph(root: &Path, conn: &Connection) -> Result<(graph::Graph, HashMap<String, i64>)> {
    let mut stmt = conn.prepare("SELECT path, size FROM files ORDER BY path")?;
    let files: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .flatten()
        .collect();
    for (rel, _) in &files {
        indexer::ensure_fresh(root, conn, rel);
    }
    let mut sym_stmt = conn.prepare(
        "SELECT s.name, s.qualified, s.kind, s.start_line, s.end_line, s.signature
         FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.path = ?1 ORDER BY s.start_line",
    )?;
    let mut sizes: HashMap<String, i64> = HashMap::new();
    let mut input: Vec<(String, Option<&str>, String, Vec<graph::SymNode>)> = Vec::new();
    for (rel, size) in files {
        let Ok(src) = std::fs::read_to_string(root.join(&rel)) else {
            continue;
        };
        let syms: Vec<graph::SymNode> = sym_stmt
            .query_map([&rel], |r| {
                let sig: String = r.get(5)?;
                Ok(graph::SymNode {
                    name: r.get(0)?,
                    qualified: r.get(1)?,
                    kind: r.get(2)?,
                    file: rel.clone(),
                    start: r.get(3)?,
                    end: r.get(4)?,
                    params: lang::param_count(&sig),
                    recv: lang::first_param_is_receiver(&sig),
                })
            })?
            .flatten()
            .collect();
        sizes.insert(rel.clone(), size);
        input.push((rel.clone(), lang::detect_lang(&rel), src, syms));
    }
    Ok((graph::Graph::build(&input), sizes))
}

fn fmt_node(g: &graph::Graph, idx: usize) -> String {
    let s = &g.syms[idx];
    format!(
        "{} {}  {}:{}-{}",
        s.kind, s.qualified, s.file, s.start, s.end
    )
}

/// Shared implementation for callers (up=true) and callees (up=false):
/// depth-first render of the transitive tree, visited-deduped, capped.
pub fn cmd_calls(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    depth: usize,
    up: bool,
    json: bool,
) -> Result<(String, i64)> {
    let (_, _, _, q) = locate_symbol(conn, symbol)?;
    let (g, sizes) = build_graph(root, conn)?;
    let anchors = g.find(&q);
    let Some(&anchor) = anchors.first() else {
        bail!("symbol '{symbol}' not in call graph — reindex?");
    };
    const MAX_NODES: usize = 80;
    const MAX_CHILDREN: usize = 12;
    let mut visited: HashSet<usize> = HashSet::new();
    visited.insert(anchor);
    let mut printed = 0usize;
    let mut involved: HashSet<String> = HashSet::new();
    involved.insert(g.syms[anchor].file.clone());
    let mut out = format!("{}\n", fmt_node(&g, anchor));
    let arrow = if up { "←" } else { "→" };
    let mut json_edges: Vec<serde_json::Value> = Vec::new();

    // (node, remaining depth, indent)
    let mut stack: Vec<(usize, usize, usize)> = vec![(anchor, depth, 1)];
    let mut capped = false;
    while let Some((cur, d, ind)) = stack.pop() {
        if d == 0 {
            continue;
        }
        let children: Vec<(usize, Option<i64>, bool)> = if up {
            g.callers_of(&g.syms[cur].name, &visited)
                .into_iter()
                .map(|(i, ln)| (i, Some(ln), false))
                .collect()
        } else {
            g.callees_of(cur)
                .into_iter()
                .filter_map(|(name, defs, call_line)| {
                    let mut defs = defs;
                    // ambiguous callee → try the semantic tier at the call site
                    // inside `cur`'s body, narrowing to a single def.
                    if defs.len() > 1 {
                        if let Some(one) = resolve_callee(root, &g, cur, &name, call_line, &defs) {
                            defs = vec![one];
                        }
                    }
                    let amb = defs.len() > 1;
                    defs.into_iter()
                        .find(|d| !visited.contains(d))
                        .map(|d| (d, None, amb))
                })
                .collect()
        };
        let over_cap = children.len() > MAX_CHILDREN;
        for (child, line, amb) in children.into_iter().take(MAX_CHILDREN) {
            if printed >= MAX_NODES {
                capped = true;
                break;
            }
            if !visited.insert(child) {
                continue;
            }
            printed += 1;
            involved.insert(g.syms[child].file.clone());
            let at = line.map(|l| format!("  (:{l})")).unwrap_or_default();
            let mark = if amb { "  ·ambiguous" } else { "" };
            out.push_str(&format!(
                "{}{arrow} {}{at}{mark}\n",
                "  ".repeat(ind),
                fmt_node(&g, child)
            ));
            json_edges.push(serde_json::json!({
                "from": g.syms[cur].qualified, "to": g.syms[child].qualified,
                "file": g.syms[child].file, "line": line, "ambiguous": amb, "depth": ind,
            }));
            stack.push((child, d - 1, ind + 1));
        }
        if over_cap {
            out.push_str(&format!(
                "{}… more (cap {MAX_CHILDREN}/level)\n",
                "  ".repeat(ind)
            ));
        }
    }
    if capped {
        out.push_str(&format!("… stopped at {MAX_NODES} nodes\n"));
    }
    if printed == 0 {
        out.push_str(&format!(
            "  no {} found within the index\n",
            if up { "callers" } else { "resolvable callees" }
        ));
    }
    // same-qualified defs necessarily share the bare name → one check suffices
    if g.find(g.syms[anchor].name.as_str()).len() > 1 {
        out.push_str("note: several definitions share this name — edges are name-matched, not type-resolved\n");
    }
    let bytes: i64 = involved.iter().filter_map(|f| sizes.get(f)).sum();
    let baseline = db::est_tokens(bytes as usize);
    if json {
        let obj = serde_json::json!({
            "symbol": q, "direction": if up { "callers" } else { "callees" },
            "edges": json_edges,
        });
        return jout(&obj, baseline);
    }
    Ok((out, baseline))
}

pub fn cmd_path(
    root: &Path,
    conn: &Connection,
    from: &str,
    to: &str,
    max_depth: usize,
    json: bool,
) -> Result<(String, i64)> {
    let (g, sizes) = build_graph(root, conn)?;
    if g.find(from).is_empty() {
        bail!("'{from}' not found — try `cona find {from}`");
    }
    if g.find(to).is_empty() {
        bail!("'{to}' not found — try `cona find {to}`");
    }
    let chain = g.path(from, to, max_depth);
    let Some(chain) = chain else {
        let out =
            format!("no call path {from} → {to} within {max_depth} hops (name-based search)\n");
        return Ok((out, 0));
    };
    let bytes: i64 = chain
        .iter()
        .map(|i| g.syms[*i].file.as_str())
        .collect::<HashSet<_>>()
        .iter()
        .filter_map(|f| sizes.get(*f))
        .sum();
    let baseline = db::est_tokens(bytes as usize);
    if json {
        let items: Vec<_> = chain
            .iter()
            .map(|i| {
                let s = &g.syms[*i];
                serde_json::json!({"kind": s.kind, "symbol": s.qualified, "file": s.file, "start": s.start})
            })
            .collect();
        return jout(&items, baseline);
    }
    let mut out = String::new();
    for (i, idx) in chain.iter().enumerate() {
        let pad = if i == 0 {
            String::new()
        } else {
            format!("{}→ ", "  ".repeat(i))
        };
        out.push_str(&format!("{pad}{}\n", fmt_node(&g, *idx)));
    }
    Ok((out, baseline))
}
