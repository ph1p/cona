//! The install/uninstall executor: guide + skill + hooks + subagent patches
//! + MCP registration per agent, marker-based and idempotent.

use super::registry::*;
use super::select::AgentSel;
use super::*;
use crate::hook::PRETOOL_MATCHER;
use crate::install::mcp_config;
use crate::install::{mark, remove_block_file, upsert_block_file, write_if_changed, SKILL_MD};
use crate::ui;
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

/// How deep a `.claude/agents` tree is walked. Shipped collections nest one
/// level (`engineering/backend.md`); the cap keeps a stray checkout or symlink
/// loop under `.claude/agents` from turning the walk unbounded.
pub(super) const SUBAGENT_MAX_DEPTH: usize = 4;

/// Every `.md` under a `.claude/agents` tree. THE subagent enumeration rule —
/// `sync_subagents` and `project_has_cona` both consume it, so "definitions nest
/// in category subdirectories" is encoded ONCE (a flat `read_dir` sees none of
/// them). Fail-open: an unreadable directory yields nothing rather than aborting
/// a whole install. Does not follow symlinks, and stops at
/// `SUBAGENT_MAX_DEPTH`.
pub(super) fn subagent_defs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth >= SUBAGENT_MAX_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        // file_type() reads the dir entry (no extra stat) and does NOT follow
        // symlinks — a link back into the tree can't make the walk recurse.
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
/// carrying the marker, so previously-patched files stay reachable even if their
/// frontmatter changed.
pub(super) fn sync_subagents(
    dir: &Path,
    install: bool,
    done: &mut Vec<crate::install::Mark>,
) -> Result<()> {
    let mut paths = Vec::new();
    subagent_defs(dir, 0, &mut paths);
    for path in paths {
        if install {
            // ONE read per file: the frontmatter gate and the splice share it.
            // Going through upsert_block_file would re-read every definition.
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

/// Register (or remove) cona as an MCP server for one agent+scope, if that
/// combination has a config we own. THE one place the two config shapes are
/// chosen between. Fail-soft: a broken foreign config warns and leaves the rest
/// of the install intact — losing the MCP entry must never cost the user the
/// guide + hooks.
/// Prune directories that only ever existed to hold the file just removed,
/// walking up from it and stopping at `stop` (the project root or `$HOME`).
///
/// `remove_dir` — never `remove_dir_all` — is what makes this safe: it fails on
/// a non-empty directory, so a dir the user also keeps things in survives, and
/// the walk ends at the first one that does. Without it an uninstall leaves a
/// trail of empty `.cursor/rules`, `.windsurf/rules`, `.github` skeletons in a
/// project that had none of them before cona, which reads as leftover state.
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

pub(super) fn mcp_register(
    agent: AgentName,
    ctx: &Ctx,
    install: bool,
    done: &mut Vec<crate::install::Mark>,
) {
    let Some(path) = agent.mcp_path(ctx.project_root, ctx.home, ctx.global) else {
        return;
    };
    // The plugin registers cona's MCP server itself; a project .mcp.json entry
    // on top would offer every session the same server twice.
    if agent == AgentName::Claude && ctx.claude_plugin {
        mark(done, "mcp server", "skipped (plugin has it)", &path);
        return;
    }
    // Only create a harness's config directory when that harness is really
    // there; installing into a project scope shouldn't conjure a `.cursor/` or
    // `.gemini/` tree the user never had. `.mcp.json` sits at the project root,
    // which always exists.
    let dir_ok = path
        .parent()
        .is_some_and(|d| d.exists() || d == ctx.project_root);
    if install && !dir_ok {
        // Say so instead of vanishing: a user who expected the MCP server
        // registered otherwise has no clue why doctor lists nothing.
        mark(done, "mcp server", "skipped (no config dir)", &path);
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
        Ok(crate::install::Change::Unchanged) => {}
        Ok(_) => {
            // Uninstall deletes a config that held only our server, which can
            // leave the harness dir `dir_ok` respected on the way in (`.cursor/`)
            // standing empty.
            let anchor: &Path = if ctx.global {
                ctx.home
            } else {
                ctx.project_root
            };
            prune_empty_dirs(&path, anchor);
            mark(done, label, "removed", &path);
        }
        Err(e) => println!("{}", ui::warn(&format!("mcp: {e}"))),
    }
}

/// The per-invocation constants the MCP loop carries. `exe` in particular is
/// resolved ONCE here rather than per agent — `agent_exe()` reads global.db.
pub(super) struct Ctx<'a> {
    project_root: &'a Path,
    home: &'a Path,
    global: bool,
    exe: String,
    /// This run must skip the Claude pieces the enabled plugin already ships
    /// (false on uninstall, so a plugin-unaware leftover still gets removed).
    claude_plugin: bool,
}

/// `cona agents install|uninstall [names…] [--all] [--global]`
/// Injects/removes cona into the selected agent configs. With no names and
/// no `--all`, installs into every detected agent (Claude Code + AGENTS.md are
/// always configured; the rest are gated on detection).
pub fn cmd_agents(
    project_root: &Path,
    action: &str,
    names: &[AgentName],
    all: bool,
    global: bool,
) -> Result<bool> {
    cmd_agents_q(project_root, action, names, all, global, false)
}

/// `quiet` suppresses the per-file/summary output and prints nothing when every
/// target is already current — used by the auto-refresh paths that run without
/// the user explicitly asking. A real change still emits a one-line restart note.
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
    // How far up `prune_empty_dirs` may climb after deleting a file: never out
    // of the scope this run was asked to touch.
    let scope_root: &Path = if global { &home } else { project_root };

    let sel = AgentSel {
        names: names.to_vec(),
        all,
        install,
    };
    // The Claude Code plugin ships hooks + skill + MCP itself; with it enabled,
    // writing them again just makes every session fire each hook twice and
    // inject the SessionStart context twice. Install skips those pieces (marked
    // "skipped"), uninstall still removes what a plugin-unaware install left
    // behind. Guide files and subagent patches stay ours — the plugin carries
    // neither.
    let claude_plugin = install && claude_plugin_enabled(project_root, &home);
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
        if claude_plugin {
            mark(&mut done, "claude skill", "skipped (plugin has it)", &skill);
        } else if install {
            let ch = write_if_changed(&skill, SKILL_MD)?;
            mark(&mut done, "claude skill", ch.verb(), &skill);
        } else if skill.exists() {
            std::fs::remove_file(&skill)?;
            prune_empty_dirs(&skill, scope_root);
            mark(&mut done, "claude skill", "removed", &skill);
        }
        // CLAUDE.md — global installs keep the guide in its own CONA.md
        // (RTK-style) and only reference it; project installs stay inline so the
        // checked-in CLAUDE.md is self-contained.
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
        if claude_plugin {
            mark(
                &mut done,
                "claude hooks",
                "skipped (plugin has them)",
                &settings,
            );
        } else {
            match claude_hooks(&settings, install) {
                Ok(changed) => {
                    if install {
                        mark(
                            &mut done,
                            "claude hooks",
                            if changed { "updated" } else { "unchanged" },
                            &settings,
                        );
                    } else if changed {
                        // A settings.json that held only our hooks is deleted by
                        // `claude_hooks`, which can leave `.claude/` empty in a
                        // project that had no Claude config before cona.
                        prune_empty_dirs(&settings, scope_root);
                        mark(&mut done, "claude hooks", "removed", &settings);
                    }
                }
                Err(e) => println!("warning: could not edit {}: {e}", settings.display()),
            }
        }
        // subagents — they run on their own system prompt and don't reliably see
        // CLAUDE.md, so each existing definition carries the guide itself (never
        // creates agent files).
        sync_subagents(&claude_dir.join("agents"), install, &mut done)?;
    } // 'claude

    // --- guide-file harnesses ---------------------------------------------
    // Every agent but Claude (whose skill/hooks/subagents block sits above)
    // reads one guide file per scope, so none needs a hand-written block:
    // `config_paths` already IS the per-scope target list, and its `Presence`
    // tag says how the file is written — `Marker` = splice a block into a file
    // the user also owns, `Exists` = the file is ours alone (content from
    // `guide_body`, which lets Cursor carry its .mdc frontmatter). Driving all
    // of them from that ONE list keeps the writer and the installed()/uninstall
    // probe from ever disagreeing about which file an agent owns.
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
                        // `remove_block_file` deletes a file that held nothing
                        // but our block, which can empty a dir the install
                        // created (`.github` for Copilot).
                        prune_empty_dirs(&path, scope_root);
                        mark(&mut done, label, "removed", &path);
                    }
                }
            }
        }
    }

    // --- MCP server ----------------------------------------------------------
    // Native tools alongside the shell-out guides. Driven by ONE loop over the
    // exhaustive `mcp_path` match rather than a call per agent block: a new
    // agent then gets its MCP entry from that arm alone, and cannot end up with
    // a path that `installed()` counts but nothing ever writes or strips.
    for a in AgentName::ALL {
        if sel.want(a, a.detected(project_root, &home, global)) {
            mcp_register(a, &ctx, install, &mut done);
        }
    }

    if done.is_empty() {
        if !quiet {
            println!("{}", ui::dim("nothing to do"));
        }
        return Ok(false);
    }
    // Did anything actually move? Read the per-mark data — no text scanning.
    let changed = done.iter().any(|d| d.changed());
    // Quiet auto-refresh stays fully silent unless (and even when) something
    // moved: it runs on the query hot path, so it must never print.
    if quiet {
        return Ok(changed);
    }
    // Print what MOVED, one line each; collapse the already-current ones into a
    // per-label tally. A big ~/.claude/agents tree yields 100+ "unchanged"
    // subagent lines, which scroll the real result off the screen — the user
    // needs to see what this run did, not an inventory of what it touched.
    let (moved, same): (Vec<_>, Vec<_>) = done.iter().partition(|d| d.changed());
    for d in &moved {
        println!("{}", d.render());
    }
    if !same.is_empty() {
        // Linear scan, not a map: the label set is closed (≤ 8 values) and
        // first-seen order matches the order the targets were touched, which a
        // hash/btree map would replace with an arbitrary/alphabetical one.
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
    // Only relevant when a Claude hook/skill actually moved — no reason to nag
    // about a restart for a Cursor/Gemini-only edit. Reads the raw label field,
    // never the colored/padded rendered line.
    let claude_moved = done
        .iter()
        .any(|d| d.changed() && d.label.starts_with("claude"));
    if install && claude_moved {
        // Claude Code snapshots hooks + skills at startup for security, so a
        // running session won't see fresh changes until it reloads them.
        println!(
            "{}",
            ui::dim(
                "note: restart Claude Code (or run /hooks) so the new hooks + skill are \
                 picked up — they are snapshotted at session start"
            )
        );
    }
    Ok(true)
}

