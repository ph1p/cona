//! Agent tool-call hooks.
//!
//! cona is a *navigation accelerator*, never a gatekeeper. The PreToolUse
//! hook only ever redirects the agent toward a faster path and always fails
//! open — any parse error, missing index, unknown/non-code file, small file or
//! partial read passes straight through untouched. It never blocks anything the
//! agent's own machinery (caching, batching, other tools) relies on.
//!
//! Two intercepts, three outcomes each. A large full read / broad identifier
//! grep in an INDEXED project is *redirected* (blocked, with the faster cona
//! command in the reason). The same call in a git repo that simply hasn't been
//! indexed yet is *nudged* (allowed, but with a one-time hint that indexing
//! unlocks cona) — this is the cold-open case where orientation help matters
//! most and the old "indexed only" rule stayed silent. Everything else passes.

use crate::{db, indexer, lang};
use anyhow::Result;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Default line threshold above which a full read of an indexed code file is
/// redirected to `cona outline`/`show`. Override with `CONA_READ_MAX_LINES`.
const DEFAULT_MAX_LINES: i64 = 300;

/// What the hook should do about a candidate tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Pass through untouched.
    Allow,
    /// Block and point at the faster cona path (project is indexed, so the
    /// redirect is actionable right now).
    Redirect,
    /// Allow, but surface a one-time hint that indexing would unlock cona.
    /// Used when the file/project is a good cona fit but not yet indexed —
    /// the moment orientation help matters most (agent cold-opening a repo).
    Nudge,
}

/// Facts about a candidate `Read` call — kept primitive so the decision is
/// pure and unit-testable.
#[derive(Debug, Clone)]
pub struct ReadFacts {
    /// The Read already has an explicit offset/limit (agent is being surgical).
    pub partial: bool,
    /// cona indexes this language.
    pub is_code: bool,
    /// The file is present in the project index.
    pub indexed: bool,
    /// The file lives inside a git repo (a project cona could index).
    pub in_repo: bool,
    /// Line count on disk.
    pub lines: i64,
    /// Threshold above which we redirect.
    pub max_lines: i64,
}

/// Decide what to do with a candidate `Read`.
/// - large indexed code file, full read → Redirect (block, actionable now)
/// - large UNINDEXED code file in a git repo, full read → Nudge (index me)
/// - everything else (partial, small, non-code, loose file) → Allow
pub fn decide_read(f: &ReadFacts) -> Decision {
    if f.partial || !f.is_code || f.lines <= f.max_lines {
        return Decision::Allow;
    }
    if f.indexed {
        Decision::Redirect
    } else if f.in_repo {
        Decision::Nudge
    } else {
        Decision::Allow
    }
}

/// Facts about a candidate `Grep` call — kept primitive so the decision is
/// pure and unit-testable.
#[derive(Debug, Clone)]
pub struct GrepFacts {
    /// The Grep is already narrowed (glob/type filter, head_limit, or a
    /// single-file path) — the agent is being surgical.
    pub surgical: bool,
    /// The pattern is a plain identifier cona can serve semantically.
    pub identifier: bool,
    /// The search root is an indexed cona project.
    pub indexed_project: bool,
    /// The search root is inside a git repo (indexable, if not yet indexed).
    pub in_repo: bool,
}

/// Decide what to do with a candidate `Grep`.
/// - broad identifier search over an indexed project → Redirect
/// - broad identifier search over an UNINDEXED git repo → Nudge
/// - surgical / regex / non-repo → Allow
pub fn decide_grep(f: &GrepFacts) -> Decision {
    if f.surgical || !f.identifier {
        return Decision::Allow;
    }
    if f.indexed_project {
        Decision::Redirect
    } else if f.in_repo {
        Decision::Nudge
    } else {
        Decision::Allow
    }
}

fn max_lines() -> i64 {
    std::env::var("CONA_READ_MAX_LINES")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_LINES)
}

/// Entry point for `cona hook <event>`. Reads the Claude hook payload from
/// stdin and prints a decision to stdout. ALWAYS exits 0 — this is a helper for
/// the agent, so a failure here must never break a tool call.
pub fn run(event: &str) -> Result<()> {
    if event != "PreToolUse" {
        return Ok(());
    }
    if std::env::var("CONA_HOOK_DISABLE").is_ok() {
        return Ok(());
    }
    // Any error → allow silently.
    let _ = try_pretooluse();
    Ok(())
}

fn try_pretooluse() -> Result<()> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let v: serde_json::Value = serde_json::from_str(&buf)?;

    match v["tool_name"].as_str() {
        Some("Read") => try_read(&v),
        Some("Grep") => try_grep(&v),
        _ => Ok(()),
    }
}

