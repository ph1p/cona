//! MCP server glue: tool schemas + dispatch to the cmd_* functions.
//! Framing lives in mcp.rs (tested); this side only defines the tool
//! schemas and routes tools/call to the command implementations.

use super::*;
use crate::mcp::{
    read_only, rows_schema, tool_annotated as mcp_tool, with_output_schema, writes, ToolOut,
};
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// Wrap a text result with the structured form parsed from the same command's
/// `--json` render, under `field`.
///
/// `json_out` is what the cmd_* function produced with `json = true`: a JSON
/// array (or object) as a string. It is parsed rather than re-queried so the
/// structured payload and the text can never describe different index states.
/// A parse failure degrades to text-only instead of failing the call — a
/// missing `structuredContent` is a lost optimisation, a failed tool call is a
/// lost answer.
fn structured(text: String, json_out: &str, field: &str) -> ToolOut {
    match serde_json::from_str::<serde_json::Value>(json_out) {
        Ok(v) => ToolOut::structured(text, serde_json::json!({field: v})),
        Err(_) => ToolOut::text(text),
    }
}

/// Names of the always-exposed core tools, in tools/list order. The rest are
/// reachable only after `more` discloses them (see `mcp_tools`).
pub const CORE_TOOLS: &[&str] = &[
    "find", "show", "refs", "outline", "tree", "grep", "context", "edit",
];

