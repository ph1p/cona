//! Agent integration: inject the usage guide + skill + hooks + MCP entry into
//! agent configs — idempotent, marker-based, uninstallable.
//!
//! `AgentName` is the roster; every per-agent fact (where its config lives,
//! how to tell it is there, which MCP key it speaks) hangs off that enum rather
//! than a list repeated per call site, so adding a harness is one variant plus
//! its match arms. Deliberately NOT enumerated in prose here — a hand-kept list
//! in a doc comment is the first thing to go stale. `cona agents --help` prints
//! the live set.

use super::mcp_config;
use super::{mark, remove_block_file, upsert_block_file, write_if_changed, SKILL_MD};
use crate::hook::PRETOOL_MATCHER;
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
`cona index` (~1s) if a repo isn't indexed yet. In a sandbox where `~/.cona`
is not writable, cona falls back to temporary storage; set `CONA_DATA_DIR` when
you need a persistent index. Use `--read-only` to inspect an existing index
without writing code, indexes, or usage stats.

Too many hits? `--path <dir>` scopes `find`/`refs`/`grep`/`tree` to a subtree.
Ambiguous name? `cona show <Sym> --all` prints every definition instead of
erroring. `cona grep` matches literally; add `--regex` for a real regex.

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

/// THE XDG config root for the harnesses that live under one (OpenCode, Zed,
/// Crush): `$XDG_CONFIG_HOME` when set, else `~/.config`.
///
/// The env var is honoured only when it is absolute AND sits under the `home`
/// being asked about. A relative or empty value is spec-invalid and would
/// otherwise resolve against the cwd, scattering config into whatever directory
/// cona ran from; and `home` is not always the real one — tests and the
/// per-scope probes pass a synthetic root, which an unfiltered env var would
/// escape, making detection read the developer's actual `~/.config`.
fn xdg_config(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.starts_with(home))
        .unwrap_or_else(|| home.join(".config"))
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
    Opencode,
    Windsurf,
    Zed,
    Qwen,
    Crush,
    Copilot,
}

