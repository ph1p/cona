//! The install/uninstall executor: guide + skill + hooks + subagent patches
//! + MCP registration per agent, marker-based and idempotent.

use super::registry::*;
use super::select::AgentSel;
use super::*;
use crate::hook::PRETOOL_MATCHER;
use crate::install::mcp_config;
use crate::install::{
    mark, mark_why, remove_block_file, upsert_block_file, write_if_changed, SKILL_MD,
};
use crate::ui;
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

/// How deep a `.claude/agents` tree is walked. Shipped collections nest one
/// level (`engineering/backend.md`); the cap keeps a stray checkout or symlink
/// loop from making the walk unbounded.
pub(super) const SUBAGENT_MAX_DEPTH: usize = 4;

/// Every `.md` under a `.claude/agents` tree. THE subagent enumeration rule —
/// `sync_subagents` and `project_has_cona` both use it, so "definitions nest in
/// category subdirectories" is encoded once. Fail-open (an unreadable dir yields
/// nothing), does not follow symlinks, stops at `SUBAGENT_MAX_DEPTH`.
pub(super) fn subagent_defs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth >= SUBAGENT_MAX_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        // file_type() does NOT follow symlinks, so a link back into the tree
        // can't make the walk recurse.
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            subagent_defs(&path, depth + 1, out);
        } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
}

/// A `.md` under `.claude/agents` is a real agent definition (YAML frontmatter),
/// not a stray README/runbook doc that happens to live in the same tree.
pub(super) fn is_agent_def(body: &str) -> bool {
    body.starts_with("---\n") || body.starts_with("---\r\n")
}

/// Splice (or strip) the guide block in every agent definition under `dir`.
/// Install only touches definitions (`is_agent_def`); uninstall cleans ANY `.md`
/// carrying the marker, even if its frontmatter has since changed.
pub(super) fn sync_subagents(
    dir: &Path,
    install: bool,
    done: &mut Vec<crate::install::Mark>,
) -> Result<()> {
    let mut paths = Vec::new();
    subagent_defs(dir, 0, &mut paths);
    for path in paths {
        if install {
            // ONE read per file, shared by the frontmatter gate and the splice
            // (upsert_block_file would re-read it).
            let Ok(existing) = std::fs::read_to_string(&path) else {
                continue;
            };
            if !is_agent_def(&existing) {
                continue;
            }
            let updated = crate::install::upsert_block(&existing, GUIDE_MD);
            if updated == existing {
                mark(done, "claude subagent", "unchanged", &path);
                continue;
            }
            std::fs::write(&path, updated)?;
            mark(done, "claude subagent", "updated", &path);
        } else if remove_block_file(&path)? {
            // Can't delete the file: install required frontmatter, so the
            // remainder after stripping our block is never empty.
            mark(done, "claude subagent", "removed", &path);
        }
    }
    Ok(())
}

/// Prune directories that only existed to hold the file just removed, walking
/// up from it and stopping at `stop` (the project root or `$HOME`).
///
/// `remove_dir` — never `remove_dir_all` — makes this safe: it fails on a
/// non-empty dir, so anything the user keeps there survives and the walk ends.
/// Without it an uninstall leaves empty `.cursor/rules`, `.windsurf/rules`,
/// `.github` skeletons behind in a project that never had them.
pub(super) fn prune_empty_dirs(file: &Path, stop: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        // Never climb past the anchor, and never remove the anchor itself.
        if d == stop || !d.starts_with(stop) || d == stop.parent().unwrap_or(stop) {
            break;
        }
        if std::fs::remove_dir(d).is_err() {
            break; // non-empty (or gone) — everything above it is too
        }
        dir = d.parent();
    }
}

