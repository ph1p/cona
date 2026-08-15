//! Mutating commands: edit, note, rename.

use super::locate_symbol;
use crate::{db, editing, indexer, lang};
use anyhow::{anyhow, bail, Result};
use rusqlite::Connection;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// Resolve a mutation target and prove it remains inside the project root.
/// Existing files are canonicalized to catch symlinks; for insertion, the
/// nearest existing parent is canonicalized instead.
fn project_path(root: &Path, supplied: &str) -> Result<PathBuf> {
    let rel = Path::new(supplied);
    if supplied.is_empty() || rel.is_absolute() {
        bail!("mutation path must be a non-empty project-relative path: {supplied}");
    }
    if rel.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("mutation path escapes project root: {supplied}");
    }
    let root = root.canonicalize()?;
    let candidate = root.join(rel);
    let checked = if candidate.exists() {
        candidate.canonicalize()?
    } else {
        let parent = candidate
            .parent()
            .ok_or_else(|| anyhow!("mutation path has no parent: {supplied}"))?
            .canonicalize()?;
        parent.join(
            candidate
                .file_name()
                .ok_or_else(|| anyhow!("invalid mutation path: {supplied}"))?,
        )
    };
    if !checked.starts_with(&root) {
        bail!("mutation path escapes project root: {supplied}");
    }
    Ok(checked)
}

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
    let (path, s, e, q) = super::locate_for_write(root, conn, symbol)?;
    let abs = project_path(root, &path)?;
    let original = std::fs::read_to_string(&abs)?;
    let new_src = editing::splice_lines(&original, s as usize, e as usize, replacement);
    write_verified(root, &path, &new_src, force)?;
    let n = indexer::reindex_file(root, conn, &path)?;
    Ok(format!(
        "edited {q} in {path} (was :{s}-{e}), re-indexed {n} symbols, syntax OK\n"
    ))
}

/// Syntax-verify (unless force) then write — the ONE gate shared by every
/// mutation path (edit / edit --range / insert). Invariant 3: on a parse error
/// the file is left untouched and an error returned.
fn write_verified(root: &Path, path: &str, new_src: &str, force: bool) -> Result<()> {
    let abs = project_path(root, path)?;
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
    // Atomic replace: write a sibling temp file, then rename over the target.
    // A crash or ENOSPC mid-write must never leave a truncated source file —
    // the rename either fully lands or the original survives intact.
    let tmp = abs.with_extension(format!(
        "{}.cona-tmp",
        abs.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, new_src)
        .and_then(|()| {
            // carry over the original's permissions (exec bits etc.) — the temp
            // file was created with defaults
            if let Ok(meta) = std::fs::metadata(&abs) {
                let _ = std::fs::set_permissions(&tmp, meta.permissions());
            }
            std::fs::rename(&tmp, &abs)
        })
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
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
    // validate the path is in-root BEFORE any read/reindex, so an out-of-root
    // `../` file never even enters the index
    let abs = project_path(root, file)?;
    // reindex so the file on disk and the index agree afterward
    indexer::reindex_file(root, conn, file)?;
    let original = std::fs::read_to_string(&abs)?;
    let new_src = editing::splice_lines(&original, start, end, replacement);
    write_verified(root, file, &new_src, force)?;
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
            let (path, s, e, q) = super::locate_for_write(root, conn, sym)?;
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
    let abs = project_path(root, &path)?;
    let original = std::fs::read_to_string(&abs).unwrap_or_default();
    let new_src = editing::splice_insert(&original, at_line, code);
    write_verified(root, &path, &new_src, force)?;
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
        let Ok(abs) = project_path(root, rel) else {
            bail!("indexed mutation path escapes project root: {rel}");
        };
        let Ok(src) = std::fs::read_to_string(abs) else {
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
        let abs = project_path(root, rel)?;
        if let Err(e) = std::fs::write(abs, new_src) {
            for (wrel, worig, ..) in written.iter().copied() {
                if let Ok(wabs) = project_path(root, wrel) {
                    let _ = std::fs::write(wabs, worig);
                }
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
        indexer::reindex_file(root, conn, rel).map_err(|e| {
            anyhow!("rename wrote source files, but index refresh failed for {rel}: {e}")
        })?;
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

#[cfg(test)]
mod tests {
    use super::project_path;
    use std::path::Path;

    fn fixture(tag: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("cona-mutation-path-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/file.rs"), "fn f() {}\n").unwrap();
        root
    }

    #[test]
    fn mutation_paths_stay_under_root() {
        let root = fixture("stay-under");
        assert!(project_path(&root, "nested/file.rs").is_ok());
        assert!(project_path(&root, "../outside.rs").is_err());
        assert!(project_path(&root, Path::new("/tmp/outside.rs").to_str().unwrap()).is_err());
        assert!(project_path(&root, "nested/new.rs").is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn mutation_paths_reject_symlink_escape() {
        let root = fixture("symlink-escape");
        let outside = root
            .parent()
            .unwrap()
            .join(format!("cona-outside-{}-symlink", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("nested/link")).unwrap();
        assert!(project_path(&root, "nested/link/created.rs").is_err());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }
}