impl AgentName {
    /// Every agent, in menu/priority order. The one place the full set lives.
    pub const ALL: [AgentName; 11] = [
        AgentName::Claude,
        AgentName::Agents,
        AgentName::Cursor,
        AgentName::Gemini,
        AgentName::Opencode,
        AgentName::Windsurf,
        AgentName::Zed,
        AgentName::Qwen,
        AgentName::Crush,
        AgentName::Copilot,
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
            AgentName::Opencode => "opencode",
            AgentName::Windsurf => "windsurf",
            AgentName::Zed => "zed",
            AgentName::Qwen => "qwen",
            AgentName::Crush => "crush",
            AgentName::Copilot => "copilot",
        }
    }

    /// One-line description for the interactive picker.
    pub fn desc(self) -> &'static str {
        match self {
            AgentName::Claude => "Claude Code — skill + hooks + CLAUDE.md",
            // The generic bucket owns the PROJECT AGENTS.md that most harnesses
            // read, plus Codex's own global copy. Harnesses with a *distinct*
            // global path (OpenCode, Zed, Crush) are their own entries below —
            // ~/.codex/AGENTS.md is Codex's, not a shared global file.
            AgentName::Agents => "AGENTS.md — Codex / Amp / Jules / Cline",
            AgentName::Cursor => "Cursor — .cursor/rules",
            AgentName::Gemini => "Gemini CLI — GEMINI.md",
            AgentName::Pi => "pi.dev — AGENTS.md",
            AgentName::Opencode => "OpenCode — AGENTS.md + opencode.json",
            AgentName::Windsurf => "Windsurf — .windsurf/rules",
            AgentName::Zed => "Zed — AGENTS.md + context servers",
            AgentName::Qwen => "Qwen Code — QWEN.md",
            AgentName::Crush => "Crush — CRUSH.md",
            AgentName::Copilot => "GitHub Copilot — copilot-instructions.md",
        }
    }

    /// Picker row description annotated with the row's state — THE state
    /// wording, shared by `cona setup` and `cona agents` so "uncheck removes"
    /// reads the same everywhere.
    pub fn state_desc(self, installed: bool, checked: bool) -> String {
        if installed {
            format!("{} · installed — uncheck to remove", self.desc())
        } else if checked {
            format!("{} · detected", self.desc())
        } else {
            self.desc().to_string()
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
            // The harnesses below read a project AGENTS.md the generic bucket
            // already writes; what makes them their OWN entry is a distinct
            // global path (and, for some, an MCP config shape of their own).
            AgentName::Opencode => {
                if global {
                    xdg_config(home).join("opencode").exists()
                } else {
                    project_root.join("opencode.json").exists()
                        || project_root.join("opencode.jsonc").exists()
                }
            }
            AgentName::Windsurf => {
                if global {
                    home.join(".codeium/windsurf").exists()
                } else {
                    project_root.join(".windsurf").exists()
                }
            }
            AgentName::Zed => {
                if global {
                    xdg_config(home).join("zed").exists()
                } else {
                    project_root.join(".zed").exists()
                }
            }
            AgentName::Qwen => {
                if global {
                    home.join(".qwen").exists()
                } else {
                    project_root.join("QWEN.md").exists() || project_root.join(".qwen").exists()
                }
            }
            AgentName::Crush => {
                if global {
                    xdg_config(home).join("crush").exists()
                } else {
                    project_root.join("CRUSH.md").exists() || project_root.join(".crush").exists()
                }
            }
            // Copilot's instructions file is checked in, so its presence IS the
            // signal at project scope; globally it is the CLI's own dir.
            AgentName::Copilot => {
                if global {
                    home.join(".copilot").exists()
                } else {
                    project_root
                        .join(".github/copilot-instructions.md")
                        .exists()
                        || project_root.join(".github/instructions").exists()
                }
            }
        }
    }

    /// Every file this agent's cona install leaves a trace in for the given
    /// scope — the guide targets PLUS the MCP entry — each tagged with HOW to
    /// detect cona there (`Presence`). This is the "is cona installed here?"
    /// question: a scope whose ONLY trace is the server entry must still count,
    /// or uninstall (`project_has_cona`) and the status ✓ would both miss it.
    ///
    /// NOT the same question as `config_paths` — see there.
    pub fn footprint_paths(
        self,
        project_root: &Path,
        home: &Path,
        global: bool,
    ) -> Vec<(PathBuf, Presence)> {
        let mut paths = self.config_paths(project_root, home, global);
        if let Some(p) = self.mcp_path(project_root, home, global) {
            paths.push((p, Presence::McpServer));
        }
        if self == AgentName::Claude {
            let base = if global { home } else { project_root };
            paths.push((base.join(".claude/agents"), Presence::SubagentDefs));
        }
        paths
    }

    /// The guide/skill/hook targets this scope can act on — everything but the
    /// MCP entry. This is the "can this scope configure the agent?" question
    /// (`agents_in_scope`, the n/a status cells): an agent that only had an MCP
    /// target here would be offered in the picker and then receive nothing, so
    /// the two readings stay separate functions. Empty = no target in this scope
    /// (e.g. Pi at project scope).
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
            // OpenCode / Zed read the PROJECT AGENTS.md the generic bucket
            // already owns — writing it twice would fight over one marker block
            // — so at project scope they contribute their MCP entry only, and
            // `config_paths` is empty (same shape as Pi).
            AgentName::Opencode if global => vec![(
                xdg_config(home).join("opencode/AGENTS.md"),
                Presence::Marker,
            )],
            AgentName::Zed if global => {
                vec![(xdg_config(home).join("zed/AGENTS.md"), Presence::Marker)]
            }
            AgentName::Opencode | AgentName::Zed => vec![],
            // Windsurf: per-rule files at project scope, one global memories
            // file. The project rule is a full-file write (ours alone), the
            // global one a marker block in a file the user also writes.
            AgentName::Windsurf if global => vec![(
                home.join(".codeium/windsurf/memories/global_rules.md"),
                Presence::Marker,
            )],
            AgentName::Windsurf => vec![(
                project_root.join(".windsurf/rules/cona.md"),
                Presence::Exists,
            )],
            AgentName::Qwen => {
                let p = if global {
                    home.join(".qwen/QWEN.md")
                } else {
                    project_root.join("QWEN.md")
                };
                vec![(p, Presence::Marker)]
            }
            AgentName::Crush => {
                let p = if global {
                    xdg_config(home).join("crush/CRUSH.md")
                } else {
                    project_root.join("CRUSH.md")
                };
                vec![(p, Presence::Marker)]
            }
            // Copilot's project file is the checked-in repo instruction file;
            // globally the CLI reads its own copy under ~/.copilot.
            AgentName::Copilot => {
                let p = if global {
                    home.join(".copilot/copilot-instructions.md")
                } else {
                    project_root.join(".github/copilot-instructions.md")
                };
                vec![(p, Presence::Marker)]
            }
        }
    }

    /// Where this agent reads MCP server definitions from, for the given scope.
    /// `None` = the harness has no MCP config we own in that scope, so the
    /// guide/skill integration is all it gets. THE single source of MCP
    /// targets — install, uninstall and the status row all read it.
    ///
    /// Claude Code deliberately has only a project target (`.mcp.json`, the
    /// checked-in team scope): its user-scope servers live in `~/.claude.json`,
    /// a file Claude Code owns as live session state — cona does not rewrite it.
    pub fn mcp_path(self, project_root: &Path, home: &Path, global: bool) -> Option<PathBuf> {
        // Same relative path in both scopes for everything but Claude — only
        // the base moves.
        let base = if global { home } else { project_root };
        match self {
            AgentName::Claude if !global => Some(project_root.join(".mcp.json")),
            AgentName::Claude => None,
            // Codex speaks TOML; project scope only applies to trusted projects,
            // but writing it is harmless there and matches its documented path.
            AgentName::Agents => Some(base.join(".codex/config.toml")),
            AgentName::Cursor => Some(base.join(".cursor/mcp.json")),
            AgentName::Gemini => Some(base.join(".gemini/settings.json")),
            // pi.dev's MCP config shape isn't ours to guess — guide only.
            AgentName::Pi => None,
            // OpenCode's project config sits at the repo root; globally it
            // lives beside its AGENTS.md under XDG.
            AgentName::Opencode if global => Some(xdg_config(home).join("opencode/opencode.json")),
            AgentName::Opencode => Some(project_root.join("opencode.json")),
            // Windsurf has no documented per-project MCP file — the one config
            // is global, under its Codeium data dir.
            AgentName::Windsurf if global => Some(home.join(".codeium/windsurf/mcp_config.json")),
            AgentName::Windsurf => None,
            AgentName::Zed if global => Some(xdg_config(home).join("zed/settings.json")),
            AgentName::Zed => Some(project_root.join(".zed/settings.json")),
            AgentName::Qwen => Some(base.join(".qwen/settings.json")),
            AgentName::Crush if global => Some(xdg_config(home).join("crush/crush.json")),
            AgentName::Crush => Some(project_root.join(".crush.json")),
            // Copilot CLI keeps one MCP config in its own dir; the VS Code
            // extension's project `.vscode/mcp.json` is IDE-managed, not ours.
            AgentName::Copilot if global => Some(home.join(".copilot/mcp-config.json")),
            AgentName::Copilot => None,
        }
    }

    /// The top-level key + entry shape this harness expects its MCP servers
    /// under. Split from `mcp_path` because the two answers vary independently:
    /// most harnesses spell it `mcpServers`, but OpenCode/Crush use `mcp` with a
    /// `"local"` transport and an argv array, and Zed calls them
    /// `context_servers`. A wrong key does not error — the harness simply never
    /// sees the server — so each agent names its own.
    pub fn mcp_key(self) -> mcp_config::ServerKey {
        match self {
            AgentName::Opencode | AgentName::Crush => mcp_config::ServerKey::Mcp,
            AgentName::Zed => mcp_config::ServerKey::ContextServers,
            _ => mcp_config::ServerKey::McpServers,
        }
    }

    /// The status-line label for this agent's guide target. Must fit
    /// `Mark::render`'s `LABEL_COL` column — a longer one shifts that row's verb
    /// and path out of line with every other row (`label_widths_fit_the_column`).
    ///
    /// Only the guide-only harnesses read this; the hand-written blocks above
    /// label several targets each ("claude skill" / "claude hooks" / …), which
    /// no single per-agent string could express.
    pub fn mark_label(self) -> &'static str {
        match self {
            AgentName::Opencode => "opencode guide",
            AgentName::Windsurf => "windsurf rule",
            AgentName::Zed => "zed guide",
            AgentName::Qwen => "qwen memory",
            AgentName::Crush => "crush memory",
            AgentName::Copilot => "copilot guide",
            AgentName::Claude => "claude skill",
            AgentName::Agents => "AGENTS.md",
            AgentName::Cursor => "cursor rule",
            AgentName::Gemini => "gemini memory",
            AgentName::Pi => "pi memory",
        }
    }

    /// Is cona currently wired into this agent for the given scope? Probes each
    /// config path the way its `Presence` tag dictates — so it reflects an
    /// actual install, not mere presence of the agent (`detected`).
    pub fn installed(self, project_root: &Path, home: &Path, global: bool) -> bool {
        self.footprint_paths(project_root, home, global)
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
    /// An MCP server entry (`mcpServers.cona` in JSON, a marked
    /// `[mcp_servers.cona]` table in TOML) — probed by `mcp_config`.
    McpServer,
    /// A `.claude/agents` tree with at least one marked subagent definition —
    /// per-def marker blocks are the only Claude footprint a fixed path list
    /// can't name, so the probe scans the dir (recursive, bounded).
    SubagentDefs,
}