/// Every tool schema cona defines, core and extended alike. `mcp_tools`
/// filters this for tools/list; `more` renders the extended tail from it, so
/// the two can never disagree about a schema.
fn all_tools() -> Vec<serde_json::Value> {
    use serde_json::json;
    let s = |d: &str| json!({"type": "string", "description": d});
    vec![
        with_output_schema(
            mcp_tool(
                "find",
                "Locate a symbol by name: file, line range, signature. Use instead of grepping for a definition",
                json!({"name": s("symbol name (exact, then substring; case-insensitive)"), "kind": s("filter: fn, struct, class, method, …"), "path": s("only symbols in files under this prefix (file or directory)"), "limit": {"type": "integer", "description": "max results (default 25)"}}),
                &["name"],
                read_only("Find symbol"),
            ),
            rows_schema(
                "symbols",
                json!({"file": s("path relative to project root"), "kind": s("symbol kind"), "symbol": s("qualified name"), "start": {"type": "integer"}, "end": {"type": "integer"}, "sig": s("signature")}),
                "matching symbols, best match first",
            ),
        ),
        mcp_tool(
            "show",
            "Print the source of one or more symbols (Name, Parent.Name or file.rs:Name). Use instead of reading a whole file to see one function/type. A file path prints that file's outline",
            json!({"symbol": s("symbol name — comma-separate several to batch; a file path yields its outline"), "kind": s("narrow to a kind (fn, struct, …)"), "context": json!({"type": "integer", "description": "extra context lines around the body (default 0)"}), "sig": json!({"type": "boolean", "description": "signature line only, no body — leanest peek"}), "all": json!({"type": "boolean", "description": "on an ambiguous name, print every candidate instead of erroring"})}),
            &["symbol"],
            read_only("Show symbol source"),
        ),
        with_output_schema(
            mcp_tool(
                "refs",
                "Usage sites of a name as file:line (semantic — strings/comments don't match). Use instead of grepping for callers/usages",
                json!({"name": s("identifier name"), "path": s("only references in files under this prefix (file or directory)"), "limit": {"type": "integer", "description": "max results (default 100)"}}),
                &["name"],
                read_only("Find references"),
            ),
            rows_schema(
                "refs",
                json!({"file": s("path relative to project root"), "line": {"type": "integer"}, "text": s("the source line")}),
                "usage sites",
            ),
        ),
        with_output_schema(
            mcp_tool(
                "outline",
                "All symbols of one file with line ranges. Use instead of reading a file to see what's in it, then show the one you need",
                json!({"file": s("path relative to the project root"), "sig": {"type": "boolean", "description": "include full signatures (default: names + ranges only)"}}),
                &["file"],
                read_only("Outline file"),
            ),
            rows_schema(
                "symbols",
                json!({"file": s("path relative to project root"), "kind": s("symbol kind"), "symbol": s("qualified name"), "start": {"type": "integer"}, "end": {"type": "integer"}, "sig": s("signature"), "stale": {"type": "boolean", "description": "index line range may be out of date"}}),
                "symbols in file order",
            ),
        ),
        mcp_tool(
            "tree",
            "Compact tree of files and top-level symbols (rank: by reference fan-in). Use to orient in an unknown codebase instead of listing/reading files",
            json!({"path": s("only files under this prefix"), "rank": {"type": "boolean", "description": "rank symbols by fan-in"}, "budget": {"type": "integer", "description": "output token budget (default 2000)"}}),
            &[],
            read_only("Project tree"),
        ),
        mcp_tool(
            "grep",
            "Code-only search; hits labeled with their enclosing symbol. Use instead of ripgrep over the repo — it skips strings, comments, and non-code. Matching is LITERAL unless regex is set",
            json!({"pattern": s("substring to search — literal unless regex is true"), "ignore_case": {"type": "boolean"}, "regex": {"type": "boolean", "description": "treat pattern as a regular expression (Rust regex syntax)"}, "path": s("only search files under this prefix (file or directory)"), "limit": {"type": "integer", "description": "max hits (default 50)"}}),
            &["pattern"],
            read_only("Code grep"),
        ),
        mcp_tool(
            "context",
            "One pack: symbol source + callee signatures + call sites. Use instead of reading a symbol plus every file it touches",
            json!({"symbol": s("symbol name"), "no_tests": {"type": "boolean", "description": "hide test call sites so production callers aren't crowded out"}, "budget": {"type": "integer", "description": "output token budget (default 3000)"}}),
            &["symbol"],
            read_only("Symbol context"),
        ),
        mcp_tool(
            "diff",
            "Changed symbols vs a git ref (incl. uncommitted/untracked)",
            json!({"ref": s("git ref to compare against (default HEAD)")}),
            &[],
            read_only("Changed symbols"),
        ),
        mcp_tool(
            "edit",
            "Replace the body of a symbol; syntax-verified, rolls back on error",
            json!({"symbol": s("symbol to replace"), "code": s("replacement source code")}),
            &["symbol", "code"],
            writes("Edit symbol", true),
        ),
        mcp_tool(
            "batch_edit",
            "Apply several symbol edits in one call; each syntax-verified. Stops at the first failure (edits already applied stay) and reports which succeeded",
            json!({"edits": {"type": "array", "description": "ordered edits", "items": {"type": "object", "properties": {"symbol": s("symbol to replace"), "code": s("replacement source")}, "required": ["symbol", "code"]}}}),
            &["edits"],
            writes("Batch edit symbols", true),
        ),
        mcp_tool(
            "insert",
            "Insert new code without touching a body: next to a symbol (before/after) or at an absolute file+line; syntax-verified",
            json!({"symbol": s("anchor symbol (omit when using file+line)"), "code": s("code to insert"), "after": {"type": "boolean", "description": "insert after (default: before)"}, "file": s("target file for absolute insertion"), "line": {"type": "integer", "description": "line to insert at (0 = prepend; needs file)"}}),
            &["code"],
            writes("Insert code", false),
        ),
        mcp_tool(
            "check",
            "Syntax-check a file (tree-sitter parse only, NOT a compiler); no file = all changed vs HEAD",
            json!({"file": s("path to check (optional)")}),
            &[],
            read_only("Syntax check"),
        ),
        mcp_tool(
            "impact",
            "Blast radius before an edit: references + immediate callers + tests + recent history",
            json!({"symbol": s("symbol name")}),
            &["symbol"],
            read_only("Change impact"),
        ),
        mcp_tool(
            "callers",
            "Transitive callers of a symbol (who reaches it; name-based, ambiguity marked)",
            json!({"symbol": s("symbol name"), "depth": {"type": "integer", "description": "call-tree depth (default 2)"}}),
            &["symbol"],
            read_only("Callers"),
        ),
        mcp_tool(
            "callees",
            "Transitive callees of a symbol (what it reaches; call sites only)",
            json!({"symbol": s("symbol name"), "depth": {"type": "integer", "description": "call-tree depth (default 2)"}}),
            &["symbol"],
            read_only("Callees"),
        ),
        mcp_tool(
            "path",
            "Shortest call chain between two symbols",
            json!({"from": s("start symbol"), "to": s("target symbol"), "max_depth": {"type": "integer", "description": "max chain length (default 8)"}}),
            &["from", "to"],
            read_only("Call path"),
        ),
        mcp_tool(
            "deps",
            "File-level import graph (+ cycles, most-imported, external deps)",
            json!({"path": s("only files under this prefix (optional)")}),
            &[],
            read_only("Import graph"),
        ),
        mcp_tool(
            "shape",
            "A symbol's source + the types it references, expanded one level",
            json!({"symbol": s("symbol name"), "kind": s("narrow to a kind (optional)"), "budget": {"type": "integer", "description": "output token budget (default 2000)"}}),
            &["symbol"],
            read_only("Symbol shape"),
        ),
        mcp_tool(
            "entries",
            "Entry points: mains, exported/public API, tests",
            json!({"path": s("only files under this prefix (optional)"), "limit": {"type": "integer", "description": "max entries per section (default 40)"}}),
            &[],
            read_only("Entry points"),
        ),
        mcp_tool(
            "tests",
            "Which tests exercise a symbol (loud when none do)",
            json!({"symbol": s("symbol name")}),
            &["symbol"],
            read_only("Tests for symbol"),
        ),
        mcp_tool(
            "note",
            "Attach a persistent note to a symbol (surfaces in show/context)",
            json!({"symbol": s("symbol name"), "text": s("note text")}),
            &["symbol", "text"],
            writes("Annotate symbol", false),
        ),
    ]
}

