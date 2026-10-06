//! `discover`: what past agent sessions in this project spent reading and
//! searching code by hand, and what cona would have cost instead.
//!
//! Hooks and the usage log only see cona-enabled sessions; Claude Code
//! transcripts (`~/.claude/projects/<encoded cwd>/`) see every Read, Grep and
//! shell `cat`/`sed`/`rg`, with result sizes.
//!
//! Conservative: a whole-file read is credited with `outline` + one
//! median-sized `show`; partial reads and greps are counted, never claimed.

use crate::hook::{classify_command, split_pipeline, ShellIntent};
use crate::{db, lang};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One tool call that pulled code into context, as recovered from a transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A cona query (CLI through the shell, or an MCP tool).
    Cona,
    /// Read of a whole file (`path` relative to the project root when inside it).
    FullRead(String),
    /// Read narrowed by offset/limit or a line range.
    PartialRead,
    /// Grep / rg / grep.
    Grep,
}

/// Map one tool_use to its events (`sed -n 1,80p a.rs; cona show X` is two).
/// `cwd` resolves relative shell paths. Empty for calls that read no code.
pub fn classify_tool_use(name: &str, input: &serde_json::Value, cwd: &Path) -> Vec<Event> {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str());
    if name.starts_with("mcp__") && name.contains("cona") {
        return vec![Event::Cona];
    }
    match name {
        "Read" => match s("file_path") {
            Some(_) if input.get("offset").is_some() || input.get("limit").is_some() => {
                vec![Event::PartialRead]
            }
            Some(p) => vec![Event::FullRead(p.to_string())],
            None => vec![],
        },
        "Grep" => vec![Event::Grep],
        "Bash" => s("command").map_or_else(Vec::new, |c| classify_line(c, cwd)),
        _ => vec![],
    }
}

/// One event per segment that reads code. A piped segment filters the
/// previous one's output (`cona show X | sed -n 1,40p`), not a file.
fn classify_line(cmd: &str, cwd: &Path) -> Vec<Event> {
    let Some(segments) = split_pipeline(cmd) else {
        return vec![];
    };
    segments
        .iter()
        .filter(|(_, piped)| !piped)
        .filter_map(|(seg, _)| {
            let prog = seg.split_whitespace().next()?;
            if prog == "cona" || prog.ends_with("/cona") {
                return Some(Event::Cona);
            }
            match classify_command(seg) {
                ShellIntent::Read { path, upto: None } => Some(Event::FullRead(
                    cwd.join(path).to_string_lossy().into_owned(),
                )),
                ShellIntent::Read { .. }
                | ShellIntent::Slice { .. }
                | ShellIntent::PartialRead { path: Some(_) } => Some(Event::PartialRead),
                ShellIntent::Grep { .. } => Some(Event::Grep),
                _ => None,
            }
        })
        .collect()
}

/// Concatenated text of a tool_result's `content` (string or block list).
fn result_chars(content: &serde_json::Value) -> usize {
    match content {
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
            .map(str::len)
            .sum(),
        _ => 0,
    }
}