impl Presence {
    fn present(self, p: &Path) -> bool {
        match self {
            Presence::Exists => p.exists(),
            Presence::Needle => std::fs::read_to_string(p).is_ok_and(|c| c.contains("cona")),
            Presence::Marker => has_marker(p),
            Presence::McpServer => mcp_config::registered(p),
            Presence::SubagentDefs => {
                let mut defs = Vec::new();
                subagent_defs(p, 0, &mut defs);
                defs.iter().any(|d| has_marker(d))
            }
        }
    }
}

/// A file carries a cona marker block. THE shared marker probe.
fn has_marker(p: &Path) -> bool {
    std::fs::read_to_string(p).is_ok_and(|c| c.contains(super::BLOCK_BEGIN))
}

/// Every MCP target cona owns, as `(agent, scope-is-global, path, registered)`.
/// THE single traversal of `AgentName::ALL × scopes × mcp_path` — `agents
/// status` folds it to one cell per agent, `doctor` prints the registered rows.
/// Two surfaces, one enumeration, so a new agent shows up in both from its
/// `mcp_path` arm alone.
pub fn mcp_registrations(
    project_root: &Path,
    home: &Path,
) -> Vec<(AgentName, bool, PathBuf, bool)> {
    let mut out = Vec::new();
    for a in AgentName::ALL {
        for global in [false, true] {
            if let Some(p) = a.mcp_path(project_root, home, global) {
                let on = mcp_config::registered(&p);
                out.push((a, global, p, on));
            }
        }
    }
    out
}

