//! `diff`: changed symbols instead of changed lines.

use crate::commands::jout;
use crate::{db, diffmap, gitmap, indexer, lang};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

/// Changed SYMBOLS instead of changed lines: parses `git diff --unified=0`
/// against a ref, maps new-side line ranges onto the (fresh) symbol index and
/// reports the innermost symbols they touch. Untracked code files appear as
/// whole new files. Review context for a fraction of the raw-diff tokens.
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
                // drop a container only when ALL of its changed lines lie
                // inside nested hit symbols (else the container itself
                // changed and must stay)
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