/// Is this tool part of the always-visible core tier?
fn is_core(t: &serde_json::Value) -> bool {
    t.get("name")
        .and_then(|n| n.as_str())
        .is_some_and(|n| CORE_TOOLS.contains(&n))
}

/// The tools/list payload: the core tier plus one `more` gate.
///
/// Progressive disclosure. The full set is 21 tools ≈ 2.6k tokens of schema
/// re-sent on EVERY request, spent whether or not the agent calls a single one
/// — a real cost for a tool whose whole purpose is spending fewer tokens. The
/// core eight answer the overwhelming majority of navigation (locate, read,
/// search, orient, edit); the other thirteen are deliberate follow-ups an agent
/// reaches for only once it knows what it wants, which is exactly when it can
/// afford one extra call to `more` to fetch their schemas.
///
/// Disclosure must go through tools/list, not through prose. A client may only
/// call what tools/list returned, so describing a gated tool in some other
/// tool's output leaves it UNREACHABLE — Claude Code answers such a call with
/// "No such tool available". Hence `more` flips the connection to expanded and
/// `serve` emits notifications/tools/list_changed, after which this returns the
/// full set. `mcp_call` still dispatches on name alone, so a client that never
/// re-lists is not broken, merely unable to discover the tail.
pub fn mcp_tools(expanded: bool) -> Vec<serde_json::Value> {
    use serde_json::json;
    let all = all_tools();
    let extended: Vec<&str> = all
        .iter()
        .filter(|t| !is_core(t))
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    // Expanded: everything, and `more` is gone — it has nothing left to unlock,
    // and leaving it would invite a second no-op call.
    if expanded {
        return all;
    }
    let mut out: Vec<serde_json::Value> = all.iter().filter(|t| is_core(t)).cloned().collect();
    out.push(mcp_tool(
        "more",
        &format!(
            "Unlock cona's {} advanced analysis tools ({}). Call this once and they \
             become available as normal tools for the rest of the session.",
            extended.len(),
            extended.join(", ")
        ),
        // `props` is the properties MAP, which tool_annotated nests under
        // "properties" — a schema fragment here yields bogus property defs and
        // clients drop the whole tools/list as invalid.
        json!({}),
        &[],
        read_only("More tools"),
    ));
    out
}

/// `more`'s body: the extended tools' schemas as JSON. Returned as text
/// because MCP tool results are content blocks; the agent reads them the same
/// way it reads tools/list.
fn mcp_more() -> Result<String> {
    let extended: Vec<serde_json::Value> =
        all_tools().into_iter().filter(|t| !is_core(t)).collect();
    // Name the CLI fallback: the schemas only become callable after the
    // harness refreshes tools/list, and not every client honours
    // list_changed. The shell spelling works either way.
    Ok(format!(
        "{} advanced cona tools are now available — call any of them by name. \
         If one is rejected as unknown (your client did not refresh its tool list), \
         every one of them is also a CLI command: run `cona <tool> …` in a shell, \
         e.g. `cona impact <Symbol>` or `cona callers <Symbol>`.\n\n{}",
        extended.len(),
        serde_json::to_string_pretty(&extended)?
    ))
}