/// Register (or remove) cona as an MCP server for one agent+scope, if that
/// combination has a config we own. THE one place the two config shapes are
/// chosen between. Fail-soft: a broken foreign config only warns — losing the
/// MCP entry must never cost the user the guide + hooks.
pub(super) fn mcp_register(
    agent: AgentName,
    ctx: &Ctx,
    install: bool,
    done: &mut Vec<crate::install::Mark>,
) {
    let Some(path) = agent.mcp_path(ctx.project_root, ctx.home, ctx.global) else {
        return;
    };
    // The plugin registers cona's MCP server itself; a .mcp.json entry on top
    // would expose every tool twice. So take the uninstall path: never write
    // it, and strip one a plugin-unaware install left (.mcp.json is Claude's
    // alone among our agents).
    let plugin = agent == AgentName::Claude && ctx.claude_plugin;
    let why = plugin.then_some("plugin has it");
    let install = install && !plugin;
    // Only create a harness's config dir when that harness is really there —
    // don't conjure a `.cursor/` or `.gemini/` tree. `.mcp.json` sits at the
    // project root, which always exists.
    let dir_ok = path
        .parent()
        .is_some_and(|d| d.exists() || d == ctx.project_root);
    if install && !dir_ok {
        // Say so, or the user has no clue why doctor lists no MCP server.
        mark_why(done, "mcp server", "skipped", Some("no config dir"), &path);
        return;
    }
    let is_toml = path.extension().and_then(|e| e.to_str()) == Some("toml");
    let res = if is_toml {
        mcp_config::toml_server(&path, &ctx.exe, install)
    } else {
        mcp_config::json_server_keyed(&path, &ctx.exe, install, agent.mcp_key())
    };
    let label = "mcp server";
    match res {
        Ok(ch) if install => mark(done, label, ch.verb(), &path),
        Ok(crate::install::Change::Unchanged) => {
            if plugin {
                mark_why(done, label, "skipped", why, &path);
            }
        }
        Ok(_) => {
            // Deleting a config that held only our server can leave the
            // harness dir (`.cursor/`) empty.
            let anchor: &Path = if ctx.global {
                ctx.home
            } else {
                ctx.project_root
            };
            prune_empty_dirs(&path, anchor);
            mark_why(done, label, "removed", why, &path);
        }
        Err(e) => println!("{}", ui::warn(&format!("mcp: {e}"))),
    }
}

/// Per-invocation constants for the MCP loop. `exe` is resolved ONCE, not per
/// agent — `agent_exe()` reads global.db.
pub(super) struct Ctx<'a> {
    project_root: &'a Path,
    home: &'a Path,
    global: bool,
    exe: String,
    /// This run must skip the Claude pieces the enabled plugin already ships
    /// (false on uninstall, so a plugin-unaware leftover still gets removed).
    claude_plugin: bool,
}

/// Set once the restart note has been printed this process.
static RESTART_NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `cona agents install|uninstall [names…] [--all] [--global]`
/// With no names and no `--all`, targets every detected agent (Claude Code +
/// AGENTS.md always; the rest gated on detection).
pub fn cmd_agents(
    project_root: &Path,
    action: &str,
    names: &[AgentName],
    all: bool,
    global: bool,
) -> Result<bool> {
    cmd_agents_q(project_root, action, names, all, global, false)
}

