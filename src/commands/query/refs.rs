//! `refs`: semantic usage sites of a name.

use crate::commands::{jout, scan_ref_sites, LIMIT_TRAILER};
use crate::db;
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

pub fn cmd_refs(
    root: &Path,
    conn: &Connection,
    name: &str,
    limit: usize,
    path_filter: Option<&str>,
    json: bool,
) -> Result<(String, i64)> {
    let mut hits: Vec<(String, usize, String)> = Vec::new();
    // Honest baseline: per hit file, a grep pass + a Read window around each
    // ref line (not the whole file) — what the same lookup costs without cona.
    let mut baseline: i64 = 0;
    let mut truncated = false;
    let mut cur_file = String::new();
    let mut cur_lens: Vec<usize> = Vec::new();
    // line-start byte offsets — O(1) hit-text access instead of re-scanning
    // the file prefix per hit (offsets don't borrow fsrc, so they can live
    // across visit calls, unlike a Vec<&str> of the lines)
    let mut cur_offsets: Vec<usize> = Vec::new();
    let mut cur_lns: Vec<usize> = Vec::new();
    scan_ref_sites(
        root,
        conn,
        name,
        None,
        path_filter,
        |rel, ln, _, _, fsrc| {
            if rel != cur_file {
                if !cur_lns.is_empty() {
                    baseline += db::baseline_tokens(&cur_lens, &cur_lns);
                }
                cur_file = rel.to_string();
                cur_lens = fsrc.lines().map(|l| l.len()).collect();
                cur_offsets = std::iter::once(0)
                    .chain(fsrc.match_indices('\n').map(|(i, _)| i + 1))
                    .collect();
                cur_lns.clear();
            }
            let ln = ln as usize;
            cur_lns.push(ln);
            let text = cur_offsets
                .get(ln - 1)
                .and_then(|&st| fsrc[st..].lines().next())
                .unwrap_or("")
                .trim();
            hits.push((rel.to_string(), ln, text.to_string()));
            if hits.len() >= limit {
                truncated = true;
                return false;
            }
            true
        },
    )?;
    if !cur_lns.is_empty() {
        baseline += db::baseline_tokens(&cur_lens, &cur_lns);
    }
    if json {
        let items: Vec<_> = hits
            .iter()
            .map(|(f, l, t)| serde_json::json!({"file": f, "line": l, "text": t}))
            .collect();
        return jout(&items, baseline);
    }
    let mut out = String::new();
    let mut current = "";
    for (f, l, t) in &hits {
        if f.as_str() != current {
            out.push_str(&format!("{f}\n"));
            current = f;
        }
        out.push_str(&format!("  {l}: {t}\n"));
    }
    if truncated {
        out.push_str(LIMIT_TRAILER);
    }
    if hits.is_empty() {
        // An empty result is a dead end unless it names the next move: in-scope
        // misses are usually a too-narrow --path, global misses a wrong name.
        out.push_str(&match path_filter {
            Some(pf) => format!(
                "no references to '{name}' under '{pf}' — try without --path, or `cona find {name}`\n"
            ),
            None => format!("no references to '{name}' — try `cona find {name}` or `cona grep {name}`\n"),
        });
    }
    Ok((out, baseline))
}