fn mcp_call(
    root: &Path,
    conn: &Connection,
    name: &str,
    args: &serde_json::Value,
) -> Result<ToolOut> {
    let t0 = Instant::now();
    let sarg = |k: &str| -> Result<&str> {
        args.get(k)
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required argument '{k}'"))
    };
    let opt = |k: &str| args.get(k).and_then(|v| v.as_str());
    let flag = |k: &str| args.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let uint = |k: &str, d: usize| {
        args.get(k)
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(d)
    };
    let int64 = |k: &str, d: i64| args.get(k).and_then(|v| v.as_i64()).unwrap_or(d);

    let (out, baseline, detail) = match name {
        "find" => {
            let n = sarg("name")?;
            let (o, b) = cmd_find(
                root,
                conn,
                n,
                opt("kind"),
                int64("limit", defaults::FIND_LIMIT),
                opt("path"),
                false,
            )?;
            (o, b, n.to_string())
        }
        "show" => {
            // CLI parity: batch several symbols (comma-separated) + --context
            let sym = sarg("symbol")?;
            let ctx = uint("context", defaults::SHOW_CONTEXT);
            let mut o = String::new();
            let mut b = 0i64;
            for (i, one) in sym
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .enumerate()
            {
                let (oo, bb) = cmd_show(
                    root,
                    conn,
                    one,
                    ShowOpts {
                        context: ctx,
                        kind: opt("kind"),
                        sig: flag("sig"),
                        all: flag("all"),
                    },
                    false,
                )?;
                if i > 0 {
                    o.push('\n');
                }
                o.push_str(&oo);
                b += bb;
            }
            (o, b, sym.to_string())
        }
        "refs" => {
            let n = sarg("name")?;
            let (o, b) = cmd_refs(
                root,
                conn,
                n,
                uint("limit", defaults::REFS_LIMIT),
                opt("path"),
                false,
            )?;
            (o, b, n.to_string())
        }
        "outline" => {
            let f = sarg("file")?;
            let sig = args.get("sig").and_then(|v| v.as_bool()).unwrap_or(false);
            let (o, b) = cmd_outline(root, conn, f, sig, false)?;
            (o, b, f.to_string())
        }
        "tree" => {
            let budget = int64("budget", defaults::TREE_BUDGET);
            let (o, b) = if flag("rank") {
                cmd_tree_rank(root, conn, budget, opt("path"), false)?
            } else {
                cmd_tree(root, conn, budget, opt("path"), false)?
            };
            (o, b, opt("path").unwrap_or("").to_string())
        }
        "grep" => {
            let p = sarg("pattern")?;
            let (o, b) = cmd_grep(
                root,
                conn,
                p,
                GrepOpts {
                    ignore_case: flag("ignore_case"),
                    regex: flag("regex"),
                    limit: uint("limit", defaults::GREP_LIMIT),
                    path: opt("path"),
                },
                false,
            )?;
            (o, b, p.to_string())
        }
        "context" => {
            let sym = sarg("symbol")?;
            let (o, b) = cmd_context(
                root,
                conn,
                sym,
                int64("budget", defaults::CONTEXT_BUDGET),
                flag("no_tests"),
                false,
            )?;
            (o, b, sym.to_string())
        }
        "diff" => {
            let r = opt("ref").unwrap_or("HEAD");
            let (o, b) = cmd_diff(root, conn, r, false)?;
            (o, b, r.to_string())
        }
        "edit" => {
            let sym = sarg("symbol")?;
            let code = sarg("code")?;
            // stdin belongs to the protocol — hand the replacement over as &str
            (
                cmd_edit_code(root, conn, sym, code, false)?,
                0,
                sym.to_string(),
            )
        }
        "batch_edit" => {
            let edits = args
                .get("edits")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("missing required argument 'edits' (array)"))?;
            let mut summary = String::new();
            let mut done = 0usize;
            for (i, e) in edits.iter().enumerate() {
                let sym = e
                    .get("symbol")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("edit #{i}: missing 'symbol'"))?;
                let code = e
                    .get("code")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("edit #{i}: missing 'code'"))?;
                match cmd_edit_code(root, conn, sym, code, false) {
                    Ok(o) => {
                        done += 1;
                        summary.push_str(&o);
                    }
                    // stop at first failure — report progress so far, don't hide it
                    Err(err) => {
                        summary.push_str(&format!(
                            "STOPPED at edit #{i} ({sym}): {err}\n{done}/{} applied\n",
                            edits.len()
                        ));
                        bail!(summary);
                    }
                }
            }
            summary.push_str(&format!("{done}/{} edits applied\n", edits.len()));
            (summary, 0, format!("{done} edits"))
        }
        "insert" => {
            let code = sarg("code")?;
            let at = opt("file").map(|f| {
                let line = args.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                (f.to_string(), line)
            });
            let label = opt("symbol").unwrap_or("--at").to_string();
            (
                cmd_insert(root, conn, opt("symbol"), flag("after"), at, code, false)?,
                0,
                label,
            )
        }
        "check" => {
            let (o, b) = cmd_check(root, conn, opt("file"), false)?;
            (o, b, opt("file").unwrap_or("*").to_string())
        }
        "impact" => {
            let sym = sarg("symbol")?;
            let (o, b) = cmd_impact(root, conn, sym, false)?;
            (o, b, sym.to_string())
        }
        "callers" => {
            let sym = sarg("symbol")?;
            let (o, b) = cmd_calls(
                root,
                conn,
                sym,
                uint("depth", defaults::CALLS_DEPTH),
                true,
                false,
            )?;
            (o, b, sym.to_string())
        }
        "callees" => {
            let sym = sarg("symbol")?;
            let (o, b) = cmd_calls(
                root,
                conn,
                sym,
                uint("depth", defaults::CALLS_DEPTH),
                false,
                false,
            )?;
            (o, b, sym.to_string())
        }
        "path" => {
            let from = sarg("from")?;
            let to = sarg("to")?;
            let (o, b) = cmd_path(
                root,
                conn,
                from,
                to,
                uint("max_depth", defaults::PATH_DEPTH),
                false,
            )?;
            (o, b, format!("{from}→{to}"))
        }
        "deps" => {
            let (o, b) = cmd_deps(root, conn, opt("path"), false)?;
            (o, b, opt("path").unwrap_or("").to_string())
        }
        "shape" => {
            let sym = sarg("symbol")?;
            let (o, b) = cmd_shape(
                root,
                conn,
                sym,
                int64("budget", defaults::SHAPE_BUDGET),
                opt("kind"),
                false,
            )?;
            (o, b, sym.to_string())
        }
        "entries" => {
            let (o, b) = cmd_entries(
                conn,
                opt("path"),
                uint("limit", defaults::ENTRIES_LIMIT),
                false,
            )?;
            (o, b, opt("path").unwrap_or("").to_string())
        }
        "tests" => {
            let sym = sarg("symbol")?;
            let (o, b) = cmd_tests(root, conn, sym, false)?;
            (o, b, sym.to_string())
        }
        "note" => {
            let sym = sarg("symbol")?;
            let text = sarg("text")?;
            let words: Vec<String> = text.split_whitespace().map(String::from).collect();
            (cmd_note(conn, Some(sym), &words, None)?, 0, sym.to_string())
        }
        // Schema disclosure, not a query: no index needed, nothing to bill a
        // baseline against.
        "more" => (mcp_more()?, 0, String::new()),
        other => bail!("unknown tool '{other}'"),
    };
    finish(root, &format!("mcp:{name}"), t0, &out, baseline, &detail);

    // Tools that declare an outputSchema must return matching
    // structuredContent. The JSON render is produced by re-running the same
    // query with json = true: the cmd_* functions return ONE string, either
    // text or JSON, so there is no single call that yields both. The repeat is
    // an indexed SQLite read against a connection already open and warm — far
    // cheaper than the tokens the agent saves by not re-parsing a text render —
    // and it is skipped entirely for tools without a schema.
    //
    // Errors here are swallowed into text-only: structuredContent is an
    // optimisation, so a hiccup on the second pass must not fail a call whose
    // answer is already in hand.
    let structured_out = |field: &str, r: Result<(String, i64)>| match r {
        Ok((j, _)) => structured(out.clone(), &j, field),
        Err(_) => ToolOut::text(out.clone()),
    };
    Ok(match name {
        // The second pass MUST read the same caller-supplied limit as the
        // first — otherwise structuredContent and the text render disagree.
        "find" => structured_out(
            "symbols",
            cmd_find(
                root,
                conn,
                sarg("name")?,
                opt("kind"),
                int64("limit", defaults::FIND_LIMIT),
                opt("path"),
                true,
            ),
        ),
        "refs" => structured_out(
            "refs",
            cmd_refs(
                root,
                conn,
                sarg("name")?,
                uint("limit", defaults::REFS_LIMIT),
                opt("path"),
                true,
            ),
        ),
        "outline" => structured_out(
            "symbols",
            cmd_outline(root, conn, sarg("file")?, flag("sig"), true),
        ),
        _ => ToolOut::text(out),
    })
}

