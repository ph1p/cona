//! Agent integration: inject the usage guide + skill + hooks into agent
//! configs (Claude Code, AGENTS.md, Cursor, Gemini, pi.dev) —
//! idempotent, marker-based, uninstallable.

use super::{mark, remove_block_file, upsert_block_file, write_if_changed, SKILL_MD};
use crate::{db, ui};
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

/// Compact variant for CLAUDE.md / AGENTS.md / rule files.
pub const GUIDE_MD: &str = r#"## cona — token-efficient code navigation

Once a repo is cona-indexed, reading ONE symbol costs a fraction of a whole
file, and `cona grep`/`refs` search code semantically (identifier nodes — never
strings or comments). Prefer them over a full Read or a broad Grep when you want
a specific function, class, or usage site.

Coarse → fine: `cona tree --rank` (orient) → `cona outline <file>` (map a file) →
`cona show <Sym>` (read one symbol) → `cona edit <Sym>` (syntax-verified write).

`<Sym>` = `Name`, `Parent.Name`, or `file.rs:Name`. Index auto-refreshes;
`cona index` (~1s) if a repo isn't indexed yet.

Too many hits? `--path <dir>` scopes `find`/`refs`/`grep`/`tree` to a subtree.
Ambiguous name? `cona show <Sym> --all` prints every definition instead of
erroring. `cona grep` is a literal substring match, not a regex.

Everything else — `context` `impact` `diff` `deps` `callers` `tests` `blame`
`insert` `rename` `note` `check` — is listed in `cona --help`, with details per
group (`cona nav --help`, `inspect`, `code`, `history`, `project`, `maint`).
"#;

/// The `cona` invocation agents should use — absolute if we know it.
pub(crate) fn agent_exe() -> String {
    db::meta_get("install_path")
        .ok()
        .flatten()
        .filter(|p| Path::new(p).exists())
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "cona".to_string())
}

/// The agents `cmd_agents` knows how to configure. A `clap::ValueEnum`, so the
/// CLI validates names at parse time (typo → clap error + possible-values in
/// `--help`) and `--all` / `want()` derive from the SAME variant set — no
/// hand-kept string list to drift against the per-agent blocks below.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum AgentName {
    Claude,
    Agents,
    Cursor,
    Gemini,
    Pi,
}

impl AgentName {
    /// Every agent, in menu/priority order. The one place the full set lives.
    pub const ALL: [AgentName; 5] = [
        AgentName::Claude,
        AgentName::Agents,
        AgentName::Cursor,
        AgentName::Gemini,
        AgentName::Pi,
    ];

