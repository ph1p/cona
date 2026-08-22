//! `outline`: every symbol of one file with line ranges.

use crate::commands::jout;
use crate::{db, indexer};
use anyhow::{bail, Result};
use rusqlite::Connection;
use std::path::Path;

/// One outline row: `(path, kind, qualified, start, end, signature, file_size)`.
type OutlineRow = (String, String, String, i64, i64, String, i64);

pub fn cmd_outline(
    root: &Path,
    conn: &Connection,
    file: &str,
    show_sig: bool,
    json: bool,
) -> Result<(String, i64)> {
    // suffix match only at a path-separator boundary (`db.rs` must not pull in
    // `gitdb.rs`), with LIKE metacharacters escaped so `_`/`%` in names stay literal
    let escaped = file
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let mut stmt = conn.prepare(
        "SELECT f.path, s.kind, s.qualified, s.start_line, s.end_line, s.signature, f.size
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE f.path = ?1 OR f.path LIKE ?2 ESCAPE '\\'
         ORDER BY f.path, s.start_line",
    )?;
    // One projection, used again after a refresh replaces the rows below.
    let like = format!("%/{escaped}");
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
    let rows = fetch()?;
    if rows.is_empty() {
        // A directory is a natural thing to hand `outline`; answer it with the
        // command that does cover directories instead of erroring out.
        // On disk normally; the index probe still answers for a directory that
        // was indexed but has since been removed from the working tree.
        let is_dir = root.join(file).is_dir()
            || conn
                .query_row(
                    "SELECT 1 FROM files WHERE path LIKE ?1 ESCAPE '\\' LIMIT 1",
                    [format!("{escaped}/%")],
                    |_| Ok(()),
                )
                .is_ok();
        if is_dir {
            bail!("'{file}' is a directory — try `cona tree --path {file}`");
        }
        // Distinguish the three remaining causes — each has a different next
        // step, and the hook may have redirected an agent here, so a dead-end
        // error would strand it with no route back to the content.
        let in_index = conn
            .query_row(
                "SELECT 1 FROM files WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\' LIMIT 1",
                rusqlite::params![file, like],
                |_| Ok(()),
            )
            .is_ok();
        if in_index {
            bail!(
                "'{file}' is indexed but has no extractable symbols — read it directly, \
                 or search it with `cona grep <pattern> --path {file}`"
            );
        }
        if root.join(file).is_file() {
            bail!("'{file}' exists but is not in the index — run `cona index`, then retry");
        }
        bail!("no file '{file}' in the index — `cona find <name>` locates symbols by name");
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
    // Every line range below comes from the index, so the matched files must be
    // refreshed before they are printed (invariant 2) — an outline is exactly the
    // map an agent uses to pick its next `show`, and stale ranges send it to the
    // wrong lines. In read-only mode the refresh cannot happen, so name the stale
    // files instead of presenting their old ranges as current.
    let refreshed = indexer::refresh_files(root, conn, rows.iter().map(|(p, ..)| p.as_str()));
    let stale = refreshed.stale;
    // Re-read only when a refresh actually wrote — otherwise the rows above are
    // still current and a second identical query is pure waste.
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
    for (path, kind, name, s, e, sig, _) in rows {
        if path != current {
            if stale.contains(&path) {
                out.push_str(&format!("{path}  (stale — file changed since indexing)\n"));
            } else {
                out.push_str(&format!("{path}\n"));
            }
            current = path;
        }
        let depth = name.matches('.').count();
        let indent = "  ".repeat(depth + 1);
        // The indent already encodes the ancestor chain, so repeating it in every
        // name is redundant — and on deeply nested trees (XML/POM: 10+ levels)
        // that redundancy is quadratic, which made `outline pom.xml` cost more
        // than reading the file. Print the leaf; `--json` keeps the full
        // qualified name, since that is what callers address symbols by.
        let leaf = name.rsplit('.').next().unwrap_or(&name);
        if show_sig {
            out.push_str(&format!("{indent}{kind} {leaf} :{s}-{e}  {sig}\n"));
        } else {
            out.push_str(&format!("{indent}{kind} {leaf} :{s}-{e}\n"));
        }
    }
    Ok((out, baseline))
}