/// Server preamble echoed in the initialize result. Clients that lack
/// cona's global guidance (i.e. anything other than Claude Code, which
/// gets CONA.md + the SessionStart hook) see only 21 flat tools with no
/// strategy; this teaches the coarse→fine workflow and the reach-for-cona
/// rule so the token savings actually materialise. Kept compact — it is
/// injected into the model's context on every session.
const MCP_INSTRUCTIONS: &str = "\
This project is indexed by cona, a symbol-level code-navigation server. \
Use these tools as your DEFAULT way to read and search code. Do NOT read whole \
files or run plain text/regex search to locate or inspect code when a cona \
tool answers the question — it returns the same answer for a fraction of the tokens.

Instead of reading a file → outline <file> (its symbols), then show <Symbol> \
for the ONE symbol you need. Instead of grep/ripgrep → grep <pattern> \
(code-only) or refs <Name> (semantic usage sites). Instead of skimming to \
understand a symbol → context <Symbol> (source + callees + call sites).

Typical flow, coarse to fine — pull the smallest slice that answers the question:
  1. tree (rank=true) — orient in an unknown codebase (symbols by reference fan-in)
  2. outline <file> — every symbol in a file with line ranges
  3. show <Symbol> — the source of exactly ONE symbol (not the whole file)
  4. context <Symbol> — that symbol plus its callee signatures and call sites

