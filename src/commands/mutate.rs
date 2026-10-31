//! Mutating commands: edit, note, rename.

use super::{locate_candidates, locate_symbol};
use crate::{db, editing, indexer, lang};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::Connection;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

/// The guard owns only paths it exclusively created. Retaining a recovery
/// artifact explicitly transfers cleanup responsibility to the user.
struct Stage {
    path: PathBuf,
    cleanup: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MutationEvent {
    Permissions,
    Validate,
    Swap,
    Recover,
}

/// Private deterministic fault seam; production always uses the defaults.
trait MutationIo {
    fn write_stage(&mut self, file: &mut File, source: &[u8]) -> std::io::Result<()> {
        file.write_all(source)
    }

    fn event(&mut self, _event: MutationEvent, _index: usize) -> Result<()> {
        Ok(())
    }
}

struct NativeIo;
impl MutationIo for NativeIo {}

impl Stage {
    fn create(
        abs: &Path,
        source: &[u8],
        permissions: Option<Permissions>,
        io: &mut impl MutationIo,
    ) -> Result<Self> {
        Self::allocate(abs, source, permissions, io, || {
            NEXT_STAGE.fetch_add(1, Ordering::Relaxed)
        })
    }

    fn allocate(
        abs: &Path,
        source: &[u8],
        permissions: Option<Permissions>,
        io: &mut impl MutationIo,
        mut next: impl FnMut() -> u64,
    ) -> Result<Self> {
        for _ in 0..128 {
            let path = abs.with_file_name(format!(".cona-{}-{}.tmp", std::process::id(), next()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                // Do not expose a private original under a more permissive
                // umask while preparing its replacement or recovery copy.
                options.mode(if permissions.is_some() { 0o600 } else { 0o666 });
            }
            let mut file = match options.open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            let stage = Self {
                path,
                cleanup: true,
            };
            // Drop the open handle before guard cleanup, including on error
            // (Windows cannot unlink an open file).
            let result = (|| -> Result<()> {
                io.write_stage(&mut file, source)?;
                if let Some(permissions) = permissions {
                    io.event(MutationEvent::Permissions, 0)?;
                    // Apply after writing: Unix writes may clear set-id
                    // bits. Existing-file stages start private (0600).
                    file.set_permissions(permissions)?;
                }
                file.sync_all()?;
                Ok(())
            })();
            drop(file);
            result?;
            return Ok(stage);
        }
        bail!("could not exclusively allocate a sibling staging file after 128 attempts")
    }

    fn retain(&mut self) {
        self.cleanup = false;
    }

    fn replace(&mut self, destination: &Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, destination)?;
        // The path no longer belongs to this guard. Never clean a file
        // another writer might subsequently create at the spent name.
        self.cleanup = false;
        Ok(())
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct SourceChange {
    rel: String,
    original: Option<String>,
    replacement: String,
}

struct PreparedChange {
    abs: PathBuf,
    replacement: Stage,
    recovery: Option<Stage>,
}

fn check_original(abs: &Path, expected: Option<&str>) -> Result<()> {
    match (std::fs::read(abs), expected) {
        (Ok(live), Some(original)) if live == original.as_bytes() => Ok(()),
        (Err(error), None) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        (Err(error), _) => Err(error.into()),
        _ => bail!(
            "source changed since mutation was planned: {}",
            abs.display()
        ),
    }
}

fn validate_change(root: &Path, change: &SourceChange, prepared: &PreparedChange) -> Result<()> {
    if project_path(root, &change.rel)? != prepared.abs {
        bail!("mutation target changed while staging: {}", change.rel);
    }
    check_original(&prepared.abs, change.original.as_deref())
}

/// Per-file atomic replacement with best-effort recovery, NOT a crash-atomic
/// transaction or a lock against other writers. Never truncate a live source.
fn commit_changes(root: &Path, changes: &[SourceChange], io: &mut impl MutationIo) -> Result<()> {
    let mut prepared = Vec::with_capacity(changes.len());
    for change in changes {
        let abs = project_path(root, &change.rel)?;
        check_original(&abs, change.original.as_deref())?;
        let permissions = if change.original.is_some() {
            Some(std::fs::metadata(&abs)?.permissions())
        } else {
            None
        };
        let replacement =
            Stage::create(&abs, change.replacement.as_bytes(), permissions.clone(), io)
                .with_context(|| {
                    format!(
                        "staging replacement for {} failed; sources unchanged",
                        change.rel
                    )
                })?;
        let recovery = change
            .original
            .as_ref()
            .map(|source| Stage::create(&abs, source.as_bytes(), permissions, io))
            .transpose()
            .with_context(|| {
                format!(
                    "staging recovery for {} failed; sources unchanged",
                    change.rel
                )
            })?;
        prepared.push(PreparedChange {
            abs,
            replacement,
            recovery,
        });
    }
    // Check the whole plan before the first swap, then each target again just
    // before its swap. This detects observed edits, not every possible race.
    for (change, stage) in changes.iter().zip(&prepared) {
        validate_change(root, change, stage)?;
    }
    for (index, change) in changes.iter().enumerate() {
        let stage = &mut prepared[index];
        let result = (|| -> Result<()> {
            io.event(MutationEvent::Validate, index)?;
            validate_change(root, change, stage)?;
            io.event(MutationEvent::Swap, index)?;
            stage.replacement.replace(&stage.abs)?;
            Ok(())
        })();
        if let Err(error) = result {
            let mut restored = 0;
            let mut failures = Vec::new();
            for previous in (0..index).rev() {
                let old = &changes[previous];
                let stage = &mut prepared[previous];
                let recovery = (|| -> Result<()> {
                    io.event(MutationEvent::Recover, previous)?;
                    if project_path(root, &old.rel)? != stage.abs {
                        bail!("mutation target changed before recovery");
                    }
                    // Do not overwrite an observed edit made after our swap.
                    check_original(&stage.abs, Some(&old.replacement))?;
                    if let Some(backup) = &mut stage.recovery {
                        backup.replace(&stage.abs)?;
                    } else {
                        std::fs::remove_file(&stage.abs)?;
                    }
                    Ok(())
                })();
                match recovery {
                    Ok(()) => restored += 1,
                    Err(error) => {
                        let location = if let Some(backup) = &mut stage.recovery {
                            backup.retain();
                            // A full path remains usable even if the root's
                            // canonical spelling differs from the input.
                            format!("original preserved at {}", backup.path.display())
                        } else {
                            "new source could not be removed".to_string()
                        };
                        failures.push(format!("  {}: {error:#}; {location}", old.rel));
                    }
                }
            }
            let detail = if failures.is_empty() {
                String::new()
            } else {
                format!("\nrecovery FAILED:\n{}", failures.join("\n"))
            };
            bail!(
                "source commit failed on {} ({error:#}); restored {restored} of {index} committed file(s); failing target was not replaced{detail}",
                change.rel
            );
        }
    }
    Ok(())
}

/// Resolve a mutation target and prove it stays inside the project root.
/// Canonicalizes the file (or, for a new file, its parent) to catch symlinks.
pub(crate) fn project_path(root: &Path, supplied: &str) -> Result<PathBuf> {
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
    let checked = match std::fs::symlink_metadata(&candidate) {
        // Includes dangling symlinks: canonicalization must fail rather than
        // treating an unresolved link as an ordinary new file.
        Ok(_) => candidate.canonicalize()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = candidate
                .parent()
                .ok_or_else(|| anyhow!("mutation path has no parent: {supplied}"))?
                .canonicalize()?;
            parent.join(
                candidate
                    .file_name()
                    .ok_or_else(|| anyhow!("invalid mutation path: {supplied}"))?,
            )
        }
        Err(error) => return Err(error.into()),
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
    write_verified(root, &path, Some(&original), &new_src, force)?;
    let n = indexer::reindex_file(root, conn, &path)
        .context("edit committed source, but index refresh failed")?;
    Ok(format!(
        "edited {q} in {path} (was :{s}-{e}), re-indexed {n} symbols, syntax OK\n"
    ))
}

/// Shared syntax rule for edit, range, insert and rename. No source is staged
/// until every required verification succeeds.
fn verify_source(path: &str, new_src: &str, force: bool) -> Result<()> {
    if !force {
        let language = lang::detect_lang(path).ok_or_else(|| anyhow!("unknown language"))?;
        let errors = lang::syntax_errors(language, new_src)?;
        if !errors.is_empty() {
            let lines: Vec<String> = errors.iter().map(|l| l.to_string()).collect();
            let s = if lines.len() == 1 { "" } else { "s" };
            bail!(
                "syntax error{s} at line{s} {} of {path} — edit rejected, file unchanged \
                 (fix the code, or --force to write it anyway)",
                lines.join(", ")
            );
        }
    }
    Ok(())
}

fn write_verified(
    root: &Path,
    path: &str,
    original: Option<&str>,
    new_src: &str,
    force: bool,
) -> Result<()> {
    project_path(root, path)?;
    verify_source(path, new_src, force)?;
    commit_changes(
        root,
        &[SourceChange {
            rel: path.to_string(),
            original: original.map(str::to_string),
            replacement: new_src.to_string(),
        }],
        &mut NativeIo,
    )
}

/// `edit --range S-E FILE` — replace absolute lines S..=E, bypassing symbol
/// resolution, so a few lines can be patched without resending a whole body.
/// Still syntax-verified + rolled back.
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
    // validate in-root BEFORE any read/reindex, so a `../` file never enters
    // the index
    let abs = project_path(root, file)?;
    // reindex so the file on disk and the index agree afterward
    indexer::reindex_file(root, conn, file)?;
    let original = std::fs::read_to_string(&abs)?;
    let new_src = editing::splice_lines(&original, start, end, replacement);
    write_verified(root, file, Some(&original), &new_src, force)?;
    let n = indexer::reindex_file(root, conn, file)
        .context("edit committed source, but index refresh failed")?;
    Ok(format!(
        "edited {file} lines {start}-{end}, re-indexed {n} symbols, syntax OK\n"
    ))
}

/// `insert` — add new source without touching an existing body. Anchored on a
/// SYMBOL (`--before`/`--after`) or an absolute position (`--at <file> <line>`,
/// 0 = prepend, past-EOF = append); `--at` also works on fresh/empty files.
/// Whole-file syntax is re-verified.
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
            (path, line, format!("{pos} {q} in"))
        }
        (None, Some((file, line))) => (file.clone(), *line, format!("at line {line} of")),
        (Some(_), Some(_)) => bail!("pass either a symbol or --at <file> <line>, not both"),
        (None, None) => bail!("insert needs a symbol anchor or --at <file> <line>"),
    };
    // read what exists (empty string if the --at file is new)
    let abs = project_path(root, &path)?;
    let original = match std::fs::read_to_string(&abs) {
        Ok(source) => Some(source),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("cannot read {path}")),
    };
    let existing = original.as_deref().unwrap_or("");
    // symbol anchor → spaced like hand-written code; --at → verbatim
    let new_src = if symbol.is_some() {
        editing::splice_insert_spaced(existing, at_line, code)
    } else {
        editing::splice_insert(existing, at_line, code)
    };
    write_verified(root, &path, original.as_deref(), &new_src, force)?;
    let n = indexer::reindex_file(root, conn, &path)
        .context("insert committed source, but index refresh failed")?;
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
            // a key nothing resolves to anymore (symbol deleted or renamed
            // outside `cona rename`) — say so instead of silently never showing
            let dangling = locate_candidates(conn, &sym, None).map_or(true, |c| c.is_empty());
            let tag = if dangling { "  · dangling" } else { "" };
            out.push_str(&format!("#{id}  {sym}  ⚑ {note}  ({}){tag}\n", db::ago(ts)));
        }
        return Ok(out);
    };
    if text.is_empty() {
        // the same notes show/context print: resolve first, else the key as given
        let key = locate_symbol(conn, symbol).map_or_else(|_| symbol.to_string(), |l| l.3);
        let notes = db::notes_for(conn, &key)?;
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
    // Confinement and readability precede refresh: refreshing must never read
    // an indexed escape, and force must not turn I/O errors into omissions.
    let indexed = conn
        .prepare("SELECT path FROM files ORDER BY path")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for rel in indexed {
        let abs = project_path(root, &rel)?;
        match std::fs::read_to_string(abs) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("cannot read {rel}")),
        }
    }
    super::refresh_candidate_files(root, conn)?;
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
        .collect::<rusqlite::Result<_>>()?;
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
    let files: Vec<String> = files_stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    // Plan all edits before staging; every syntax gate must pass first.
    let mut plans = Vec::new();
    let mut hit_counts = Vec::new();
    let mut fallback_files: Vec<String> = Vec::new();
    for rel in &files {
        let abs = project_path(root, rel)
            .with_context(|| format!("invalid indexed mutation path: {rel}"))?;
        let src = std::fs::read_to_string(abs).with_context(|| format!("cannot read {rel}"))?;
        let flang = lang::detect_lang(rel);
        let (positions, semantic) = lang::ident_positions(flang, &src, &name);
        if positions.is_empty() {
            continue;
        }
        if !semantic {
            fallback_files.push(rel.clone());
        }
        let new_src = editing::apply_renames(&src, &positions, name.len(), new_name);
        if flang.is_some() {
            verify_source(rel, &new_src, force)?;
        }
        plans.push(SourceChange {
            rel: rel.clone(),
            original: Some(src),
            replacement: new_src,
        });
        hit_counts.push(positions.len());
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
    commit_changes(root, &plans, &mut NativeIo)?;
    // notes follow the symbol (and its members) instead of dangling
    let new_q = crate::graph::scope_parent(&q)
        .map_or_else(|| new_name.to_string(), |p| format!("{p}.{new_name}"));
    let moved = db::notes_rekey(conn, &q, &new_q)
        .context("rename committed source files, but note migration failed")?;
    let mut total = 0usize;
    let mut per_file = String::new();
    for (plan, hits) in plans.iter().zip(hit_counts) {
        let rel = &plan.rel;
        indexer::reindex_file(root, conn, rel).map_err(|e| {
            anyhow!("rename wrote source files, but index refresh failed for {rel}: {e}")
        })?;
        per_file.push_str(&format!("  {rel}: {hits}\n"));
        total += hits;
    }
    // per-file list only when the rename spans several files
    let mut out = format!(
        "renamed '{name}' → '{new_name}': {} in {}{}\n",
        crate::ui::plural(total, "occurrence"),
        crate::ui::plural(plans.len(), "file"),
        // --force skips the verify gate, so only an unforced run may claim it
        if force { "" } else { ", syntax OK" }
    );
    if plans.len() > 1 {
        out.push_str(&per_file);
    }
    if moved > 0 {
        out.push_str(&format!(
            "moved {} to {new_q}\n",
            crate::ui::plural(moved, "note")
        ));
    }
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
    use super::*;
    use std::path::Path;

    struct Fixture {
        root: PathBuf,
        conn: Connection,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "cona-mutation-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(
                "CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT UNIQUE,mtime INTEGER,size INTEGER,lang TEXT);
                 CREATE TABLE symbols(id INTEGER PRIMARY KEY,file_id INTEGER,name TEXT,qualified TEXT,kind TEXT,
                    parent TEXT,start_line INTEGER,end_line INTEGER,signature TEXT);
                 CREATE TABLE notes(id INTEGER PRIMARY KEY,symbol TEXT,note TEXT,ts INTEGER);"
            ).unwrap();
            for (path, src) in files {
                std::fs::write(root.join(path), src).unwrap();
                indexer::reindex_file(&root, &conn, path).unwrap();
            }
            Self { root, conn }
        }

        fn read(&self, path: &str) -> String {
            std::fs::read_to_string(self.root.join(path)).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[derive(Default)]
    struct FaultIo {
        fail_events: Vec<(MutationEvent, usize)>,
        writes: usize,
        fail_write: Option<usize>,
        edit_on_event: Option<(MutationEvent, usize, PathBuf, Vec<u8>)>,
    }

    impl MutationIo for FaultIo {
        fn write_stage(&mut self, file: &mut File, source: &[u8]) -> std::io::Result<()> {
            let index = self.writes;
            self.writes += 1;
            if self.fail_write == Some(index) {
                file.write_all(&source[..source.len().min(5)])?;
                return Err(std::io::Error::other("injected partial stage write"));
            }
            file.write_all(source)
        }

        fn event(&mut self, event: MutationEvent, index: usize) -> Result<()> {
            if let Some((when, at, path, content)) = &self.edit_on_event {
                if (*when, *at) == (event, index) {
                    std::fs::write(path, content)?;
                }
            }
            if self.fail_events.contains(&(event, index)) {
                bail!("injected {event:?} failure");
            }
            Ok(())
        }
    }

    fn artifacts(root: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".cona-")
            })
            .collect()
    }

    #[test]
    fn exclusive_stage_collision_and_cleanup() {
        let f = Fixture::new(&[("a.rs", "fn old() {}\n")]);
        let candidate = f.root.join(format!(".cona-{}-42.tmp", std::process::id()));
        std::fs::write(&candidate, "sentinel").unwrap();
        let error = Stage::allocate(
            &f.root.join("a.rs"),
            b"replacement",
            None,
            &mut NativeIo,
            || 42,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("128 attempts"));
        assert_eq!(std::fs::read_to_string(&candidate).unwrap(), "sentinel");
        std::fs::remove_file(candidate).unwrap();
        let mut io = FaultIo {
            fail_write: Some(0),
            ..Default::default()
        };
        assert!(Stage::create(&f.root.join("a.rs"), b"replacement", None, &mut io).is_err());
        assert_eq!(f.read("a.rs"), "fn old() {}\n");
        assert!(artifacts(&f.root).is_empty());
    }

    #[test]
    fn concurrent_stage_allocations_are_distinct_and_owned() {
        let f = Fixture::new(&[("a.rs", "fn old() {}\n")]);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    let abs = f.root.join("a.rs");
                    scope.spawn(move || Stage::create(&abs, b"stage", None, &mut NativeIo).unwrap())
                })
                .collect();
            let stages: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            let paths: std::collections::HashSet<_> = stages.iter().map(|s| &s.path).collect();
            assert_eq!(paths.len(), 16);
            assert_eq!(artifacts(&f.root).len(), 16);
        });
        assert!(artifacts(&f.root).is_empty());
    }

    #[test]
    fn consumed_stage_does_not_clean_a_reused_path() {
        let f = Fixture::new(&[("a.rs", "fn old() {}\n")]);
        let mut stage = Stage::create(&f.root.join("a.rs"), b"new", None, &mut NativeIo).unwrap();
        stage.replace(&f.root.join("a.rs")).unwrap();
        let spent = stage.path.clone();
        std::fs::write(&spent, "sentinel").unwrap();
        drop(stage);
        assert_eq!(std::fs::read_to_string(spent).unwrap(), "sentinel");
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_stage_skips_symlinks_and_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new(&[("a.rs", "fn old() {}\n")]);
        let candidate = f.root.join(format!(".cona-{}-42.tmp", std::process::id()));
        std::fs::write(f.root.join("sentinel"), "sentinel").unwrap();
        std::os::unix::fs::symlink("sentinel", &candidate).unwrap();
        assert!(Stage::allocate(
            &f.root.join("a.rs"),
            b"replacement",
            None,
            &mut NativeIo,
            || 42
        )
        .is_err());
        assert_eq!(f.read("sentinel"), "sentinel");
        assert!(candidate.is_symlink());
        std::fs::set_permissions(f.root.join("a.rs"), Permissions::from_mode(0o751)).unwrap();
        write_verified(
            &f.root,
            "a.rs",
            Some(&f.read("a.rs")),
            "fn new() {}\n",
            false,
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(f.root.join("a.rs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
        let mut io = FaultIo {
            fail_events: vec![(MutationEvent::Permissions, 0)],
            ..Default::default()
        };
        assert!(Stage::create(
            &f.root.join("a.rs"),
            b"other",
            Some(Permissions::from_mode(0o751)),
            &mut io
        )
        .is_err());
        assert_eq!(f.read("a.rs"), "fn new() {}\n");
        assert_eq!(artifacts(&f.root), vec![candidate]);
    }

    #[test]
    fn legacy_staging_file_is_not_owned() {
        let f = Fixture::new(&[("a.rs", "fn old() {}\n")]);
        std::fs::write(f.root.join("a.rs.cona-tmp"), "sentinel").unwrap();
        write_verified(
            &f.root,
            "a.rs",
            Some(&f.read("a.rs")),
            "fn new() {}\n",
            false,
        )
        .unwrap();
        assert_eq!(f.read("a.rs.cona-tmp"), "sentinel");
    }

    #[cfg(unix)]
    #[test]
    fn legacy_staging_symlink_is_not_followed() {
        let f = Fixture::new(&[("a.rs", "fn old() {}\n")]);
        std::fs::write(f.root.join("sentinel"), "sentinel").unwrap();
        std::os::unix::fs::symlink("sentinel", f.root.join("a.rs.cona-tmp")).unwrap();
        write_verified(
            &f.root,
            "a.rs",
            Some(&f.read("a.rs")),
            "fn new() {}\n",
            false,
        )
        .unwrap();
        assert_eq!(f.read("sentinel"), "sentinel");
        assert!(f.root.join("a.rs.cona-tmp").is_symlink());
    }

    #[test]
    fn rename_refreshes_before_authorizing() {
        for (changed, expected) in [
            ("fn other() { old(); }\n", "not found"),
            ("fn old() {}\nfn taken() {}\n", "already defined"),
        ] {
            let f = Fixture::new(&[("a.rs", "fn old() {}\n")]);
            db::note_add(&f.conn, "old", "keep").unwrap();
            let notes = db::notes_all(&f.conn).unwrap();
            std::fs::write(f.root.join("a.rs"), changed).unwrap();
            let error = cmd_rename(&f.root, &f.conn, "old", "taken", false).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(f.read("a.rs"), changed);
            assert_eq!(db::notes_all(&f.conn).unwrap(), notes);
        }
    }

    #[test]
    fn rename_refreshes_new_ambiguity() {
        let f = Fixture::new(&[("a.rs", "fn old() {}\n"), ("b.rs", "fn other() {}\n")]);
        std::fs::write(f.root.join("b.rs"), "fn old() {}\nfn another() {}\n").unwrap();
        db::note_add(&f.conn, "old", "keep").unwrap();
        let notes = db::notes_all(&f.conn).unwrap();
        let before = [f.read("a.rs"), f.read("b.rs")];
        assert!(cmd_rename(&f.root, &f.conn, "old", "new", false).is_err());
        assert_eq!([f.read("a.rs"), f.read("b.rs")], before);
        assert_eq!(db::notes_all(&f.conn).unwrap(), notes);
    }

    #[test]
    fn rename_prunes_deleted_target_and_refuses_unreadable_candidate() {
        let f = Fixture::new(&[
            ("a.rs", "fn old() {}\n"),
            ("b.rs", "fn use_it() { old(); }\n"),
        ]);
        db::note_add(&f.conn, "old", "keep").unwrap();
        let notes = db::notes_all(&f.conn).unwrap();
        std::fs::remove_file(f.root.join("a.rs")).unwrap();
        assert!(cmd_rename(&f.root, &f.conn, "old", "new", false).is_err());
        assert_eq!(f.read("b.rs"), "fn use_it() { old(); }\n");
        assert_eq!(db::notes_all(&f.conn).unwrap(), notes);
        std::fs::write(f.root.join("a.rs"), "fn old() {}\n").unwrap();
        indexer::reindex_file(&f.root, &f.conn, "a.rs").unwrap();
        std::fs::write(f.root.join("b.rs"), [0xff]).unwrap();
        for force in [false, true] {
            let error = cmd_rename(&f.root, &f.conn, "old", "new", force).unwrap_err();
            assert!(error.to_string().contains("cannot read b.rs"), "{error}");
            assert_eq!(f.read("a.rs"), "fn old() {}\n");
            assert_eq!(std::fs::read(f.root.join("b.rs")).unwrap(), [0xff]);
            assert_eq!(db::notes_all(&f.conn).unwrap(), notes);
        }
    }

    #[test]
    fn rename_force_collision_fallback_and_notes() {
        let f = Fixture::new(&[
            ("a.rs", "fn old() {}\nfn taken() {}\n"),
            ("b.rs", "fn broken( { old();"),
        ]);
        db::note_add(&f.conn, "old", "keep").unwrap();
        assert!(cmd_rename(&f.root, &f.conn, "old", "new", false).is_err());
        assert_eq!(f.read("a.rs"), "fn old() {}\nfn taken() {}\n");
        // Unsupported indexed data uses lang's existing textual fallback.
        std::fs::write(f.root.join("c.txt"), "old").unwrap();
        let modified = std::fs::metadata(f.root.join("c.txt"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        f.conn
            .execute(
                "INSERT INTO files(path,mtime,size) VALUES('c.txt',?1,3)",
                [modified],
            )
            .unwrap();
        let output = cmd_rename(&f.root, &f.conn, "old", "taken", true).unwrap();
        assert!(output.contains("textual fallback"), "{output}");
        assert!(output.contains("moved 1 note"), "{output}");
        assert!(!output.contains("syntax OK"));
        assert_eq!(db::notes_for(&f.conn, "taken").unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn force_cannot_refresh_or_read_an_indexed_escape() {
        let f = Fixture::new(&[("a.rs", "fn old() {}\n")]);
        // The sentinel is outside the project, but inside this owned fixture.
        let project = f.root.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("a.rs"), "fn old() {}\n").unwrap();
        indexer::reindex_file(&project, &f.conn, "a.rs").unwrap();
        std::fs::remove_file(project.join("a.rs")).unwrap();
        std::os::unix::fs::symlink("../a.rs", project.join("a.rs")).unwrap();
        assert!(cmd_rename(&project, &f.conn, "old", "new", true).is_err());
        assert!(cmd_edit_code(&project, &f.conn, "old", "fn new() {}", true).is_err());
        assert_eq!(f.read("a.rs"), "fn old() {}\n");
    }

    #[test]
    fn syntax_failure_leaves_source_and_sentinel_unchanged() {
        let f = Fixture::new(&[("a.rs", "fn old() {}\r\n")]);
        std::fs::write(f.root.join("a.rs.cona-tmp"), "sentinel").unwrap();
        assert!(write_verified(&f.root, "a.rs", Some(&f.read("a.rs")), "fn {", false).is_err());
        assert_eq!(f.read("a.rs"), "fn old() {}\r\n");
        assert_eq!(f.read("a.rs.cona-tmp"), "sentinel");
    }

    fn changes(f: &Fixture) -> Vec<SourceChange> {
        ["a.rs", "b.rs", "c.rs"]
            .into_iter()
            .map(|rel| {
                let original = f.read(rel);
                SourceChange {
                    rel: rel.to_string(),
                    replacement: original.replace("old", "new"),
                    original: Some(original),
                }
            })
            .collect()
    }

    fn recovery_fixture() -> Fixture {
        let f = Fixture::new(&[
            ("a.rs", "fn old() {}\r\n"),
            (
                "b.rs",
                &format!("fn call() {{ old(); }}\n// {}", "x".repeat(1600)),
            ),
            ("c.rs", "fn other() { old(); }"),
        ]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in ["a.rs", "b.rs", "c.rs"] {
                std::fs::set_permissions(f.root.join(path), Permissions::from_mode(0o751)).unwrap();
            }
        }
        f
    }

    fn assert_originals(f: &Fixture, plans: &[SourceChange]) {
        for plan in plans {
            assert_eq!(
                f.read(&plan.rel),
                *plan.original.as_ref().unwrap(),
                "{}",
                plan.rel
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(f.root.join(&plan.rel))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o751
                );
            }
        }
    }

    #[test]
    fn partial_staging_failures_never_touch_any_source() {
        for fail in 0..6 {
            let f = recovery_fixture();
            let plans = changes(&f);
            let mut io = FaultIo {
                fail_write: Some(fail),
                ..Default::default()
            };
            let error = commit_changes(&f.root, &plans, &mut io).unwrap_err();
            assert!(
                format!("{error:#}").contains("partial stage write"),
                "{error:#}"
            );
            assert_originals(&f, &plans);
            assert!(artifacts(&f.root).is_empty());
        }
    }

    #[test]
    fn swap_and_path_failures_recover_actual_committed_sources() {
        for (event, index) in [
            (MutationEvent::Swap, 0),
            (MutationEvent::Swap, 1),
            (MutationEvent::Swap, 2),
            (MutationEvent::Validate, 1),
            (MutationEvent::Validate, 2),
        ] {
            let f = recovery_fixture();
            let plans = changes(&f);
            let mut io = FaultIo {
                fail_events: vec![(event, index)],
                ..Default::default()
            };
            let error = commit_changes(&f.root, &plans, &mut io).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("restored {index} of {index}")),
                "{error}"
            );
            assert_originals(&f, &plans); // includes the failing file, not just earlier files
            assert!(artifacts(&f.root).is_empty());
        }
    }

    #[test]
    fn recovery_failure_retains_and_names_only_unrestored_backups() {
        let f = recovery_fixture();
        let plans = changes(&f);
        let mut io = FaultIo {
            fail_events: vec![(MutationEvent::Swap, 2), (MutationEvent::Recover, 0)],
            ..Default::default()
        };
        let error = commit_changes(&f.root, &plans, &mut io)
            .unwrap_err()
            .to_string();
        assert!(error.contains("restored 1 of 2"), "{error}");
        assert!(error.contains("recovery FAILED:\n  a.rs:"), "{error}");
        assert_eq!(f.read("a.rs"), plans[0].replacement);
        assert_eq!(f.read("b.rs"), *plans[1].original.as_ref().unwrap());
        assert_eq!(f.read("c.rs"), *plans[2].original.as_ref().unwrap());
        // Discover via a noncanonical spelling so this also covers canonical
        // diagnostic paths on Unix, not just Windows' extended-length prefix.
        let backups = artifacts(&f.root.join("."));
        assert_eq!(backups.len(), 1);
        let backup = backups[0].canonicalize().unwrap();
        assert!(
            error.contains(&format!("original preserved at {}", backup.display())),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(&backups[0]).unwrap(),
            *plans[0].original.as_ref().unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&backups[0]).unwrap().permissions().mode() & 0o777,
                0o751
            );
        }
    }

    #[test]
    fn post_commit_note_and_index_failures_report_changed_source() {
        let f = Fixture::new(&[("a.rs", "fn old() {}\n")]);
        db::note_add(&f.conn, "old", "keep").unwrap();
        f.conn
            .execute_batch(
                "CREATE TRIGGER fail_notes BEFORE UPDATE ON notes BEGIN
             SELECT RAISE(ABORT, 'injected notes failure'); END;",
            )
            .unwrap();
        let error = cmd_rename(&f.root, &f.conn, "old", "new", false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("committed source files, but note migration failed"),
            "{error}"
        );
        assert_eq!(f.read("a.rs"), "fn new() {}\n");
        assert_eq!(db::notes_for(&f.conn, "old").unwrap().len(), 1);
        assert!(artifacts(&f.root).is_empty());

        let f = Fixture::new(&[("a.rs", "fn old() {}\n")]);
        f.conn
            .execute_batch(
                "CREATE TRIGGER fail_index BEFORE INSERT ON symbols WHEN new.name = 'new' BEGIN
             SELECT RAISE(ABORT, 'injected index failure'); END;",
            )
            .unwrap();
        let error = cmd_edit_code(&f.root, &f.conn, "old", "fn new() {}", false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("committed source, but index refresh failed"),
            "{error}"
        );
        assert_eq!(f.read("a.rs"), "fn new() {}\n");
        assert!(artifacts(&f.root).is_empty());

        let f = Fixture::new(&[("a.rs", "fn old() {}\n")]);
        db::note_add(&f.conn, "old", "keep").unwrap();
        f.conn
            .execute_batch(
                "CREATE TRIGGER fail_index BEFORE INSERT ON symbols WHEN new.name = 'new' BEGIN
             SELECT RAISE(ABORT, 'injected index failure'); END;",
            )
            .unwrap();
        let error = cmd_rename(&f.root, &f.conn, "old", "new", false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("wrote source files, but index refresh failed"),
            "{error}"
        );
        assert_eq!(f.read("a.rs"), "fn new() {}\n");
        assert_eq!(db::notes_for(&f.conn, "new").unwrap().len(), 1);
        assert!(artifacts(&f.root).is_empty());
    }

    #[test]
    fn observed_edits_are_not_overwritten_before_commit_or_during_recovery() {
        let f = recovery_fixture();
        let plans = changes(&f);
        std::fs::write(f.root.join("b.rs"), "external edit").unwrap();
        assert!(commit_changes(&f.root, &plans, &mut NativeIo).is_err());
        assert_eq!(f.read("a.rs"), *plans[0].original.as_ref().unwrap());
        assert_eq!(f.read("b.rs"), "external edit");
        assert!(artifacts(&f.root).is_empty());

        let f = recovery_fixture();
        let plans = changes(&f);
        let mut io = FaultIo {
            edit_on_event: Some((
                MutationEvent::Validate,
                1,
                f.root.join("b.rs"),
                b"external edit".to_vec(),
            )),
            ..Default::default()
        };
        let error = commit_changes(&f.root, &plans, &mut io)
            .unwrap_err()
            .to_string();
        assert!(error.contains("restored 1 of 1"), "{error}");
        assert_eq!(f.read("a.rs"), *plans[0].original.as_ref().unwrap());
        assert_eq!(f.read("b.rs"), "external edit");
        assert!(artifacts(&f.root).is_empty());

        let f = recovery_fixture();
        let plans = changes(&f);
        let mut io = FaultIo {
            fail_events: vec![(MutationEvent::Swap, 1)],
            edit_on_event: Some((
                MutationEvent::Recover,
                0,
                f.root.join("a.rs"),
                b"external edit".to_vec(),
            )),
            ..Default::default()
        };
        let error = commit_changes(&f.root, &plans, &mut io)
            .unwrap_err()
            .to_string();
        assert!(error.contains("restored 0 of 1"), "{error}");
        assert!(error.contains("original preserved at"), "{error}");
        assert_eq!(f.read("a.rs"), "external edit");
        assert_eq!(artifacts(&f.root).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn actual_path_revalidation_failure_recovers_prior_swaps() {
        struct EscapeOnValidate(PathBuf);
        impl MutationIo for EscapeOnValidate {
            fn event(&mut self, event: MutationEvent, index: usize) -> Result<()> {
                if (event, index) == (MutationEvent::Validate, 1) {
                    std::fs::remove_file(self.0.join("project/b.rs"))?;
                    std::os::unix::fs::symlink("../sentinel", self.0.join("project/b.rs"))?;
                }
                Ok(())
            }
        }
        let owner = Fixture::new(&[]);
        let root = owner.root.join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(owner.root.join("sentinel"), "sentinel").unwrap();
        let plans: Vec<_> = ["a.rs", "b.rs"]
            .into_iter()
            .map(|rel| {
                std::fs::write(root.join(rel), "fn old() {}").unwrap();
                SourceChange {
                    rel: rel.to_string(),
                    original: Some("fn old() {}".to_string()),
                    replacement: "fn new() {}".to_string(),
                }
            })
            .collect();
        let error = commit_changes(&root, &plans, &mut EscapeOnValidate(owner.root.clone()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("escapes project root"), "{error}");
        assert!(error.contains("restored 1 of 1"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "fn old() {}"
        );
        assert_eq!(owner.read("sentinel"), "sentinel");
        assert!(artifacts(&root).is_empty());
    }

    #[test]
    fn mutation_paths_stay_under_root() {
        let f = Fixture::new(&[]);
        let root = &f.root;
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/file.rs"), "fn f() {}\n").unwrap();
        assert!(project_path(root, "nested/file.rs").is_ok());
        assert!(project_path(root, "../outside.rs").is_err());
        assert!(project_path(root, Path::new("/tmp/outside.rs").to_str().unwrap()).is_err());
        assert!(project_path(root, "nested/new.rs").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn mutation_paths_reject_symlink_escape() {
        let f = Fixture::new(&[]);
        let root = f.root.join("project");
        let outside = f.root.join("outside");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("nested/link")).unwrap();
        assert!(project_path(&root, "nested/link/created.rs").is_err());
        std::os::unix::fs::symlink(outside.join("missing.rs"), root.join("dangling.rs")).unwrap();
        assert!(project_path(&root, "dangling.rs").is_err());
    }
}
