//! `outline`: every symbol of one file with line ranges.

use crate::commands::{clip_str, jout};
use crate::{db, indexer};
use anyhow::{bail, Result};
use rusqlite::Connection;
use std::path::Path;

/// A file with no symbols still has a shape worth one call: a re-export
/// barrel, a config object, a route table. Small → the file itself (cheaper
/// than the error + the Read it forces); larger → its import/export lines.
/// `None` when neither applies and the caller's error is the honest answer.
fn symbolless(root: &Path, conn: &Connection, file: &str) -> Option<(String, i64)> {
    const WHOLE_MAX_LINES: usize = 60;
    let named = crate::commands::files_named(conn, file);
    let rel = match named.as_slice() {
        [one] => one.as_str(),
        _ => file.trim_start_matches("./"),
    };
    let src = std::fs::read_to_string(root.join(rel)).ok()?;
    let lines: Vec<&str> = src.lines().collect();
    let baseline = db::est_tokens(src.len());
    if lines.len() <= WHOLE_MAX_LINES {
        let mut out = format!("{rel}  (no symbols — whole file, {} lines)\n", lines.len());
        crate::commands::push_numbered_lines(&mut out, &lines, 0, lines.len());
        return Some((out, baseline));
    }
    let edges: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_start();
            t.starts_with("export ") || t.starts_with("module.exports") || t.starts_with("pub use ")
        })
        .map(|(i, l)| (i + 1, l.trim()))
        .take(WHOLE_MAX_LINES)
        .collect();
    if edges.is_empty() {
        return None;
    }
    let mut out = format!(
        "{rel}  (no symbols — export lines of {}; `cona show {rel}:A-B` for a range)\n",
        lines.len()
    );
    for (n, l) in edges {
        out.push_str(&format!("  {n}: {}\n", clip_str(l, 160)));
    }
    Some((out, baseline))
}

/// One outline row: `(path, kind, qualified, start, end, signature, file_size)`.
type OutlineRow = (String, String, String, i64, i64, String, i64);

pub fn cmd_outline(
    root: &Path,
    conn: &Connection,
    file: &str,
    show_sig: bool,
    json: bool,
) -> Result<(String, i64)> {
    let mut stmt = conn.prepare(
        "SELECT f.path, s.kind, s.qualified, s.start_line, s.end_line, s.signature, f.size
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE f.path = ?1 OR f.path LIKE ?2 ESCAPE '\\'
         ORDER BY f.path, s.start_line",
    )?;
    // reused after a refresh replaces the rows below
    let like = crate::commands::suffix_like(file);
    let mut fetch = || -> Result<Vec<OutlineRow>> {
        Ok(stmt
            .query_map(rusqlite::params![file, like], |r| {
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
            .collect())
    };
    let mut rows = fetch()?;
    if rows.is_empty() {
        // A file indexed with zero symbols may just be stale (edited, or an
        // extractor upgrade marked it) — the refresh below only covers files
        // that already had rows, so refresh the symbol-less matches first.
        let named = crate::commands::files_named(conn, file);
        let refreshed = named
            .iter()
            .filter(|f| indexer::ensure_fresh(root, conn, f))
            .count();
        if refreshed > 0 {
            rows = fetch()?;
        }
    }
    if rows.is_empty() {
        // A directory: point at the command that covers directories. The
        // index probe also catches an indexed dir since removed from disk.
        let is_dir =
            root.join(file).is_dir() || crate::commands::indexed_prefix(conn, &format!("{file}/"));
        if is_dir {
            bail!("'{file}' is a directory — try `cona tree --path {file}`");
        }
        // Distinguish the remaining causes — each has a different next step,
        // and a hook-redirected agent must not be stranded by a dead end.
        let indexed = !crate::commands::files_named(conn, file).is_empty();
        // created since the last walk: index it now rather than send the
        // agent to `cona index` — unless the walk excludes it for good
        match crate::commands::index_if_unseen(root, conn, file) {
            Ok(true) => rows = fetch()?,
            Ok(false) => {}
            Err(why) => bail!("'{file}' is not indexed ({why}) — read it directly"),
        }
        if rows.is_empty() {
            if indexed || root.join(file).is_file() {
                if let Some(answer) = symbolless(root, conn, file).filter(|_| !json) {
                    return Ok(answer);
                }
                bail!(
                    "'{file}' is indexed but has no extractable symbols — read it directly, \
                     or search it with `cona grep <pattern> --path {file}`"
                );
            }
            let near = crate::commands::did_you_mean_file(conn, file);
            if !near.is_empty() {
                bail!("no file '{file}' in the index{near}");
            }
            bail!("no file '{file}' in the index — `cona find <name>` locates symbols by name");
        }
    }
    // Baseline: sum each matched file's size once (rows are path-ordered).
    let mut bytes: i64 = 0;
    let mut current = "";
    for (p, .., size) in &rows {
        if p.as_str() != current {
            current = p;
            bytes += size;
        }
    }
    let baseline = db::est_tokens(bytes as usize);
    // Line ranges come from the index, so refresh the matched files first
    // (invariant 2) — stale ranges send the next `show` to the wrong lines.
    // Read-only mode can't refresh, so it flags the stale files instead.
    let refreshed = indexer::refresh_files(root, conn, rows.iter().map(|(p, ..)| p.as_str()));
    let stale = refreshed.stale;
    // re-read only when a refresh actually wrote
    let rows = if refreshed.any_refreshed {
        fetch()?
    } else {
        rows
    };
    if json {
        let items: Vec<_> = rows
            .iter()
            .map(|(p, k, q, s, e, sig, _)| {
                serde_json::json!({"file": p, "kind": k, "symbol": q, "start": s, "end": e, "sig": sig, "stale": stale.contains(p)})
            })
            .collect();
        return jout(&items, baseline);
    }
    let mut out = String::new();
    let mut current = String::new();
    // Symbols whose span is still open — the lexical ancestors of the next row.
    let mut open: Vec<(String, i64)> = Vec::new();
    for (path, kind, name, s, e, sig, _) in rows {
        if path != current {
            open.clear();
            if stale.contains(&path) {
                out.push_str(&format!("{path}  (stale — file changed since indexing)\n"));
            } else {
                out.push_str(&format!("{path}\n"));
            }
            current = path;
        }
        while open.last().is_some_and(|(_, end)| *end < s) {
            open.pop();
        }
        // A qualified parent that is not the enclosing span (a Go method
        // beside, not inside, its receiver type) prints its full name at its
        // lexical depth, or the indent would claim the preceding symbol owns it.
        let nested = name
            .rsplit_once('.')
            .is_none_or(|(p, _)| open.last().is_some_and(|(q, _)| q == p));
        let depth = if nested {
            name.matches('.').count()
        } else {
            open.len()
        };
        let indent = "  ".repeat(depth + 1);
        open.push((name.clone(), e));
        // The indent already encodes the ancestors; repeating them per name is
        // quadratic on deep trees (XML/POM: 10+ levels made `outline pom.xml`
        // cost more than reading the file). Print the leaf; `--json` keeps the
        // full qualified name callers address symbols by.
        let leaf = if nested {
            name.rsplit('.').next().unwrap_or(&name)
        } else {
            &name
        };
        if show_sig {
            out.push_str(&format!("{indent}{kind} {leaf} :{s}-{e}  {sig}\n"));
        } else {
            out.push_str(&format!("{indent}{kind} {leaf} :{s}-{e}\n"));
        }
    }
    Ok((out, baseline))
}