    /// CLI spelling (matches the ValueEnum variant name lower-cased).
    pub fn slug(self) -> &'static str {
        match self {
            AgentName::Claude => "claude",
            AgentName::Agents => "agents",
            AgentName::Cursor => "cursor",
            AgentName::Gemini => "gemini",
            AgentName::Pi => "pi",
        }
    }

    /// One-line description for the interactive picker.
    pub fn desc(self) -> &'static str {
        match self {
            AgentName::Claude => "Claude Code — skill + hooks + CLAUDE.md",
            AgentName::Agents => "AGENTS.md — Codex / OpenCode / Amp / Jules",
            AgentName::Cursor => "Cursor — .cursor/rules",
            AgentName::Gemini => "Gemini CLI — GEMINI.md",
            AgentName::Pi => "pi.dev — AGENTS.md",
        }
    }

    /// Is this agent's config present on disk? Claude Code + (project) AGENTS.md
    /// are always considered present — they are the unconditional core. THE one
    /// detection source, shared by cmd_agents' gating and the setup picker.
    pub fn detected(self, project_root: &Path, home: &Path, global: bool) -> bool {
        match self {
            AgentName::Claude => true,
            AgentName::Agents => {
                if global {
                    home.join(".codex").exists()
                } else {
                    true
                }
            }
            AgentName::Cursor => {
                let base = if global { home } else { project_root };
                base.join(".cursor").exists()
            }
            AgentName::Gemini => {
                if global {
                    home.join(".gemini").exists()
                } else {
                    project_root.join("GEMINI.md").exists() || project_root.join(".gemini").exists()
                }
            }
            // project scope is a no-op (project AGENTS.md is already covered
            // by the Agents bucket above) — never detected there, so it's
            // never offered/selected for a project-scope install.
            AgentName::Pi => global && home.join(".pi").exists(),
        }
    }

    /// Config files this agent's cona integration lives in for the given scope,
    /// each tagged with HOW to detect a cona install there (`Presence`). THE
    /// single source for both — `installed()` reads these tags rather than
    /// re-deriving the probe from the filename. Empty = this agent has no target
    /// in this scope (e.g. Pi / global-only Cursor at project scope).
    pub fn config_paths(
        self,
        project_root: &Path,
        home: &Path,
        global: bool,
    ) -> Vec<(PathBuf, Presence)> {
        // Where the scope's config lives: project root or home.
        let base = if global { home } else { project_root };
        match self {
            AgentName::Claude => {
                let dir = base.join(".claude");
                let md = if global {
                    home.join(".claude/CLAUDE.md")
                } else {
                    project_root.join("CLAUDE.md")
                };
                vec![
                    (dir.join("skills/cona/SKILL.md"), Presence::Exists),
                    (dir.join("settings.json"), Presence::Needle),
                    (md, Presence::Marker),
                ]
            }
            AgentName::Agents => {
                let p = if global {
                    home.join(".codex/AGENTS.md")
                } else {
                    project_root.join("AGENTS.md")
                };
                vec![(p, Presence::Marker)]
            }
            AgentName::Cursor => {
                vec![(base.join(".cursor/rules/cona.mdc"), Presence::Exists)]
            }
            AgentName::Gemini => {
                let p = if global {
                    home.join(".gemini/GEMINI.md")
                } else {
                    project_root.join("GEMINI.md")
                };
                vec![(p, Presence::Marker)]
            }
            // Pi only has its own path at global scope.
            AgentName::Pi if global => vec![(home.join(".pi/agent/AGENTS.md"), Presence::Marker)],
            AgentName::Pi => vec![],
        }
    }

    /// Is cona currently wired into this agent for the given scope? Probes each
    /// config path the way its `Presence` tag dictates — so it reflects an
    /// actual install, not mere presence of the agent (`detected`).
    pub fn installed(self, project_root: &Path, home: &Path, global: bool) -> bool {
        self.config_paths(project_root, home, global)
            .iter()
            .any(|(p, kind)| kind.present(p))
    }
}

/// How to tell a cona install is present in a config file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Full-file write (skill, cursor rule): the file simply existing = present.
    Exists,
    /// cona hooks embedded in a JSON config: a `"cona"` needle anywhere in it.
    Needle,
    /// A marker block spliced into a shared file (CLAUDE.md, AGENTS.md, …).
    Marker,
}

impl Presence {
    fn present(self, p: &Path) -> bool {
        match self {
            Presence::Exists => p.exists(),
            Presence::Needle => std::fs::read_to_string(p).is_ok_and(|c| c.contains("cona")),
            Presence::Marker => has_marker(p),
        }
    }
}

/// A file carries a cona marker block. THE shared marker probe.
fn has_marker(p: &Path) -> bool {
    std::fs::read_to_string(p).is_ok_and(|c| c.contains(super::BLOCK_BEGIN))
}

/// The agents whose config is detected on disk (used to pre-check the picker
/// and as the non-interactive autodetect set).
pub fn detected_agents(project_root: &Path, home: &Path, global: bool) -> Vec<AgentName> {
    AgentName::ALL
        .into_iter()
        .filter(|a| a.detected(project_root, home, global))
        .collect()
}

/// Agents that have a config target in `global`/project scope — the ones a
/// scope can actually act on. THE scope-eligibility rule (setup picker, the
/// interactive command, status all read it), so a scope-less agent (e.g. Pi at
/// project scope) is filtered in ONE place.
pub fn agents_in_scope(project_root: &Path, home: &Path, global: bool) -> Vec<AgentName> {
    AgentName::ALL
        .into_iter()
        .filter(|a| !a.config_paths(project_root, home, global).is_empty())
        .collect()
}