fn try_read(v: &serde_json::Value) -> Result<()> {
    let input = &v["tool_input"];
    let Some(file_path) = input["file_path"].as_str() else {
        return Ok(());
    };
    let partial = !input["offset"].is_null() || !input["limit"].is_null();

    let file_abs = PathBuf::from(file_path);
    let dir = file_abs.parent().unwrap_or(Path::new("."));
    // prefer the git root the agent is working in
    let cwd = v["cwd"].as_str().map(PathBuf::from);
    let root = cwd
        .filter(|c| file_abs.starts_with(c))
        .map(|c| db::git_root_from(&c))
        .unwrap_or_else(|| db::git_root_from(dir));

    let rel = file_abs
        .strip_prefix(&root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| file_path.to_string());

    let is_code = lang::detect_lang(&rel).is_some();
    let in_repo = root.join(".git").exists();

    // Cheap gates BEFORE reading any bytes: partial/non-code reads are Allow
    // regardless of size, and a multi-GB data file must never be slurped into
    // the hook just to be allowed anyway. A file of N lines is ≥ N bytes, so
    // size ≤ max_lines guarantees lines ≤ max_lines without reading. These
    // gates are deliberately narrower than decide_read's own cheap-gate — they
    // must NOT fold in `in_repo`, or a large indexed file in a non-git repo
    // would bail here before we ever check the index (that was the bug).
    let max_lines = max_lines();
    if partial || !is_code {
        return Ok(());
    }
    match std::fs::metadata(&file_abs) {
        Ok(m) if m.len() as i64 > max_lines => {}
        _ => return Ok(()),
    }
    let (lines, tokens) = match std::fs::read_to_string(&file_abs) {
        Ok(src) => (src.lines().count() as i64, db::est_tokens(src.len())),
        Err(_) => return Ok(()),
    };
    if lines <= max_lines {
        return Ok(());
    }

    // Large code file — is it indexed? Only open the DB if one already exists;
    // never create a project DB from a hook (mirrors try_grep's has_index gate).
    if !db::project_db_path(&root).exists() {
        return Ok(());
    }
    let conn = db::open_project_db(&root)?;
    let indexed: bool = conn
        .query_row("SELECT 1 FROM files WHERE path = ?1", [&rel], |_| Ok(true))
        .unwrap_or(false);

    let facts = ReadFacts {
        partial,
        is_code,
        indexed,
        in_repo,
        lines,
        max_lines,
    };
    match decide_read(&facts) {
        Decision::Allow => Ok(()),
        Decision::Redirect => {
            // refresh a stale index entry so line ranges we point at are correct
            if indexer::is_stale(&root, &conn, &rel) {
                let _ = indexer::reindex_file(&root, &conn, &rel);
            }
            let reason = format!(
                "{rel} is {lines} lines (~{tokens} tokens). cona can take you straight to \
                 the right spot for a fraction of the tokens: `cona outline {rel}` lists \
                 every symbol with its line range, then `cona show <Symbol>` prints only \
                 those lines. To understand a symbol (its body + what it calls + who calls it) \
                 in ONE call, prefer `cona context <Symbol>`; before changing one, \
                 `cona impact <Symbol>` shows its blast radius. (Also `cona find <Name>` \
                 / `cona refs <Name>`.) If you genuinely need the whole file, re-issue Read \
                 with an explicit offset/limit."
            );
            deny(&root, "hook:read-block", &rel, &reason)
        }
        Decision::Nudge => {
            // Fresh repo, large code file — indexing unlocks the fast path.
            // One hint per session so it never nags on subsequent reads.
            if !nudge_once(&root) {
                return Ok(());
            }
            let reason = format!(
                "This repo isn't cona-indexed yet. `cona index` (~1s) then \
                 `cona tree --rank` orients you, and `cona outline {rel}` / \
                 `cona show <Symbol>` read one symbol instead of all {lines} lines \
                 (~{tokens} tokens). Reading the whole file is fine for now."
            );
            allow_with_reason(&root, "hook:read-nudge", &rel, &reason)
        }
    }
}