/// The agents whose config is detected on disk (used to pre-check the picker
/// and as the non-interactive autodetect set).
pub fn detected_agents(project_root: &Path, home: &Path, global: bool) -> Vec<AgentName> {
    AgentName::ALL
        .into_iter()
        .filter(|a| a.detected(project_root, home, global))
        .collect()
}

/// The agents that already carry cona config in this scope — THE refresh
/// target set. Upgrades re-sync what IS installed, never what merely COULD be
/// (the rustup/brew model: updating refreshes installed components only) — a
/// detected-but-never-selected agent must not gain config from an upgrade.
pub fn installed_agents(project_root: &Path, home: &Path, global: bool) -> Vec<AgentName> {
    AgentName::ALL
        .into_iter()
        .filter(|a| a.installed(project_root, home, global))
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

    // One row per agent: name, the scope cells (guide + mcp), description. A table scans
    // in one glance where a block-per-agent needs scrolling — and `▸` stays a
    // section marker instead of doubling as a row bullet.
    // Pad BEFORE coloring — ANSI escapes would break every column width.
    let name_w = AgentName::ALL
        .iter()
        .map(|a| a.slug().len())
        .max()
        .unwrap_or(0);
    let cell = |installed: bool, na: bool| {
        // Widest cell text is "– off"/"✓ on"/"n/a" → pad to 5 chars.
        if na {
            ui::dim(&format!("{:<5}", "n/a"))
        } else if installed {
            ui::green(&format!("{:<5}", "✓ on"))
        } else {
            ui::dim(&format!("{:<5}", "– off"))
        }
    };
    println!(
        "  {}  {}  {}  {}  {}",
        ui::dim(&format!("{:<name_w$}", "agent")),
        ui::dim(&format!("{:<5}", "proj")),
        ui::dim(&format!("{:<5}", "glob")),
        ui::dim(&format!("{:<5}", "mcp")),
        ui::dim("target")
    );
    let mut any_installed = false;
    let mcp = mcp_registrations(project_root, &home);
    for a in AgentName::ALL {
        let proj = a.installed(project_root, &home, false);
        let glob = a.installed(project_root, &home, true);
        any_installed |= proj || glob;
        // does this agent even have a target in each scope?
        let proj_na = a.config_paths(project_root, &home, false).is_empty();
        let glob_na = a.config_paths(project_root, &home, true).is_empty();
        // MCP is a second, optional surface: on when the server entry exists in
        // EITHER scope, n/a for a harness cona has no MCP config for.
        let mut rows = mcp.iter().filter(|(n, ..)| *n == a).peekable();
        let mcp_na = rows.peek().is_none();
        let mcp_on = rows.any(|&(.., on)| on);
        println!(
            "  {}  {}  {}  {}  {}",
            ui::bold(&format!("{:<name_w$}", a.slug())),
            cell(proj, proj_na),
            cell(glob, glob_na),
            cell(mcp_on, mcp_na),
            ui::dim(a.desc())
        );
    }
    println!();

    println!("{}", ui::heading("manage"));
    print!(
        "{}",
        ui::cmd_table(&[
            (
                "cona agents add <name>",
                "configure one agent (this project)"
            ),
            (
                "cona agents add <name> --global",
                "configure one agent (home configs)"
            ),
            ("cona agents remove <name>", "remove one agent"),
            ("cona agents", "interactive checklist (toggle any)"),
        ])
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
        .map(|(a, on)| ui::Row::Item(a.slug(), a.state_desc(*on, *on).into(), *on))
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
fn prune_empty_dirs(file: &Path, stop: &Path) {
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

fn mcp_register(agent: AgentName, ctx: &Ctx, install: bool, done: &mut Vec<super::Mark>) {
    let Some(path) = agent.mcp_path(ctx.project_root, ctx.home, ctx.global) else {
        return;
    };
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
        Ok(super::Change::Unchanged) => {}
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
struct Ctx<'a> {
    project_root: &'a Path,
    home: &'a Path,
    global: bool,
    exe: String,
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
    // How far up `prune_empty_dirs` may climb after deleting a file: never out
    // of the scope this run was asked to touch.
    let scope_root: &Path = if global { &home } else { project_root };

    let sel = AgentSel {
        names: names.to_vec(),
        all,
        install,
    };
    let ctx = Ctx {
        project_root,
        home: &home,
        global,
        exe: agent_exe(),
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
            prune_empty_dirs(&cursor, scope_root);
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

    // --- guide-only harnesses -------------------------------------------------
    // OpenCode, Windsurf, Zed, Qwen, Crush, Copilot each read one guide file per
    // scope, so they need no hand-written block: `config_paths` already IS the
    // per-scope target list, and its `Presence` tag says how the file is
    // written — `Marker` = splice a block into a file the user also owns,
    // `Exists` = the file is ours alone. Driving both from that ONE list keeps
    // the writer and the installed()/uninstall probe from ever disagreeing
    // about which file an agent owns.
    for a in [
        AgentName::Opencode,
        AgentName::Windsurf,
        AgentName::Zed,
        AgentName::Qwen,
        AgentName::Crush,
        AgentName::Copilot,
    ] {
        if !sel.want(a, a.detected(project_root, &home, global)) {
            continue;
        }
        let label = a.mark_label();
        for (path, kind) in a.config_paths(project_root, &home, global) {
            match kind {
                // Ours alone: a whole-file write, removed outright.
                Presence::Exists => {
                    if install {
                        let ch = write_if_changed(&path, GUIDE_MD)?;
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
        // pid-suffixed so concurrent test invocations (e.g. `cargo test` in two
        // checkouts) can't race on one shared directory
        let dir =
            std::env::temp_dir().join(format!("cona-quiet-reinstall-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // seed a Claude project so the install has a target
        std::fs::write(dir.join("CLAUDE.md"), "# my project\n").unwrap();
        // first install writes the guide block → changed
        let first =
            cmd_agents_q(&dir, "install", &[AgentName::Claude], false, false, true).unwrap();
        assert!(first, "first install must report a change");
        // Second install with identical baked content → no change. The MCP
        // entry bakes in `agent_exe()`, which reads `install_path` from the
        // SHARED global.db that a concurrently running lib test may rewrite
        // between installs; that flips the entry's command and makes a rerun
        // report a change for a reason this test isn't about. Retry until an
        // install reports no change — each rerun re-bakes the currently
        // resolved exe, so it converges once the flipping stops, while a real
        // "reinstall always reports change" bug still exhausts the retries.
        // (Comparing agent_exe() before/after one call is NOT enough: the flip
        // can land between the previous install and the `before` sample.)
        let mut second = true;
        for _ in 0..5 {
            second =
                cmd_agents_q(&dir, "install", &[AgentName::Claude], false, false, true).unwrap();
            if !second {
                break;
            }
        }
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

    /// The upgrade refresh path targets `installed_agents` — an agent that is
    /// merely DETECTED (its dir exists) but never got cona config must not be
    /// in the set, or every upgrade would install config the user never chose.
    #[test]
    fn installed_agents_refresh_set_excludes_detected_but_unconfigured() {
        let tmp = std::env::temp_dir().join(format!("cona-installedset-{}", std::process::id()));
        let proj = tmp.join("proj");
        let home = tmp.join("home");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(proj.join(".cursor")).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        // .cursor exists (detected) but carries no cona rule; AGENTS.md holds
        // the marker block (installed).
        std::fs::write(
            proj.join("AGENTS.md"),
            format!("{}\nguide\n", super::super::BLOCK_BEGIN),
        )
        .unwrap();

        let got = installed_agents(&proj, &home, false);
        assert_eq!(got, vec![AgentName::Agents]);
        // sanity: detection WOULD have pulled in Claude + Cursor too
        assert!(detected_agents(&proj, &home, false).contains(&AgentName::Cursor));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A scope whose only Claude footprint is a marked subagent definition
    /// still counts Claude as installed — else the version-gated re-sync would
    /// leave exactly those defs stale.
    #[test]
    fn installed_agents_counts_subagent_only_claude_footprint() {
        let tmp = std::env::temp_dir().join(format!("cona-installedsub-{}", std::process::id()));
        let proj = tmp.join("proj");
        let home = tmp.join("home");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(proj.join(".claude/agents/engineering")).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        assert!(installed_agents(&proj, &home, false).is_empty());
        std::fs::write(
            proj.join(".claude/agents/engineering/dev.md"),
            format!(
                "---\nname: dev\n---\nbody\n{}\nguide\n",
                super::super::BLOCK_BEGIN
            ),
        )
        .unwrap();
        assert_eq!(
            installed_agents(&proj, &home, false),
            vec![AgentName::Claude]
        );

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
        // Exactly the agents whose project-scope guide is the project AGENTS.md
        // the generic bucket already owns. Writing it from their block too would
        // put two owners on one marker block, so they contribute an MCP entry
        // there and nothing else. Derived from ALL, not a second hand-kept list:
        // a new agent that forgets a project target then fails HERE rather than
        // silently installing nothing.
        let no_project_target = [AgentName::Pi, AgentName::Opencode, AgentName::Zed];
        for a in AgentName::ALL {
            let empty = a.config_paths(proj, home, false).is_empty();
            assert_eq!(
                empty,
                no_project_target.contains(&a),
                "{} project-scope config_paths emptiness",
                a.slug()
            );
            // Every agent has a global target — that is what makes it an entry
            // of its own rather than a row in the generic AGENTS.md bucket.
            assert!(
                !a.config_paths(proj, home, true).is_empty(),
                "{} has no global config target",
                a.slug()
            );
        }
    }

    /// `Mark::render` pads the label to a fixed column; a longer one pushes its
    /// row's verb and path out of line with every other row. Checked for every
    /// agent, so adding one with a verbose label fails here rather than
    /// producing a ragged install log nobody notices.
    #[test]
    fn label_widths_fit_the_column() {
        for a in AgentName::ALL {
            let label = a.mark_label();
            assert!(
                label.len() <= super::super::LABEL_COL,
                "{} label {label:?} is {} chars, over the {}-char column",
                a.slug(),
                label.len(),
                super::super::LABEL_COL
            );
        }
    }

    /// Pruning must clean up the scaffolding an install created without ever
    /// taking a directory the user also keeps things in, and without walking
    /// out of the scope it was given. The stop-at-first-non-empty rule is what
    /// buys both: it is the same guarantee, checked from three directions.
    #[test]
    fn prune_stops_at_non_empty_dirs_and_at_the_anchor() {
        let tmp = std::env::temp_dir().join(format!("cona-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        // A chain nothing else lives in: every level goes.
        let deep = tmp.join("proj/.cursor/rules/cona.mdc");
        std::fs::create_dir_all(deep.parent().unwrap()).unwrap();
        std::fs::write(&deep, "x").unwrap();
        std::fs::remove_file(&deep).unwrap();
        super::prune_empty_dirs(&deep, &tmp.join("proj"));
        assert!(
            !tmp.join("proj/.cursor").exists(),
            "empty chain should be gone"
        );
        assert!(
            tmp.join("proj").exists(),
            "the anchor itself is never removed"
        );

        // A sibling the user owns stops the walk at that level.
        let ours = tmp.join("proj2/.windsurf/rules/cona.md");
        std::fs::create_dir_all(ours.parent().unwrap()).unwrap();
        std::fs::write(&ours, "x").unwrap();
        std::fs::write(ours.parent().unwrap().join("theirs.md"), "keep").unwrap();
        std::fs::remove_file(&ours).unwrap();
        super::prune_empty_dirs(&ours, &tmp.join("proj2"));
        assert!(
            ours.parent().unwrap().exists(),
            "a directory still holding the user's file must survive"
        );

        // Anchored at the file's own dir: nothing above it may be touched.
        let shallow = tmp.join("proj3/only.md");
        std::fs::create_dir_all(shallow.parent().unwrap()).unwrap();
        std::fs::write(&shallow, "x").unwrap();
        std::fs::remove_file(&shallow).unwrap();
        super::prune_empty_dirs(&shallow, &tmp.join("proj3"));
        assert!(
            tmp.join("proj3").exists(),
            "must not remove the anchor when empty"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Scratch settings.json path, unique per test + process.
    fn settings_tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cona-hooks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }

    #[test]
    fn uninstall_removes_a_settings_file_that_only_held_our_hooks() {
        let p = settings_tmp("husk");
        assert!(claude_hooks(&p, true).unwrap(), "install must write");
        assert!(p.exists());
        assert!(claude_hooks(&p, false).unwrap(), "uninstall must change");
        // No `{"hooks": {"PreToolUse": [], …}}` husk left behind: the file cona
        // created, and whose only content was cona's, goes away entirely.
        assert!(!p.exists(), "cona-only settings.json must be removed");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn uninstall_keeps_foreign_settings_and_prunes_only_what_it_emptied() {
        let p = settings_tmp("foreign");
        std::fs::write(
            &p,
            r#"{"model":"opus","hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"mine"}]}]}}"#,
        )
        .unwrap();
        claude_hooks(&p, true).unwrap();
        claude_hooks(&p, false).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["model"], "opus");
        // PreToolUse still holds the foreign entry, so it survives …
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(v["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "mine");
        // … while the events only cona ever populated are gone, not left empty.
        assert!(v["hooks"].get("PostToolUse").is_none());
        assert!(v["hooks"].get("SessionStart").is_none());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    /// An event array that was ALREADY empty before the install belongs to the
    /// user, not to us. The uninstall sweep removes empty arrays as husks of
    /// our own hooks, so without remembering which ones arrived empty it would
    /// delete this one — and, since it was the file's only key, take the whole
    /// settings.json with it (invariant 6: never touch foreign content).
    #[test]
    fn uninstall_keeps_an_event_array_that_was_empty_before_we_installed() {
        let p = settings_tmp("preempty");
        std::fs::write(&p, r#"{"hooks":{"Custom":[]}}"#).unwrap();
        claude_hooks(&p, true).unwrap();
        claude_hooks(&p, false).unwrap();
        assert!(
            p.exists(),
            "settings.json holding a foreign key must survive"
        );
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(
            v["hooks"]["Custom"]
                .as_array()
                .is_some_and(|a| a.is_empty()),
            "the user's empty event must come back exactly as it was, got {v}"
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn uninstall_without_our_hooks_never_creates_structure() {
        let p = settings_tmp("noop");
        std::fs::write(&p, "{\"model\":\"opus\"}").unwrap();
        assert!(
            !claude_hooks(&p, false).unwrap(),
            "uninstall with nothing of ours must report no change"
        );
        // Byte-identical: no `hooks` key materialized on the way out.
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"model\":\"opus\"}");
        // Same for a settings.json that does not exist at all.
        let missing = p.parent().unwrap().join("absent.json");
        assert!(!claude_hooks(&missing, false).unwrap());
        assert!(!missing.exists());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}