/// Which agents a given invocation targets. Encodes the selection rule in ONE
/// place: explicit names (or `--all`) override detection; with neither, an
/// agent is configured only when its config is detected on disk. Uninstall
/// runs every requested agent regardless of detection (so a leftover config
/// is always removable).
struct AgentSel {
    names: Vec<AgentName>,
    all: bool,
    install: bool,
}

impl AgentSel {
    /// Should this agent be acted on? `detected` = its config dir/file exists.
    fn want(&self, name: AgentName, detected: bool) -> bool {
        if self.all {
            return true;
        }
        if !self.names.is_empty() {
            return self.names.contains(&name);
        }
        // no explicit selection: autodetect on install; on uninstall a bare
        // call means "clean whatever is there", so detection doesn't gate it.
        !self.install || detected
    }
}

/// `cona agents status` — one glance at what is wired where. Per agent, per
/// scope: ✓ configured / – not configured / (n/a for scopes an agent lacks),
/// plus the exact copy-paste command to add or remove it. THE self-explaining
/// surface for managing single agents.
pub fn cmd_agents_status(project_root: &Path) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    println!("{}\n", ui::bold("cona agents"));

    let mut any_installed = false;
    for a in AgentName::ALL {
        let proj = a.installed(project_root, &home, false);
        let glob = a.installed(project_root, &home, true);
        any_installed |= proj || glob;
        // does this agent even have a target in each scope?
        let proj_na = a.config_paths(project_root, &home, false).is_empty();
        let glob_na = a.config_paths(project_root, &home, true).is_empty();
        let cell = |installed: bool, na: bool| {
            if na {
                ui::dim("n/a")
            } else if installed {
                ui::green("✓ on")
            } else {
                ui::dim("– off")
            }
        };
        println!("{}", ui::heading(a.slug()));
        println!("  {}", ui::dim(a.desc()));
        println!(
            "  project {}    global {}",
            cell(proj, proj_na),
            cell(glob, glob_na)
        );
        println!();
    }

    println!("{}", ui::heading("manage"));
    println!(
        "  {}",
        ui::dim("cona agents add <name>            configure one agent (this project)")
    );
    println!(
        "  {}",
        ui::dim("cona agents add <name> --global   configure one agent (home configs)")
    );
    println!(
        "  {}",
        ui::dim("cona agents remove <name>         remove one agent")
    );
    println!(
        "  {}",
        ui::dim("cona agents                       interactive checklist (toggle any)")
    );
    if !any_installed {
        println!(
            "\n{}",
            ui::warn("no agents configured yet — run `cona setup`")
        );
    }
    Ok(())
}

/// Interactive add/remove for single agents: a pre-checked checklist of every
/// known agent (checked = currently configured). Confirming installs the newly
/// checked ones and uninstalls the newly unchecked ones — the one-screen way to
/// add or remove any single agent. TTY-only; callers gate on that.
pub fn cmd_agents_interactive(project_root: &Path, global: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    // Only agents that have a target in this scope are actionable.
    let choices = agents_in_scope(project_root, &home, global);
    let before: Vec<bool> = choices
        .iter()
        .map(|a| a.installed(project_root, &home, global))
        .collect();
    let rows: Vec<ui::Row> = choices
        .iter()
        .zip(&before)
        .map(|(a, on)| ui::Row::Item(a.slug(), a.desc(), *on))
        .collect();

    let title = if global {
        "configure agents (home configs)"
    } else {
        "configure agents (this project)"
    };
    let picked = match ui::multiselect(title, &rows)? {
        Some(p) => p,
        None => {
            println!("{}", ui::dim("cancelled — nothing changed"));
            return Ok(());
        }
    };
    // `picked` = ordinals of the now-checked items (1:1 with `choices`). Diff
    // against `before`: newly on → add, newly off → remove, unchanged → skip.
    let now_on: std::collections::HashSet<usize> = picked.into_iter().collect();
    let mut to_add = Vec::new();
    let mut to_remove = Vec::new();
    for (i, (&a, &was)) in choices.iter().zip(&before).enumerate() {
        match (was, now_on.contains(&i)) {
            (false, true) => to_add.push(a),
            (true, false) => to_remove.push(a),
            _ => {}
        }
    }

    if to_add.is_empty() && to_remove.is_empty() {
        println!("{}", ui::dim("no changes"));
        return Ok(());
    }
    if !to_remove.is_empty() {
        cmd_agents(project_root, "uninstall", &to_remove, false, global)?;
    }
    if !to_add.is_empty() {
        cmd_agents(project_root, "install", &to_add, false, global)?;
    }
    Ok(())
}

