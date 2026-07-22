//! Mutating commands: edit, note, rename.

use super::locate_symbol;
use crate::{db, editing, indexer, lang};
use anyhow::{anyhow, bail, Result};
use rusqlite::Connection;
use std::io::Read;
use std::path::Path;

pub fn cmd_edit(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    file: Option<&str>,
    force: bool,
) -> Result<String> {
    let replacement = match file {
        Some(f) => std::fs::read_to_string(f)?,
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };
    cmd_edit_code(root, conn, symbol, &replacement, force)
}

/// The edit mechanism proper — replacement as a string, so MCP (which owns
/// stdin) and the CLI wrapper share one implementation.
pub(crate) fn cmd_edit_code(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    replacement: &str,
    force: bool,
) -> Result<String> {
    if replacement.trim().is_empty() {
        bail!("replacement is empty — pass --file or pipe code via stdin");
    }
    // first locate to learn which file, then re-index THAT file and locate
    // again — guarantees line numbers are fresh even if the file changed
    // since the last `cona index`.
    let (path0, _, _, _) = locate_symbol(conn, symbol)?;
    indexer::reindex_file(root, conn, &path0)?;
    let (path, s, e, q) = locate_symbol(conn, symbol)?;
    let abs = root.join(&path);
    let original = std::fs::read_to_string(&abs)?;
    let new_src = editing::splice_lines(&original, s as usize, e as usize, replacement);
    write_verified(root, conn, &path, &new_src, force)?;
    let n = indexer::reindex_file(root, conn, &path)?;
    Ok(format!(
        "edited {q} in {path} (was :{s}-{e}), re-indexed {n} symbols, syntax OK\n"
    ))
}

/// Syntax-verify (unless force) then write — the ONE gate shared by every
/// mutation path (edit / edit --range / insert). Invariant 3: on a parse error
/// the file is left untouched and an error returned.
fn write_verified(
    root: &Path,
    _conn: &Connection,
    path: &str,
    new_src: &str,
    force: bool,
) -> Result<()> {
    if !force {
        let language = lang::detect_lang(path).ok_or_else(|| anyhow!("unknown language"))?;
        let errors = lang::syntax_errors(language, new_src)?;
        if !errors.is_empty() {
            bail!(
                "syntax errors at lines {:?} — edit rejected, file unchanged (use --force to override)",
                errors
            );
        }
    }
    std::fs::write(root.join(path), new_src)?;
    Ok(())
}

/// `edit --range S-E FILE` — replace absolute lines S..=E of a file directly,
/// bypassing symbol resolution. Lets an agent patch a few lines without
/// resending a whole symbol body. Still syntax-verified + rolled back.
pub fn cmd_edit_range(
    root: &Path,
    conn: &Connection,
    file: &str,
    start: usize,
    end: usize,
    replacement: &str,
    force: bool,
) -> Result<String> {
    if start == 0 || end < start {
        bail!("invalid --range {start}-{end} (1-based, start ≤ end)");
    }
    // reindex first so the file on disk and the index agree afterward
    indexer::reindex_file(root, conn, file)?;
    let abs = root.join(file);
    let original = std::fs::read_to_string(&abs)?;
    let new_src = editing::splice_lines(&original, start, end, replacement);
    write_verified(root, conn, file, &new_src, force)?;
    let n = indexer::reindex_file(root, conn, file)?;
    Ok(format!(
        "edited {file} lines {start}-{end}, re-indexed {n} symbols, syntax OK\n"
    ))
}

/// `insert` — add new source without touching an existing body. Two targeting
/// modes: relative to a SYMBOL (`--before`/`--after`), or an absolute file
/// position (`--at <file> <line>`, `line` 0 = prepend, past-EOF = append).
/// The `--at` mode works on files with no indexed symbol (e.g. a fresh/empty
/// file). Fills the no-add-a-symbol gap; whole-file syntax is re-verified.
pub fn cmd_insert(
    root: &Path,
    conn: &Connection,
    symbol: Option<&str>,
    after: bool,
    at: Option<(String, usize)>,
    code: &str,
    force: bool,
) -> Result<String> {
    if code.trim().is_empty() {
        bail!("nothing to insert — pass --file or pipe code via stdin");
    }
    // resolve the target file + the insertion line
    let (path, at_line, label) = match (symbol, &at) {
        (Some(sym), None) => {
            let (path0, _, _, _) = locate_symbol(conn, sym)?;
            indexer::reindex_file(root, conn, &path0)?;
            let (path, s, e, q) = locate_symbol(conn, sym)?;
            // before → at line s-1; after → just past line e
            let line = if after {
                e as usize
            } else {
                (s as usize).saturating_sub(1)
            };
            let pos = if after { "after" } else { "before" };
            (path, line, format!("{pos} {q}"))
        }
        (None, Some((file, line))) => (file.clone(), *line, format!("at line {line} of")),
        (Some(_), Some(_)) => bail!("pass either a symbol or --at <file> <line>, not both"),
        (None, None) => bail!("insert needs a symbol anchor or --at <file> <line>"),
    };
    // read what exists (empty string if the --at file is new)
    let abs = root.join(&path);
    let original = std::fs::read_to_string(&abs).unwrap_or_default();
    let new_src = editing::splice_insert(&original, at_line, code);
    write_verified(root, conn, &path, &new_src, force)?;
    let n = indexer::reindex_file(root, conn, &path)?;
    Ok(format!(
        "inserted {label} {path}, re-indexed {n} symbols, syntax OK\n"
    ))
}