fn try_grep(v: &serde_json::Value) -> Result<()> {
    let input = &v["tool_input"];
    let Some(pattern) = input["pattern"].as_str() else {
        return Ok(());
    };
    let path = input["path"].as_str();
    // any narrowing signal = surgical, pass through untouched
    let surgical = !input["glob"].is_null()
        || !input["type"].is_null()
        || !input["head_limit"].is_null()
        || path.map(|p| Path::new(p).is_file()).unwrap_or(false);
    // cheap gates first — only a broad identifier search pays for the DB check
    if surgical || !lang::is_valid_ident(pattern) {
        return Ok(());
    }

    let start = path
        .map(PathBuf::from)
        .or_else(|| v["cwd"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let root = db::git_root_from(&start);

    let facts = GrepFacts {
        surgical,
        identifier: true,
        // only projects the user already indexed — never create a DB from a hook
        indexed_project: db::has_index(&root),
        in_repo: root.join(".git").exists(),
    };
    match decide_grep(&facts) {
        Decision::Allow => Ok(()),
        Decision::Redirect => {
            let reason = format!(
                "this project is cona-indexed — `cona grep {pattern}` searches code only \
                 and labels every hit with its enclosing symbol, and `cona refs {pattern}` \
                 gives semantic usage sites (strings/comments never match). If you really need \
                 a raw regex/all-file search, re-issue Grep with a glob, type, path or \
                 head_limit filter."
            );
            deny(&root, "hook:grep-block", pattern, &reason)
        }
        Decision::Nudge => {
            if !nudge_once(&root) {
                return Ok(());
            }
            let reason = format!(
                "This repo isn't cona-indexed yet. `cona index` then \
                 `cona grep {pattern}` searches code only (skips strings/comments/other \
                 files) and labels each hit with its enclosing symbol; `cona refs {pattern}` \
                 gives semantic usage sites. This Grep runs as-is."
            );
            allow_with_reason(&root, "hook:grep-nudge", pattern, &reason)
        }
    }
}

/// Emit the PreToolUse deny decision and count the intercept. Credits no
/// tokens — the follow-up cona query logs the actual savings, crediting
/// the redirect too would count the same avoided read twice.
fn deny(root: &Path, cmd: &str, target: &str, reason: &str) -> Result<()> {
    db::log_usage_detail(root, cmd, 0, 1, 0, 0, target);
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

/// Let the tool call proceed but attach a hint the agent sees — a nudge, not
/// a block. Deliberately NO permissionDecision: emitting "allow" would bypass
/// the permission system (silently auto-approving e.g. an out-of-workspace
/// read); additionalContext leaves the permission flow untouched.
fn allow_with_reason(root: &Path, cmd: &str, target: &str, reason: &str) -> Result<()> {
    db::log_usage_detail(root, cmd, 0, 1, 0, 0, target);
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": reason,
        }
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

/// True at most once per (project, session): the first time we consider nudging
/// a fresh repo. A marker file under the data dir, stamped with the current
/// session id, makes the hint fire once and then stay quiet for the rest of the
/// session — subsequent large reads in the same unindexed repo pass silently.
///
/// Session identity comes from `CLAUDE_SESSION_ID` when the agent exports it;
/// without it we fall back to a per-day key so a long-lived shell still only
/// nags occasionally rather than on every read.
fn nudge_once(root: &Path) -> bool {
    let session = std::env::var("CLAUDE_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            // no session id: bucket by day so we nudge at most once/day/repo
            format!("day-{}", db::now() / 86_400)
        });
    let Ok(dir) = db::data_dir() else {
        return true; // can't track → don't suppress the (useful) first hint
    };
    let marker = dir
        .join("nudged")
        .join(format!("{}-{session}", db::project_hash(root)));
    if marker.exists() {
        return false;
    }
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // create the marker (best-effort); either way we nudge this once
    let _ = std::fs::write(&marker, b"");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> ReadFacts {
        ReadFacts {
            partial: false,
            is_code: true,
            indexed: true,
            in_repo: true,
            lines: 800,
            max_lines: 300,
        }
    }

    #[test]
    fn redirects_full_read_of_large_indexed_code_file() {
        assert_eq!(decide_read(&facts()), Decision::Redirect);
    }

    #[test]
    fn allows_partial_read() {
        assert_eq!(
            decide_read(&ReadFacts {
                partial: true,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn allows_small_file() {
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 42,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn allows_non_code_file() {
        assert_eq!(
            decide_read(&ReadFacts {
                is_code: false,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn nudges_large_unindexed_code_file_in_repo() {
        assert_eq!(
            decide_read(&ReadFacts {
                indexed: false,
                ..facts()
            }),
            Decision::Nudge
        );
    }

    #[test]
    fn allows_large_unindexed_file_outside_any_repo() {
        assert_eq!(
            decide_read(&ReadFacts {
                indexed: false,
                in_repo: false,
                ..facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn allows_exactly_at_threshold() {
        assert_eq!(
            decide_read(&ReadFacts {
                lines: 300,
                ..facts()
            }),
            Decision::Allow
        );
    }

    fn grep_facts() -> GrepFacts {
        GrepFacts {
            surgical: false,
            identifier: true,
            indexed_project: true,
            in_repo: true,
        }
    }

    #[test]
    fn redirects_broad_identifier_grep_in_indexed_project() {
        assert_eq!(decide_grep(&grep_facts()), Decision::Redirect);
    }

    #[test]
    fn allows_surgical_grep() {
        assert_eq!(
            decide_grep(&GrepFacts {
                surgical: true,
                ..grep_facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn allows_regex_pattern() {
        assert_eq!(
            decide_grep(&GrepFacts {
                identifier: false,
                ..grep_facts()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn nudges_broad_grep_in_unindexed_repo() {
        assert_eq!(
            decide_grep(&GrepFacts {
                indexed_project: false,
                ..grep_facts()
            }),
            Decision::Nudge
        );
    }

    #[test]
    fn allows_broad_grep_outside_any_repo() {
        assert_eq!(
            decide_grep(&GrepFacts {
                indexed_project: false,
                in_repo: false,
                ..grep_facts()
            }),
            Decision::Allow
        );
    }
}