/// How deep a `.claude/agents` tree is walked. Shipped collections nest one
/// level (`engineering/backend.md`); the cap keeps a stray checkout or symlink
/// loop under `.claude/agents` from turning the walk unbounded.
const SUBAGENT_MAX_DEPTH: usize = 4;

/// Every `.md` under a `.claude/agents` tree. THE subagent enumeration rule —
/// `sync_subagents` and `project_has_cona` both consume it, so "definitions nest
/// in category subdirectories" is encoded ONCE (a flat `read_dir` sees none of
/// them). Fail-open: an unreadable directory yields nothing rather than aborting
/// a whole install. Does not follow symlinks, and stops at
/// `SUBAGENT_MAX_DEPTH`.
fn subagent_defs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
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
fn is_agent_def(body: &str) -> bool {
    body.starts_with("---\n") || body.starts_with("---\r\n")
}

/// Splice (or strip) the guide block in every agent definition under `dir`.
/// Install only touches definitions (`is_agent_def`); uninstall cleans ANY `.md`
/// carrying the marker, so previously-patched files stay reachable even if their
/// frontmatter changed.
fn sync_subagents(dir: &Path, install: bool, done: &mut Vec<super::Mark>) -> Result<()> {
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
            let updated = super::upsert_block(&existing, GUIDE_MD);
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
    let mut done: Vec<super::Mark> = Vec::new();
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;

    let sel = AgentSel {
        names: names.to_vec(),
        all,
        install,
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
        if install {
            let ch = write_if_changed(&skill, SKILL_MD)?;
            mark(&mut done, "claude skill", ch.verb(), &skill);
        } else if skill.exists() {
            std::fs::remove_file(&skill)?;
            let _ = std::fs::remove_dir(skill.parent().unwrap());
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
                    mark(&mut done, "claude hooks", "removed", &settings);
                }
            }
            Err(e) => println!("warning: could not edit {}: {e}", settings.display()),
        }
        // subagents — they run on their own system prompt and don't reliably see
        // CLAUDE.md, so each existing definition carries the guide itself (never
        // creates agent files).
        sync_subagents(&claude_dir.join("agents"), install, &mut done)?;
    } // 'claude

    // --- generic AGENTS.md (Codex, OpenCode, Amp, Jules, …) ----------------
    if sel.want(
        AgentName::Agents,
        AgentName::Agents.detected(project_root, &home, global),
    ) {
        let agents_md = if global {
            home.join(".codex/AGENTS.md")
        } else {
            project_root.join("AGENTS.md")
        };
        let label = if global { "codex memory" } else { "AGENTS.md" };
        if install {
            let ch = upsert_block_file(&agents_md, GUIDE_MD)?;
            mark(&mut done, label, ch.verb(), &agents_md);
        } else if remove_block_file(&agents_md)? {
            mark(&mut done, label, "removed", &agents_md);
        }
    }

    // --- Cursor ------------------------------------------------------------
    let cursor = if global {
        home.join(".cursor/rules/cona.mdc")
    } else {
        project_root.join(".cursor/rules/cona.mdc")
    };
    if sel.want(
        AgentName::Cursor,
        AgentName::Cursor.detected(project_root, &home, global),
    ) {
        if install {
            let content = format!(
                "---\ndescription: cona — token-efficient code navigation\nalwaysApply: true\n---\n\n{GUIDE_MD}"
            );
            let ch = write_if_changed(&cursor, &content)?;
            mark(&mut done, "cursor rule", ch.verb(), &cursor);
        } else if cursor.exists() {
            std::fs::remove_file(&cursor)?;
            mark(&mut done, "cursor rule", "removed", &cursor);
        }
    }

    // --- Gemini CLI ----------------------------------------------------------
    let gemini = if global {
        home.join(".gemini/GEMINI.md")
    } else {
        project_root.join("GEMINI.md")
    };
    if sel.want(
        AgentName::Gemini,
        AgentName::Gemini.detected(project_root, &home, global),
    ) {
        if install {
            let ch = upsert_block_file(&gemini, GUIDE_MD)?;
            mark(&mut done, "gemini memory", ch.verb(), &gemini);
        } else if remove_block_file(&gemini)? {
            mark(&mut done, "gemini memory", "removed", &gemini);
        }
    }

    // --- pi.dev --------------------------------------------------------------
    // Project scope reads the project's own AGENTS.md, already handled by the
    // generic Agents bucket above — only global has a path of its own
    // (~/.pi/agent/AGENTS.md, distinct from Codex's ~/.codex/AGENTS.md).
    if global
        && sel.want(
            AgentName::Pi,
            AgentName::Pi.detected(project_root, &home, global),
        )
    {
        let pi_agents = home.join(".pi/agent/AGENTS.md");
        if install {
            let ch = upsert_block_file(&pi_agents, GUIDE_MD)?;
            mark(&mut done, "pi memory", ch.verb(), &pi_agents);
        } else if remove_block_file(&pi_agents)? {
            mark(&mut done, "pi memory", "removed", &pi_agents);
        }
    }

    if done.is_empty() {
        if !quiet {
            println!("{}", ui::dim("nothing to do"));
        }
        return Ok(false);
    }
    // Did anything actually move? Read the per-mark flag — no text scanning.
    let changed = done.iter().any(|d| d.changed);
    // Quiet auto-refresh stays fully silent unless (and even when) something
    // moved: it runs on the query hot path, so it must never print.
    if quiet {
        return Ok(changed);
    }
    for d in &done {
        println!("{}", d.line);
    }
    println!(
        "{}",
        ui::ok(&format!(
            "agents {} ({})",
            if install { "installed" } else { "uninstalled" },
            if global { "global" } else { "project" }
        ))
    );
    // Only relevant when a Claude hook/skill actually moved (labels start with
    // "claude ") — no reason to nag about a restart for a Cursor/Gemini-only edit.
    let claude_moved = done.iter().any(|d| d.changed && d.line.contains("claude "));
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
    // inert). `installed()` covers skill/cursor/marker files + settings.json.
    if AgentName::ALL
        .iter()
        .any(|a| a.installed(project_root, project_root, false))
    {
        return true;
    }
    // Extra target `config_paths` doesn't enumerate (a glob has no place in its
    // fixed path list): subagent definitions carrying the marker block. Shares
    // the recursive enumerator with the writer — a flat scan would miss the
    // nested definitions install actually patches, so uninstall and the
    // version-gated re-sync would both skip a scope whose only footprint is
    // there.
    let mut paths = Vec::new();
    subagent_defs(&project_root.join(".claude/agents"), 0, &mut paths);
    paths.iter().any(|p| has_marker(p))
}