pub fn cmd_note(
    conn: &Connection,
    symbol: Option<&str>,
    text: &[String],
    rm: Option<i64>,
) -> Result<String> {
    if let Some(id) = rm {
        return if db::note_rm(conn, id)? {
            Ok(format!("removed note #{id}\n"))
        } else {
            bail!("no note #{id}")
        };
    }
    let Some(symbol) = symbol else {
        let all = db::notes_all(conn)?;
        if all.is_empty() {
            return Ok("no notes yet — `cona note <Sym> <text…>` to add one\n".to_string());
        }
        let mut out = String::new();
        for (id, sym, note, ts) in all {
            out.push_str(&format!("#{id}  {sym}  ⚑ {note}  ({})\n", db::ago(ts)));
        }
        return Ok(out);
    };
    if text.is_empty() {
        let notes = db::notes_for(conn, symbol)?;
        if notes.is_empty() {
            return Ok(format!("no notes on '{symbol}'\n"));
        }
        let mut out = String::new();
        for (id, note, ts) in notes {
            out.push_str(&format!("#{id}  ⚑ {note}  ({})\n", db::ago(ts)));
        }
        return Ok(out);
    }
    let note = text.join(" ");
    // prefer the canonical qualified name so lookups from show/context match
    let key = match locate_symbol(conn, symbol) {
        Ok((_, _, _, q)) => q,
        Err(_) => {
            eprintln!("note: '{symbol}' not (uniquely) in the index — saving under the given key");
            symbol.to_string()
        }
    };
    let id = db::note_add(conn, &key, &note)?;
    Ok(format!(
        "note #{id} added to {key} — shows up in show/context\n"
    ))
}

pub fn cmd_rename(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    new_name: &str,
    force: bool,
) -> Result<String> {
    if !lang::is_valid_ident(new_name) {
        bail!("'{new_name}' is not a valid identifier");
    }
    let (_, _, _, q) = locate_symbol(conn, symbol)?; // ambiguity → error here
    let name = db::name_tail(&q).to_string();
    if name == new_name {
        bail!("old and new name are identical");
    }
    // collision: an existing definition of the target name makes every
    // name-based ref ambiguous afterwards → refuse unless forced
    let mut coll_stmt = conn.prepare(
        "SELECT s.qualified, f.path, s.start_line FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.name = ?1 LIMIT 5",
    )?;
    let collisions: Vec<(String, String, i64)> = coll_stmt
        .query_map([new_name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .flatten()
        .collect();
    if !collisions.is_empty() && !force {
        let list: Vec<String> = collisions
            .iter()
            .map(|(cq, p, l)| format!("  {cq}  {p}:{l}"))
            .collect();
        bail!(
            "'{new_name}' already defined — renaming would collide (--force to override):\n{}",
            list.join("\n")
        );
    }
    let mut files_stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let files: Vec<String> = files_stmt.query_map([], |r| r.get(0))?.flatten().collect();
    // plan all edits in memory first — nothing is written until every file
    // passes; the original source rides along as the rollback copy
    let mut plans: Vec<(String, String, String, usize)> = Vec::new(); // rel, orig, new_src, hits
    let mut fallback_files: Vec<String> = Vec::new();
    for rel in &files {
        let Ok(src) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        let flang = lang::detect_lang(rel);
        let (positions, semantic) = lang::ident_positions(flang, &src, &name);
        if positions.is_empty() {
            continue;
        }
        if !semantic {
            fallback_files.push(rel.clone());
        }
        let new_src = editing::apply_renames(&src, &positions, name.len(), new_name);
        if let Some(l) = flang {
            let errs = lang::syntax_errors(l, &new_src).unwrap_or_default();
            if !errs.is_empty() && !force {
                bail!(
                    "rename would break {rel} (syntax error near line {}) — nothing written",
                    errs[0]
                );
            }
        }
        plans.push((rel.clone(), src, new_src, positions.len()));
    }
    if plans.is_empty() {
        bail!("no identifier occurrences of '{name}' found");
    }
    if !fallback_files.is_empty() && !force {
        bail!(
            "these files can't be parsed semantically; a textual rename could touch strings/comments (--force to proceed):\n  {}",
            fallback_files.join("\n  ")
        );
    }
    // write all-or-nothing: the planned originals are the rollback copies
    let mut written: Vec<&(String, String, String, usize)> = Vec::new();
    for plan in &plans {
        let (rel, _, new_src, _) = plan;
        if let Err(e) = std::fs::write(root.join(rel), new_src) {
            for (wrel, worig, ..) in written.iter().copied() {
                let _ = std::fs::write(root.join(wrel), worig);
            }
            bail!(
                "write failed on {rel} ({e}) — rolled back {} file(s)",
                written.len()
            );
        }
        written.push(plan);
    }
    let mut total = 0usize;
    let mut out = format!("renamed '{name}' → '{new_name}':\n");
    for (rel, _, _, hits) in &plans {
        let _ = indexer::reindex_file(root, conn, rel);
        out.push_str(&format!("  {rel}: {hits}\n"));
        total += hits;
    }
    out.push_str(&format!(
        "{total} occurrence(s) across {} file(s)\n",
        plans.len()
    ));
    if !fallback_files.is_empty() {
        out.push_str(&format!(
            "warning: textual fallback used in: {}\n",
            fallback_files.join(", ")
        ));
    }
    Ok(out)
}