/// `quiet` suppresses all output — used by the auto-refresh paths the user did
/// not explicitly ask for.
pub fn cmd_agents_q(
    project_root: &Path,
    action: &str,
    names: &[AgentName],
    all: bool,
    global: bool,
    quiet: bool,
) -> Result<bool> {
    let install = action == "install";
    let mut done: Vec<crate::install::Mark> = Vec::new();
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    // `prune_empty_dirs` never climbs out of the scope this run touches.
    let scope_root: &Path = if global { &home } else { project_root };

    let sel = AgentSel {
        names: names.to_vec(),
        all,
        install,
    };
    // The Claude Code plugin ships hooks + skill + MCP itself; writing them
    // again makes every hook (and the SessionStart context) fire twice. So
    // install never writes those pieces and REMOVES any a plugin-unaware
    // install left — one `agents install` is the whole fix. Guide files and
    // subagent patches stay ours; the plugin carries neither.
    let claude_plugin = install && claude_plugin_enabled(project_root, &home, global);
    let ctx = Ctx {
        project_root,
        home: &home,
        global,
        exe: agent_exe(),
        claude_plugin,
    };

    // --- Claude Code -------------------------------------------------------
    // (labeled block so the guard doesn't reindent the whole section)
    'claude: {
        if !sel.want(AgentName::Claude, true) {
            break 'claude;
        }
        let claude_dir = if global {
            home.join(".claude")
        } else {
            project_root.join(".claude")
        };
        // skill
        let skill = claude_dir.join("skills/cona/SKILL.md");
        // With the plugin, skill + hooks take the uninstall path (see above).
        let why = claude_plugin.then_some("plugin has it");
        let want = install && !claude_plugin;
        if want {
            let ch = write_if_changed(&skill, SKILL_MD)?;
            mark(&mut done, "claude skill", ch.verb(), &skill);
        } else if skill.exists() {
            std::fs::remove_file(&skill)?;
            prune_empty_dirs(&skill, scope_root);
            mark_why(&mut done, "claude skill", "removed", why, &skill);
        } else if claude_plugin {
            mark_why(&mut done, "claude skill", "skipped", why, &skill);
        }
        // CLAUDE.md — global installs keep the guide in CONA.md and only
        // reference it; project installs inline it so the checked-in CLAUDE.md
        // is self-contained.
        let claude_md = if global {
            home.join(".claude/CLAUDE.md")
        } else {
            project_root.join("CLAUDE.md")
        };
        if global {
            let cona_md = home.join(".claude/CONA.md");
            if install {
                let g = write_if_changed(&cona_md, GUIDE_MD)?;
                mark(&mut done, "claude guide", g.verb(), &cona_md);
                let m = upsert_block_file(&claude_md, "@CONA.md")?;
                mark(&mut done, "claude memory", m.verb(), &claude_md);
            } else {
                if cona_md.exists() {
                    std::fs::remove_file(&cona_md)?;
                    mark(&mut done, "claude guide", "removed", &cona_md);
                }
                if remove_block_file(&claude_md)? {
                    mark(&mut done, "claude memory", "removed", &claude_md);
                }
            }
        } else if install {
            let m = upsert_block_file(&claude_md, GUIDE_MD)?;
            mark(&mut done, "claude memory", m.verb(), &claude_md);
        } else if remove_block_file(&claude_md)? {
            mark(&mut done, "claude memory", "removed", &claude_md);
        }
        // hooks in settings.json — keep the index fresh after agent edits
        let settings = claude_dir.join("settings.json");
        // "created" vs "updated" is about OUR hooks, not the file.
        let (had_index, had_read) = crate::install::doctor::settings_cona_hooks(&settings);
        match claude_hooks(&settings, want) {
            Ok(changed) => {
                if want {
                    let verb = match (changed, had_index || had_read) {
                        (false, _) => "unchanged",
                        (true, false) => "created",
                        (true, true) => "updated",
                    };
                    mark(&mut done, "claude hooks", verb, &settings);
                } else if changed {
                    // `claude_hooks` deletes a settings.json that held only
                    // our hooks, which can leave `.claude/` empty.
                    prune_empty_dirs(&settings, scope_root);
                    mark_why(&mut done, "claude hooks", "removed", why, &settings);
                } else if claude_plugin {
                    mark_why(&mut done, "claude hooks", "skipped", why, &settings);
                }
            }
            Err(e) => println!("warning: could not edit {}: {e}", settings.display()),
        }
        // subagents don't reliably see CLAUDE.md, so each existing definition
        // carries the guide itself (never creates agent files).
        sync_subagents(&claude_dir.join("agents"), install, &mut done)?;
    } // 'claude

    // --- guide-file harnesses ---------------------------------------------
    // Every agent but Claude reads one guide file per scope. `config_paths` IS
    // the per-scope target list, and its `Presence` tag says how to write it:
    // `Marker` = splice a block into a file the user also owns, `Exists` = the
    // file is ours alone (from `guide_body`, so Cursor keeps its .mdc
    // frontmatter). One list keeps the writer and the installed()/uninstall
    // probe from disagreeing about which file an agent owns.
    for a in AgentName::ALL {
        if a == AgentName::Claude {
            continue;
        }
        if !sel.want(a, a.detected(project_root, &home, global)) {
            continue;
        }
        let label = a.mark_label();
        for (path, kind) in a.config_paths(project_root, &home, global) {
            match kind {
                // Ours alone: a whole-file write, removed outright.
                Presence::Exists => {
                    if install {
                        let ch = write_if_changed(&path, &a.guide_body())?;
                        mark(&mut done, label, ch.verb(), &path);
                    } else if path.exists() {
                        std::fs::remove_file(&path)?;
                        prune_empty_dirs(&path, scope_root);
                        mark(&mut done, label, "removed", &path);
                    }
                }
                // Shared with the user: marker block only (invariant 6).
                _ => {
                    if install {
                        let ch = upsert_block_file(&path, GUIDE_MD)?;
                        mark(&mut done, label, ch.verb(), &path);
                    } else if remove_block_file(&path)? {
                        // Deleting a block-only file can empty a dir the
                        // install created (`.github` for Copilot).
                        prune_empty_dirs(&path, scope_root);
                        mark(&mut done, label, "removed", &path);
                    }
                }
            }
        }
    }

    // --- MCP server ----------------------------------------------------------
    // ONE loop over the exhaustive `mcp_path` match: a new agent gets its MCP
    // entry from that arm alone, and can't end up with a path `installed()`
    // counts but nothing writes or strips.
    for a in AgentName::ALL {
        if sel.want(a, a.detected(project_root, &home, global)) {
            mcp_register(a, &ctx, install, &mut done);
        }
    }

    if done.is_empty() {
        if !quiet {
            // Name the searched scope, or a user cleaning up home configs
            // never learns `--global` was the missing piece.
            let msg = match (install, global) {
                (false, false) => "nothing to remove in this project — home configs need --global",
                (false, true) => {
                    "nothing to remove in home configs — project configs: drop --global"
                }
                _ => "nothing to do",
            };
            println!("{}", ui::dim(msg));
        }
        return Ok(false);
    }
    let changed = done.iter().any(|d| d.changed());
    // Quiet auto-refresh runs on the query hot path, so it never prints.
    if quiet {
        return Ok(changed);
    }
    // Print what MOVED, one line each; collapse already-current ones into a
    // per-label tally (a big ~/.claude/agents tree yields 100+ "unchanged"
    // lines that would scroll the result away). Two agents can share one
    // config (`.mcp.json`): the second finds it current — drop that no-op.
    // A mark with a reason explains why nothing was written, so it keeps its
    // own line.
    let (moved, same): (Vec<_>, Vec<_>) = done
        .iter()
        .filter(|d| {
            d.changed()
                || !done
                    .iter()
                    .any(|o| o.changed() && o.label == d.label && o.path == d.path)
        })
        .partition(|d| d.changed() || d.why.is_some());
    for d in &moved {
        println!("{}", d.render());
    }
    if !same.is_empty() {
        // Linear scan, not a map: the label set is tiny (≤ 8) and this keeps
        // first-seen (touch) order.
        let mut tally: Vec<(&str, usize)> = Vec::new();
        for d in &same {
            match tally.iter_mut().find(|(l, _)| *l == d.label) {
                Some((_, n)) => *n += 1,
                None => tally.push((d.label, 1)),
            }
        }
        // One label → name it ("claude skill"); several → just the total.
        let detail = if tally.len() == 1 {
            format!("{} already current", tally[0].0)
        } else {
            let parts: Vec<String> = tally
                .iter()
                .map(|(l, n)| {
                    if *n == 1 {
                        l.to_string()
                    } else {
                        format!("{l} ×{n}")
                    }
                })
                .collect();
            format!("{} already current: {}", same.len(), parts.join(", "))
        };
        println!("{}", ui::item(&ui::dim(&detail)));
    }
    println!(
        "{}",
        ui::ok(&format!(
            "agents {} ({})",
            if install { "installed" } else { "uninstalled" },
            if global { "global" } else { "project" }
        ))
    );
    // Restart note only when a Claude piece actually moved, not for a
    // Cursor/Gemini-only edit.
    let claude_moved = done
        .iter()
        .any(|d| d.changed() && d.label.starts_with("claude"));
    // Once per process: `setup` installs project AND home scope back to back.
    if install && claude_moved && !RESTART_NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        // Claude Code snapshots hooks + skills at session start.
        println!(
            "{}",
            ui::dim(
                "note: restart Claude Code (or run /hooks) so the hook + skill changes are \
                 picked up — they are snapshotted at session start"
            )
        );
    }
    Ok(true)
}