/// Add/remove cona hooks in a Claude Code settings.json.
/// Returns Ok(true) if the file was changed.
fn claude_hooks(settings_path: &Path, install: bool) -> Result<bool> {
    let existing = std::fs::read_to_string(settings_path).unwrap_or_else(|_| "{}".into());
    let mut root: serde_json::Value = serde_json::from_str(&existing).map_err(|e| {
        anyhow!("existing settings.json is not valid JSON ({e}) — fix it or add the hook manually")
    })?;
    if !root.is_object() {
        bail!("settings.json top level is not an object");
    }
    let exe = agent_exe();
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
            Some("Edit|Write|MultiEdit|NotebookEdit"),
            &index_cmd,
            "index --quiet",
        ),
        ("SessionStart", None, &session_cmd, "index --quiet"),
        // navigation accelerator: redirect wasteful full reads + broad
        // identifier greps toward cona
        (
            "PreToolUse",
            Some("Read|Grep"),
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
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        bail!("settings.json 'hooks' is not an object");
    }
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
        let arr = hooks
            .as_object_mut()
            .unwrap()
            .entry(event)
            .or_insert_with(|| serde_json::json!([]));
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
    if changed {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(names: &[AgentName], all: bool, install: bool) -> AgentSel {
        AgentSel {
            names: names.to_vec(),
            all,
            install,
        }
    }

    #[test]
    fn project_has_cona_detects_markers_and_ignores_clean() {
        let dir = std::env::temp_dir().join("cona-hascn-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // empty project → nothing
        assert!(!project_has_cona(&dir));
        // foreign CLAUDE.md without the marker → still nothing
        std::fs::write(dir.join("CLAUDE.md"), "# my project\n").unwrap();
        assert!(!project_has_cona(&dir));
        // marker block present → detected
        std::fs::write(
            dir.join("CLAUDE.md"),
            format!("# my project\n{}\nguide\n", super::super::BLOCK_BEGIN),
        )
        .unwrap();
        assert!(project_has_cona(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quiet_reinstall_is_a_noop_when_already_current() {
        let dir = std::env::temp_dir().join("cona-quiet-reinstall-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // seed a Claude project so the install has a target
        std::fs::write(dir.join("CLAUDE.md"), "# my project\n").unwrap();
        // first install writes the guide block → changed
        let first =
            cmd_agents_q(&dir, "install", &[AgentName::Claude], false, false, true).unwrap();
        assert!(first, "first install must report a change");
        // second install with identical baked content → no change, quiet returns false
        let second =
            cmd_agents_q(&dir, "install", &[AgentName::Claude], false, false, true).unwrap();
        assert!(!second, "re-install of current config must be a no-op");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_no_selection_autodetects() {
        let s = sel(&[], false, true);
        assert!(s.want(AgentName::Cursor, true)); // detected → yes
        assert!(!s.want(AgentName::Cursor, false)); // undetected → no
    }

    #[test]
    fn install_explicit_name_overrides_detection() {
        let s = sel(&[AgentName::Gemini], false, true);
        assert!(s.want(AgentName::Gemini, false)); // named, undetected → still yes
        assert!(!s.want(AgentName::Cursor, true)); // not named → no, even if detected
    }

    #[test]
    fn all_targets_everything_regardless_of_detection() {
        let s = sel(&[], true, true);
        assert!(s.want(AgentName::Cursor, false));
        assert!(s.want(AgentName::Gemini, false));
    }

    #[test]
    fn uninstall_no_selection_cleans_regardless_of_detection() {
        let s = sel(&[], false, false);
        assert!(s.want(AgentName::Cursor, false)); // bare uninstall → clean whatever is there
    }

    #[test]
    fn uninstall_explicit_name_still_scoped() {
        let s = sel(&[AgentName::Cursor], false, false);
        assert!(s.want(AgentName::Cursor, false));
        assert!(!s.want(AgentName::Gemini, false));
    }

    #[test]
    fn detected_agents_project_core_plus_present_dirs() {
        let tmp = std::env::temp_dir().join(format!("cona-detect-{}", std::process::id()));
        let proj = tmp.join("proj");
        let home = tmp.join("home");
        std::fs::create_dir_all(proj.join(".cursor")).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        // project scope: Claude + AGENTS always; Cursor detected (.cursor exists);
        // Gemini not (no GEMINI.md / .gemini).
        let got = detected_agents(&proj, &home, false);
        assert!(got.contains(&AgentName::Claude));
        assert!(got.contains(&AgentName::Agents));
        assert!(got.contains(&AgentName::Cursor));
        assert!(!got.contains(&AgentName::Gemini));

        // global scope: only Claude is unconditional; nothing else present in home.
        let got_global = detected_agents(&proj, &home, true);
        assert_eq!(got_global, vec![AgentName::Claude]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Agent collections nest definitions in category subdirectories — the walk
    /// must reach them, skip non-definition docs, and round-trip cleanly.
    #[test]
    fn subagents_are_patched_recursively() {
        let tmp = std::env::temp_dir().join(format!("cona-subagents-{}", std::process::id()));
        let proj = tmp.join("proj");
        let nested = proj.join(".claude/agents/engineering/deep");
        std::fs::create_dir_all(&nested).unwrap();

        let top = proj.join(".claude/agents/top.md");
        let deep = nested.join("backend.md");
        let doc = proj.join(".claude/agents/README.md");
        std::fs::write(&top, "---\nname: top\n---\n\nbody\n").unwrap();
        std::fs::write(&deep, "---\nname: backend\n---\n\nbody\n").unwrap();
        std::fs::write(&doc, "# just docs\n").unwrap();

        cmd_agents_q(&proj, "install", &[AgentName::Claude], false, false, true).unwrap();
        assert!(has_marker(&top));
        assert!(has_marker(&deep), "nested agent definition must be patched");
        assert!(!has_marker(&doc), "non-definition doc must stay untouched");

        // the probe shares the walk: a nested-only footprint must still count as
        // installed, else uninstall/re-sync skip the scope
        std::fs::remove_file(&top).unwrap();
        assert!(project_has_cona(&proj));

        cmd_agents_q(&proj, "uninstall", &[AgentName::Claude], false, false, true).unwrap();
        assert!(!has_marker(&deep));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The walk is bounded: it stops at SUBAGENT_MAX_DEPTH and never follows a
    /// symlink, so a loop under .claude/agents can't recurse forever.
    #[test]
    fn subagent_walk_is_bounded() {
        let tmp = std::env::temp_dir().join(format!("cona-subwalk-{}", std::process::id()));
        let agents = tmp.join(".claude/agents");
        let too_deep = agents.join("a/b/c/d/e");
        std::fs::create_dir_all(&too_deep).unwrap();
        std::fs::write(agents.join("a/shallow.md"), "---\nx\n---\n").unwrap();
        std::fs::write(too_deep.join("buried.md"), "---\nx\n---\n").unwrap();

        let mut found = Vec::new();
        subagent_defs(&agents, 0, &mut found);
        assert!(found.iter().any(|p| p.ends_with("shallow.md")));
        assert!(
            !found.iter().any(|p| p.ends_with("buried.md")),
            "walk must stop at SUBAGENT_MAX_DEPTH"
        );

        // a symlink pointing back at the tree must not be descended
        #[cfg(unix)]
        {
            let loop_link = agents.join("loop");
            std::os::unix::fs::symlink(&agents, &loop_link).unwrap();
            let mut again = Vec::new();
            subagent_defs(&agents, 0, &mut again);
            assert_eq!(found.len(), again.len(), "symlinked dir must be skipped");
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn installed_reflects_add_then_remove_per_agent() {
        let tmp = std::env::temp_dir().join(format!("cona-installed-{}", std::process::id()));
        let proj = tmp.join("proj");
        let home = tmp.join("home");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        // clean project: no agent reports installed
        assert!(!AgentName::Cursor.installed(&proj, &home, false));
        assert!(!AgentName::Gemini.installed(&proj, &home, false));

        // add just Cursor → only Cursor flips on, others stay off
        cmd_agents_q(&proj, "install", &[AgentName::Cursor], false, false, true).unwrap();
        assert!(AgentName::Cursor.installed(&proj, &home, false));
        assert!(!AgentName::Gemini.installed(&proj, &home, false));

        // remove Cursor → back to off (round-trip leaves no residue)
        cmd_agents_q(&proj, "uninstall", &[AgentName::Cursor], false, false, true).unwrap();
        assert!(!AgentName::Cursor.installed(&proj, &home, false));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn config_paths_empty_only_for_pi_project_scope() {
        let proj = Path::new("/proj");
        let home = Path::new("/home");
        // Pi has no project target; everything else does.
        assert!(AgentName::Pi.config_paths(proj, home, false).is_empty());
        assert!(!AgentName::Pi.config_paths(proj, home, true).is_empty());
        for a in [
            AgentName::Claude,
            AgentName::Agents,
            AgentName::Cursor,
            AgentName::Gemini,
        ] {
            assert!(!a.config_paths(proj, home, false).is_empty());
            assert!(!a.config_paths(proj, home, true).is_empty());
        }
    }
}