/// Cheap read-only probe: does this project carry ANY cona agent
/// integration? Used by uninstall to skip registered-but-clean projects
/// (which would otherwise each print an empty heading + "nothing to do").
/// Mirrors the project-scoped removal targets in `cmd_agents`.
pub fn project_has_cona(project_root: &Path) -> bool {
    // The per-agent probe is THE source of which files carry an install; reuse
    // it (project scope never reads home, so passing project_root as `home` is
    // inert). Claude's footprint includes the subagent-defs probe, so a scope
    // whose only trace is a marked nested subagent definition still counts.
    AgentName::ALL
        .iter()
        .any(|a| a.installed(project_root, project_root, false))
}

/// Add/remove cona hooks in a Claude Code settings.json.
/// Returns Ok(true) if the file was changed.
pub(super) fn claude_hooks(settings_path: &Path, install: bool) -> Result<bool> {
    let existing = std::fs::read_to_string(settings_path).unwrap_or_else(|_| "{}".into());
    let mut root: serde_json::Value = serde_json::from_str(&existing).map_err(|e| {
        anyhow!("existing settings.json is not valid JSON ({e}) — fix it or add the hook manually")
    })?;
    if !root.is_object() {
        bail!("settings.json top level is not an object");
    }
    // quoted: these commands run through a shell, and an install path with
    // spaces would otherwise break every hook invocation
    let exe = crate::install::sh_quote(&agent_exe());
    let index_cmd = format!("{exe} index --quiet");
    // SessionStart also emits a repo-orientation context block (see
    // main.rs session_start_context). Distinct command, but its marker stays
    // the shared "index --quiet" substring so reconcile/uninstall still match
    // it (and self-heal an older plain `index --quiet` SessionStart entry to
    // this one on reinstall).
    let session_cmd = format!("{exe} index --quiet --session-start");
    let pretool_cmd = format!("{exe} hook PreToolUse");
    // Shell-gated: the re-nudge is off by default (see DEFAULT_RENUDGE_EVERY in
    // hook.rs), and this entry fires on EVERY tool call — without the gate each
    // call would fork the cona binary just to exit at the disabled check. The
    // `[ … -gt 0 ]` test keeps the disabled path binary-free while the env var
    // alone still opts in (no reinstall). `|| :` keeps it fail-open.
    let posttool_cmd = format!(
        "[ \"${{CONA_RENUDGE_EVERY:-0}}\" -gt 0 ] 2>/dev/null && {exe} hook PostToolUse || :"
    );
    // (event, matcher, command, marker that identifies our entry)
    let specs: [(&str, Option<&str>, &str, &str); 4] = [
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
        // periodic re-nudge: registered even though it's off by default (see
        // posttool_cmd above — the shell gate makes the disabled path free).
        // Distinct marker from the index PostToolUse entry above, so both
        // coexist.
        ("PostToolUse", None, &posttool_cmd, "hook PostToolUse"),
    ];
    let mut changed = false;
    // Uninstall never CREATES structure — only an install may. Without this an
    // uninstall on a settings.json that has no cona entries would materialize
    // `"hooks": {}` plus an empty array per event and leave that husk behind.
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
    // Which event arrays were ALREADY empty before we touched anything. The
    // uninstall sweep below removes empty arrays as husks of our own hooks, but
    // an array that arrived empty is the user's — deleting it (and, when it was
    // the only key, the whole file with it) would be us editing foreign config.
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
        // Same rule per event: an install may add the array, an uninstall only
        // ever edits one that is already there.
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
    // Uninstall leaves no husk: an event array we emptied goes, and so does
    // `hooks` if that was all it held. Only arrays/objects that are now empty
    // are touched — a foreign hook keeps its event alive.
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
        // A settings.json that only ever held our hooks is ours to remove —
        // leaving `{}` behind is litter, not preservation.
        if !install && root.as_object().map(|o| o.is_empty()).unwrap_or(false) {
            let _ = std::fs::remove_file(settings_path);
            return Ok(true);
        }
        if let Some(dir) = settings_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            settings_path,
            format!("{}\n", serde_json::to_string_pretty(&root)?),
        )?;
    }
    Ok(changed)
}