/// Cheap read-only probe: does this project carry ANY cona agent integration?
/// Lets uninstall skip registered-but-clean projects instead of printing an
/// empty heading + "nothing to do" for each.
pub fn project_has_cona(project_root: &Path) -> bool {
    // Reuse THE per-agent probe (project scope never reads home, so passing
    // project_root as `home` is inert). Claude's probe includes nested
    // subagent definitions.
    AgentName::ALL
        .iter()
        .any(|a| a.installed(project_root, project_root, false))
}

/// Add/remove cona hooks in a Claude Code settings.json.
/// Returns Ok(true) if the file was changed.
pub(super) fn claude_hooks(settings_path: &Path, install: bool) -> Result<bool> {
    let mut root = load_settings(settings_path, "the hook")?;
    // Quoted: hooks run through a shell, and a path with spaces would break.
    let exe = crate::install::sh_quote(&agent_exe());
    let index_cmd = format!("{exe} index --quiet");
    // SessionStart also emits a repo-orientation block (main.rs
    // session_start_context). Its marker stays the shared "index --quiet"
    // substring so reconcile/uninstall match it (and self-heal an older plain
    // `index --quiet` entry on reinstall).
    let session_cmd = format!("{exe} index --quiet --session-start");
    let pretool_cmd = format!("{exe} hook PreToolUse");
    // Compaction drops injected hook context, so the SessionStart block is
    // gone mid-session — restate the rule there.
    let precompact_cmd = format!("{exe} hook PreCompact");
    // Shell-gated: the re-nudge is off by default (DEFAULT_RENUDGE_EVERY) and
    // this fires on EVERY tool call; the `[ … -gt 0 ]` test avoids forking cona
    // just to exit, while the env var alone still opts in (no reinstall).
    // `|| :` keeps it fail-open.
    let posttool_cmd = format!(
        "[ \"${{CONA_RENUDGE_EVERY:-0}}\" -gt 0 ] 2>/dev/null && {exe} hook PostToolUse || :"
    );
    // (event, matcher, command, marker that identifies our entry)
    let specs: [(&str, Option<&str>, &str, &str); 5] = [
        (
            "PostToolUse",
            Some(crate::hook::POSTTOOL_MATCHER),
            &index_cmd,
            "index --quiet",
        ),
        ("SessionStart", None, &session_cmd, "index --quiet"),
        // navigation accelerator: redirect wasteful full reads + broad
        // identifier greps toward cona
        (
            "PreToolUse",
            Some(PRETOOL_MATCHER.as_str()),
            &pretool_cmd,
            "hook PreToolUse",
        ),
        // periodic re-nudge: registered though off by default (the shell gate
        // makes that free). Distinct marker from the index entry, so both
        // coexist.
        ("PostToolUse", None, &posttool_cmd, "hook PostToolUse"),
        // re-state the navigation rule across a compaction boundary
        ("PreCompact", None, &precompact_cmd, "hook PreCompact"),
    ];
    let mut changed = false;
    // Uninstall never CREATES structure (it would leave a `"hooks": {}` husk
    // with an empty array per event).
    if !install && !root.get("hooks").map(|h| h.is_object()).unwrap_or(false) {
        return Ok(false);
    }
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        bail!("settings.json 'hooks' is not an object");
    }
    // Event arrays that were ALREADY empty. The uninstall sweep removes empty
    // arrays as husks of our hooks, but one that arrived empty is the user's —
    // deleting it would be editing foreign config.
    let preexisting_empty: Vec<String> = hooks
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, v)| v.as_array().is_some_and(|a| a.is_empty()))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();
    for (event, matcher, cmd, marker) in specs {
        let is_ours = |v: &serde_json::Value| -> bool {
            v["hooks"]
                .as_array()
                .map(|hs| {
                    hs.iter().any(|h| {
                        h["command"]
                            .as_str()
                            .map(|c| c.contains("cona") && c.contains(marker))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        };
        let events = hooks.as_object_mut().unwrap();
        // Same rule per event: only install may add the array.
        let arr = if install {
            events.entry(event).or_insert_with(|| serde_json::json!([]))
        } else {
            match events.get_mut(event) {
                Some(v) => v,
                None => continue,
            }
        };
        let Some(list) = arr.as_array_mut() else {
            continue;
        };
        let present = list.iter().position(is_ours);
        if install && present.is_none() {
            let mut entry = serde_json::json!({
                "hooks": [{"type": "command", "command": cmd}]
            });
            if let Some(m) = matcher {
                entry["matcher"] = serde_json::json!(m);
            }
            list.push(entry);
            changed = true;
        } else if install {
            // ours is present — reconcile it to the current spec so any drift
            // (matcher widened, command renamed/moved) self-heals on reinstall
            let i = present.unwrap();
            if list[i]["matcher"].as_str() != matcher {
                match matcher {
                    Some(m) => list[i]["matcher"] = serde_json::json!(m),
                    None => {
                        list[i].as_object_mut().map(|o| o.remove("matcher"));
                    }
                }
                changed = true;
            }
            if let Some(hs) = list[i]["hooks"].as_array_mut() {
                for h in hs.iter_mut().filter(|h| {
                    h["command"]
                        .as_str()
                        .map(|c| c.contains("cona") && c.contains(marker))
                        .unwrap_or(false)
                }) {
                    if h["command"].as_str() != Some(cmd) {
                        h["command"] = serde_json::json!(cmd);
                        changed = true;
                    }
                }
            }
        } else if let Some(i) = present {
            // uninstall
            list.remove(i);
            changed = true;
        }
    }
    // Uninstall leaves no husk: an event array we emptied goes, and `hooks`
    // too if now empty. A foreign hook keeps its event alive.
    if !install && changed {
        if let Some(events) = hooks.as_object_mut() {
            events.retain(|k, v| {
                !v.as_array().map(|a| a.is_empty()).unwrap_or(false)
                    || preexisting_empty.iter().any(|p| p == k)
            });
            let empty = events.is_empty();
            if empty {
                root.as_object_mut().unwrap().remove("hooks");
            }
        }
    }
    if changed {
        store_settings(settings_path, &root, install)?;
    }
    Ok(changed)
}

/// A Claude settings.json as a JSON object; a missing file reads as `{}`.
/// Invalid JSON is an error, never overwritten (invariant 6). `what` names
/// the manual fallback in the message.
fn load_settings(path: &Path, what: &str) -> Result<serde_json::Value> {
    let existing = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".into());
    let root: serde_json::Value = serde_json::from_str(&existing).map_err(|e| {
        anyhow!("existing settings.json is not valid JSON ({e}) — fix it or add {what} manually")
    })?;
    if !root.is_object() {
        bail!("settings.json top level is not an object");
    }
    Ok(root)
}

/// Write `root` back. A settings.json that an uninstall left as `{}` held only
/// ours and is removed — leaving `{}` behind is litter, not preservation.
fn store_settings(path: &Path, root: &serde_json::Value, install: bool) -> Result<()> {
    if !install && root.as_object().is_some_and(|o| o.is_empty()) {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    write_if_changed(path, &format!("{}\n", serde_json::to_string_pretty(root)?))?;
    Ok(())
}

/// Read-only cona subcommands the Bash allow rules are built from. Writers and
/// maintenance commands are deliberately absent so auto mode's classifier keeps
/// judging them. "Read-only" is toward the project: `index` writes only cona's
/// own index under ~/.cona. The MCP side derives from the tools' `readOnlyHint`
/// (`read_only_tool_names`).
#[allow(dead_code)] // until claude_permissions is wired into `agents install`
const READ_ONLY_CMDS: &[&str] = &[
    "tree", "outline", "find", "show", "refs", "context", "grep", "diff", "impact", "callers",
    "callees", "path", "deps", "shape", "entries", "tests", "check", "blame", "stats", "index",
];
/// Prefix of every `autoMode` prose rule we write — how install reconciles and
/// uninstall finds ours among the user's own rules.
#[allow(dead_code)] // until claude_permissions is wired into `agents install`
const AUTOMODE_TAG: &str = "cona —";

/// MCP namespaces a cona server shows up under: registered by `agents install`
/// (`mcp__cona__*`) and shipped by the plugin, whose name is also cona
/// (`mcp__plugin_cona_cona__*`).
#[allow(dead_code)] // until claude_permissions is wired into `agents install`
fn mcp_prefixes() -> [String; 2] {
    let s = mcp_config::SERVER_NAME;
    [format!("mcp__{s}__"), format!("mcp__plugin_{s}_{s}__")]
}

/// The `permissions.allow` rules cona owns. Narrow per-subcommand rules on
/// purpose: auto mode suspends broad ones (`Bash(*)`-shaped), narrow ones it
/// honors without a classifier round-trip.
#[allow(dead_code)] // until claude_permissions is wired into `agents install`
pub(super) fn cona_allow_rules() -> Vec<String> {
    let tools = crate::commands::mcp_server::read_only_tool_names();
    let mut v: Vec<String> = READ_ONLY_CMDS
        .iter()
        .map(|c| format!("Bash(cona {c} *)"))
        .collect();
    for p in mcp_prefixes() {
        v.extend(tools.iter().map(|t| format!("{p}{t}")));
    }
    v
}

/// `autoMode` prose for the classifier: `environment` says what cona is,
/// `allow` says which of its actions are routine. Every entry starts with
/// `AUTOMODE_TAG`; the lists in it render from the same consts as the rules.
#[allow(dead_code)] // until claude_permissions is wired into `agents install`
pub(super) fn cona_automode() -> [(&'static str, Vec<String>); 2] {
    let namespaces = mcp_prefixes().map(|p| format!("`{p}*`")).join(", ");
    [
        (
            "environment",
            vec![format!(
                "{AUTOMODE_TAG} a trusted, local, read-only code-navigation CLI (`cona`) and \
                 MCP server ({namespaces}) installed by the user. It reads source through a \
                 per-project SQLite index under ~/.cona and never touches the network or \
                 anything outside the project and ~/.cona. It is the PREFERRED way to read \
                 and search code in this environment."
            )],
        ),
        (
            // Read-only commands only — writers stay with the classifier (see
            // READ_ONLY_CMDS), so no prose here may pre-approve them either.
            "allow",
            vec![format!(
                "{AUTOMODE_TAG} running cona navigation/inspection commands ({}) and the \
                 equivalent read-only cona MCP tools is always allowed: read-only, local, no \
                 side effects beyond refreshing its own index.",
                READ_ONLY_CMDS.join(", ")
            )],
        ),
    ]
}

/// Splice `ours` into `obj[key]` (an array of strings). Strings matching
/// `is_ours` are replaced wholesale on install (self-heals reworded prose) and
/// dropped on uninstall. `seed` is prepended only when install CREATES the
/// array; an array left holding only the seed is removed on uninstall.
#[allow(dead_code)] // until claude_permissions is wired into `agents install`
fn splice_strings(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    ours: &[String],
    is_ours: impl Fn(&str) -> bool,
    seed: Option<&str>,
    install: bool,
) -> Result<bool> {
    use serde_json::json;
    if !install && !obj.contains_key(key) {
        return Ok(false);
    }
    let arr = obj
        .entry(key)
        .or_insert_with(|| json!(seed.map(|s| vec![s]).unwrap_or_default()));
    let Some(list) = arr.as_array_mut() else {
        bail!("settings.json '{key}' is not an array");
    };
    let before = list.clone();
    list.retain(|v| !v.as_str().is_some_and(&is_ours));
    if install {
        list.extend(ours.iter().map(|s| json!(s)));
    }
    let changed = *list != before;
    let husk = list.is_empty() || (list.len() == 1 && seed.is_some() && list[0].as_str() == seed);
    if !install && husk && changed {
        obj.remove(key);
    }
    Ok(changed)
}

/// Run `f` on the object at `obj[key]`, creating it on install; on uninstall
/// an object left empty is removed. A non-object is an error, never replaced.
#[allow(dead_code)] // until claude_permissions is wired into `agents install`
fn with_object(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    install: bool,
    f: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<bool>,
) -> Result<bool> {
    let Some(inner) = obj
        .entry(key)
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
    else {
        bail!("settings.json '{key}' is not an object");
    };
    let changed = f(inner)?;
    if !install && inner.is_empty() {
        obj.remove(key);
    }
    Ok(changed)
}

/// Allow rules (+ `autoMode` prose when `automode`) in a Claude settings.json.
/// `autoMode` is only read from user-level settings, so project installs pass
/// `false`. Same contract as `claude_hooks` (no foreign edits, no husks). Runs
/// even with the plugin enabled — a plugin cannot ship settings.
#[allow(dead_code)] // until claude_permissions is wired into `agents install`
pub(super) fn claude_permissions(
    settings_path: &Path,
    install: bool,
    automode: bool,
) -> Result<bool> {
    let mut root = load_settings(settings_path, "the rules")?;
    let obj = root
        .as_object_mut()
        .expect("load_settings checks for an object");
    let mut changed = false;

    if install || obj.get("permissions").is_some_and(|p| p.is_object()) {
        let rules = cona_allow_rules();
        changed |= with_object(obj, "permissions", install, |perms| {
            splice_strings(
                perms,
                "allow",
                &rules,
                |s| rules.iter().any(|r| r == s),
                None,
                install,
            )
        })?;
    }

    // autoMode (user scope only); still cleaned on any uninstall.
    if (install && automode) || (!install && obj.get("autoMode").is_some_and(|a| a.is_object())) {
        changed |= with_object(obj, "autoMode", install, |am| {
            let mut changed = false;
            for (key, prose) in cona_automode() {
                // "$defaults" keeps Claude Code's built-in rules; without it
                // our array would REPLACE them.
                let tagged = |s: &str| s.starts_with(AUTOMODE_TAG);
                changed |= splice_strings(am, key, &prose, tagged, Some("$defaults"), install)?;
            }
            Ok(changed)
        })?;
    }

    if changed {
        store_settings(settings_path, &root, install)?;
    }
    Ok(changed)
}
