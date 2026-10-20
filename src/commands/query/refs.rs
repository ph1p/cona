//! `refs`: semantic usage sites of a name.

use crate::commands::{jout, scan_ref_sites, LIMIT_TRAILER};
use crate::db;
use crate::entries;
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

pub fn cmd_refs(
    root: &Path,
    conn: &Connection,
    name: &str,
    limit: usize,
    path_filter: Option<&str>,
    tests: bool,
    json: bool,
) -> Result<(String, i64)> {
    // Test sites are kept apart: on a hot symbol they outnumber production
    // sites ~5:1 and eat the limit. They are folded into one count line unless
    // asked for (`tests`), the caller scoped with --path, or they are all there is.
    let mut prod: Vec<(String, usize, String)> = Vec::new();
    let mut test: Vec<(String, usize, String)> = Vec::new();
    // Honest baseline: per hit file, a grep pass + a Read window per ref line.
    // Kept per side so folded test sites claim nothing.
    let (mut base_prod, mut base_test): (i64, i64) = (0, 0);
    let mut truncated = false;
    let mut cur_file = String::new();
    let mut cur_test = false;
    let mut cur_lens: Vec<usize> = Vec::new();
    // Line-start byte offsets for O(1) hit-text access; unlike a Vec<&str>
    // they don't borrow fsrc, so they outlive each visit call.
    let mut cur_offsets: Vec<usize> = Vec::new();
    let mut cur_lns: Vec<usize> = Vec::new();
    let fold = !tests && path_filter.is_none();
    let mut flush = |lens: &[usize], lns: &[usize], is_test: bool| {
        if !lns.is_empty() {
            let b = db::baseline_tokens(lens, lns);
            if is_test {
                base_test += b
            } else {
                base_prod += b
            }
        }
    };
    scan_ref_sites(
        root,
        conn,
        name,
        None,
        path_filter,
        |rel, ln, _, _, fsrc| {
            if rel != cur_file {
                flush(&cur_lens, &cur_lns, cur_test);
                cur_file = rel.to_string();
                cur_test = fold && entries::is_test_path(rel);
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
            let side = if cur_test { &mut test } else { &mut prod };
            side.push((rel.to_string(), ln, text.to_string()));
            // Test sites never truncate while folded — only the shown side counts.
            if prod.len() >= limit || (!fold && test.len() >= limit) {
                truncated = true;
                return false;
            }
            true
        },
    )?;
    flush(&cur_lens, &cur_lns, cur_test);
    // Nothing but test sites: they ARE the answer, show them (up to limit).
    let folded = (!prod.is_empty() && !test.is_empty()).then(|| (test.len(), hits_files(&test)));
    let (hits, baseline) = if prod.is_empty() {
        test.truncate(limit);
        truncated |= test.len() >= limit;
        (test, base_test)
    } else {
        (prod, base_prod)
    };
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
    if let Some((n, files)) = folded {
        let at_least = if truncated { "≥" } else { "" };
        out.push_str(&format!(
            "+ {at_least}{n} test site{} in {files} file{} folded (--tests lists them)\n",
            if n == 1 { "" } else { "s" },
            if files == 1 { "" } else { "s" }
        ));
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

/// Distinct files among sorted-by-file hits.
fn hits_files(hits: &[(String, usize, String)]) -> usize {
    hits.windows(2).filter(|w| w[0].0 != w[1].0).count() + usize::from(!hits.is_empty())
}