The tools listed here are the core set. Call `more` once to unlock thirteen \
advanced ones — diff, insert, batch_edit, check, impact, callers, callees, \
path, deps, shape, entries, tests, note — after which they behave like any \
other tool for the rest of the session. Before changing code: deps / callers / \
callees / path map imports and call chains; impact / tests / shape scope a \
change first. edit / batch_edit / insert are syntax-verified and roll back on \
a parse error.

Paths are relative to the project root. Only fall back to reading a whole file \
when it is not indexed or you truly need every line.";

/// `cona mcp` — serve MCP over stdio until stdin closes. One connection
/// for the whole session (WAL keeps external reindexes visible; per-command
/// freshness is locate_fresh's job as everywhere else).
pub fn cmd_mcp(root: &Path) -> Result<()> {
    // The DB is opened LAZILY, on the first tools/call — never before serve()
    // enters its loop. A stdio MCP client (e.g. Devin) expects `initialize` to
    // be answered immediately; doing the fallible/slow index work up front
    // (open_indexed may bail on a home/fs-root cwd, or block auto-indexing a
    // large tree) made the process exit or stall inside the initialize window,
    // which the client reports as "connection closed: initialize response".
    // Deferring it means initialize/tools/list always succeed; an index error
    // surfaces as an isError tool result instead of a dead connection.
    let conn: std::cell::OnceCell<Connection> = std::cell::OnceCell::new();
    crate::mcp::serve(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        mcp_tools,
        Some(MCP_INSTRUCTIONS),
        |name, args| {
            // `more` only reflects over static schemas — opening (and possibly
            // building) the index for it would make schema discovery as
            // expensive as a query, and fail in an unindexed tree.
            if name == "more" {
                return Ok(ToolOut::text(mcp_more()?).expanding());
            }
            let conn = match conn.get() {
                Some(c) => c,
                None => {
                    let c = open_indexed(root)?;
                    conn.get_or_init(|| c)
                }
            };
            mcp_call(root, conn, name, args)
        },
    )
}