/// Events of one transcript, each with the token size of its result.
pub fn scan_transcript(text: &str) -> Vec<(Event, i64)> {
    let mut pending: HashMap<String, Vec<Event>> = HashMap::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let cwd = PathBuf::from(v.get("cwd").and_then(|c| c.as_str()).unwrap_or(""));
        let Some(items) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for it in items {
            match it.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => {
                    let (Some(id), Some(name)) = (
                        it.get("id").and_then(|x| x.as_str()),
                        it.get("name").and_then(|x| x.as_str()),
                    ) else {
                        continue;
                    };
                    let input = it.get("input").cloned().unwrap_or_default();
                    let ev = classify_tool_use(name, &input, &cwd);
                    if !ev.is_empty() {
                        pending.insert(id.to_string(), ev);
                    }
                }
                Some("tool_result") => {
                    let Some(id) = it.get("tool_use_id").and_then(|x| x.as_str()) else {
                        continue;
                    };
                    if let Some(events) = pending.remove(id) {
                        // A denied/failed call returned an error, not the file.
                        let failed = it.get("is_error").and_then(|b| b.as_bool()) == Some(true);
                        // a compound line's ONE output is charged to its heaviest
                        // event (cona's share is unknowable, reads dominate)
                        let toks = db::est_tokens(result_chars(&it["content"]));
                        let weight = |e: &Event| match e {
                            Event::FullRead(_) => 3,
                            Event::Grep => 2,
                            Event::PartialRead => 1,
                            Event::Cona => 0,
                        };
                        let heaviest = (0..events.len()).max_by_key(|&i| weight(&events[i]));
                        for (i, e) in events.into_iter().enumerate() {
                            let t = if Some(i) == heaviest && e != Event::Cona {
                                toks
                            } else {
                                0
                            };
                            out.push(match (failed, e) {
                                (true, Event::FullRead(_)) => (Event::PartialRead, 0),
                                (_, e) => (e, t),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Claude Code's directory name for a project: every non-alphanumeric
/// character of the absolute path becomes `-`.
pub fn claude_project_dir_name(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Transcript files (incl. subagent transcripts) modified within `days`.
fn transcripts(root: &Path, days: i64) -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return vec![];
    };
    let dir = home
        .join(".claude/projects")
        .join(claude_project_dir_name(root));
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs((days.max(0) as u64) * 86_400);
    let mut found = Vec::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "jsonl")
                && e.metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t >= cutoff)
            {
                found.push(p);
            }
        }
    }
    found
}

/// What cona would have spent instead of a whole-file read of `rel`: outline
/// plus one median-sized symbol. `None` (nothing claimed) when the file is
/// not indexed code with symbols.
fn cona_estimate(root: &Path, conn: &Connection, rel: &str) -> Option<i64> {
    let lang: String = conn
        .query_row("SELECT lang FROM files WHERE path = ?1", [rel], |r| {
            r.get(0)
        })
        .ok()?;
    if !lang::has_callable_symbols(&lang) {
        return None;
    }
    let mut stmt = conn
        .prepare(
            "SELECT s.kind, s.qualified, s.start_line, s.end_line
             FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.path = ?1",
        )
        .ok()?;
    let rows: Vec<(String, String, i64, i64)> = stmt
        .query_map([rel], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .ok()?
        .flatten()
        .collect();
    if rows.is_empty() {
        return None;
    }
    let src = std::fs::read_to_string(root.join(rel)).ok()?;
    let lines = src.lines().count().max(1) as i64;
    let per_line = src.len() as i64 / lines;
    // outline prints `kind name :start-end` per symbol
    let outline: i64 = rows
        .iter()
        .map(|(k, q, ..)| db::est_tokens(k.len() + q.len() + 12))
        .sum();
    let mut spans: Vec<i64> = rows.iter().map(|(.., s, e)| e - s + 1).collect();
    spans.sort_unstable();
    let median = spans[spans.len() / 2];
    Some(outline + db::est_tokens((median * (per_line + 6)) as usize))
}

#[derive(Default)]
struct FileTally {
    reads: i64,
    tokens: i64,
    cona: i64,
}

pub fn cmd_discover(
    root: &Path,
    conn: &Connection,
    days: i64,
    limit: usize,
    json: bool,
) -> Result<String> {
    let files = transcripts(root, days);
    let (mut cona, mut partial, mut grep, mut grep_tok) = (0i64, 0i64, 0i64, 0i64);
    let (mut full_other, mut full_other_tok) = (0i64, 0i64);
    let mut per_file: HashMap<String, FileTally> = HashMap::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        for (e, toks) in scan_transcript(&text) {
            match e {
                Event::Cona => cona += 1,
                Event::PartialRead => partial += 1,
                Event::Grep => {
                    grep += 1;
                    grep_tok += toks;
                }
                // "file unchanged since last read" stubs put nothing in context
                Event::FullRead(_) if toks == 0 => {}
                Event::FullRead(p) => {
                    let rel = Path::new(&p)
                        .strip_prefix(root)
                        .map(|r| r.to_string_lossy().into_owned())
                        .unwrap_or(p);
                    match cona_estimate(root, conn, &rel) {
                        Some(est) => {
                            let t = per_file.entry(rel).or_default();
                            t.reads += 1;
                            t.tokens += toks;
                            t.cona += est.min(toks);
                        }
                        None => {
                            full_other += 1;
                            full_other_tok += toks;
                        }
                    }
                }
            }
        }
    }
    let code_reads: i64 = per_file.values().map(|t| t.reads).sum();
    let code_tok: i64 = per_file.values().map(|t| t.tokens).sum();
    let cona_tok: i64 = per_file.values().map(|t| t.cona).sum();
    let avoidable = (code_tok - cona_tok).max(0);
    let lookups = cona + code_reads + partial + grep;
    let adoption = if lookups > 0 { cona * 100 / lookups } else { 0 };
    let mut top: Vec<(String, FileTally)> = per_file.into_iter().collect();
    top.sort_by_key(|t| std::cmp::Reverse(t.1.tokens));
    top.truncate(limit);

    if json {
        let tops: Vec<_> = top
            .iter()
            .map(|(p, t)| serde_json::json!({"file": p, "reads": t.reads, "tokens": t.tokens, "cona_tokens": t.cona}))
            .collect();
        return Ok(format!(
            "{}\n",
            serde_json::json!({
                "transcripts": files.len(), "days": days, "cona_calls": cona,
                "adoption_pct": adoption, "full_code_reads": code_reads,
                "full_code_read_tokens": code_tok, "cona_estimate_tokens": cona_tok,
                "avoidable_tokens": avoidable, "partial_reads": partial,
                "greps": grep, "grep_tokens": grep_tok,
                "other_full_reads": full_other, "other_full_read_tokens": full_other_tok,
                "top_files": tops,
            })
        ));
    }

    let mut out = format!("── discover · this project · last {days}d ──\n");
    if files.is_empty() {
        out.push_str(&format!(
            "  no Claude Code transcripts found under ~/.claude/projects/{}\n",
            claude_project_dir_name(root)
        ));
        return Ok(out);
    }
    out.push_str(&format!(
        "  {} transcripts · {lookups} code lookups · {cona} via cona ({adoption}% adoption)\n",
        files.len()
    ));
    out.push_str(&format!(
        "  whole-file reads of indexed code  {code_reads:>5}×  {code_tok:>9} tok → cona ≈{cona_tok} · ~{avoidable} avoidable\n"
    ));
    out.push_str(&format!(
        "  partial reads                     {partial:>5}×  (not claimed)\n"
    ));
    out.push_str(&format!(
        "  greps (Grep/rg/grep)              {grep:>5}×  {grep_tok:>9} tok (not claimed)\n"
    ));
    if full_other > 0 {
        out.push_str(&format!(
            "  other whole-file reads            {full_other:>5}×  {full_other_tok:>9} tok (docs/config/unindexed — fine)\n"
        ));
    }
    if !top.is_empty() {
        out.push_str(
            "  most re-read code files (→ `cona outline <file>`, then `cona show <Symbol>`):\n",
        );
        for (p, t) in &top {
            out.push_str(&format!(
                "    {:>3}×  {p}  {} tok → ≈{}\n",
                t.reads, t.tokens, t.cona
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_reads_greps_and_cona() {
        let cwd = Path::new("/p");
        let ev = |name: &str, input| classify_tool_use(name, &input, cwd);
        assert_eq!(
            ev("Read", json!({"file_path": "/p/a.rs"})),
            [Event::FullRead("/p/a.rs".into())]
        );
        assert_eq!(
            ev("Read", json!({"file_path": "/p/a.rs", "limit": 20})),
            [Event::PartialRead]
        );
        assert_eq!(ev("Grep", json!({"pattern": "x"})), [Event::Grep]);
        assert_eq!(ev("mcp__plugin_cona_cona__show", json!({})), [Event::Cona]);
        assert_eq!(
            ev("Bash", json!({"command": "cat src/a.rs"})),
            [Event::FullRead("/p/src/a.rs".into())]
        );
        // compound line: each segment counts; a pipe filters, it reads no file
        assert_eq!(
            ev(
                "Bash",
                json!({"command": "sed -n 5,40p a.rs; cona show X | grep -n y"})
            ),
            [Event::PartialRead, Event::Cona]
        );
        assert!(ev("Bash", json!({"command": "cargo test"})).is_empty());
    }

    #[test]
    fn scan_pairs_results_and_drops_errors() {
        let t = [
            json!({"cwd": "/p", "message": {"content": [
                {"type": "tool_use", "id": "1", "name": "Read", "input": {"file_path": "/p/a.rs"}},
                {"type": "tool_use", "id": "2", "name": "Read", "input": {"file_path": "/p/b.rs"}}]}}),
            json!({"message": {"content": [
                {"type": "tool_result", "tool_use_id": "1", "content": "x".repeat(400)},
                {"type": "tool_result", "tool_use_id": "2", "is_error": true, "content": "denied"}]}}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        let ev = scan_transcript(&t);
        assert_eq!(ev[0], (Event::FullRead("/p/a.rs".into()), 100));
        // a denied read never put the file in context
        assert_eq!(ev[1], (Event::PartialRead, 0));
    }

    #[test]
    fn claude_dir_encoding() {
        assert_eq!(
            claude_project_dir_name(Path::new("/Users/me/dev/my.app")),
            "-Users-me-dev-my-app"
        );
    }
}
