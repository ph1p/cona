//! The agent roster: `AgentName` and every per-agent fact (config paths,
//! detection, MCP key), plus presence probes and the detected/installed sets.

use super::apply::subagent_defs;
use super::*;
use crate::install::mcp_config;
use std::path::{Path, PathBuf};

/// THE XDG config root for the harnesses that live under one (OpenCode, Zed,
/// Crush): `$XDG_CONFIG_HOME` when set, else `~/.config`.
///
/// The env var is honoured only when it is absolute AND sits under the `home`
/// being asked about. A relative or empty value is spec-invalid and would
/// otherwise resolve against the cwd, scattering config into whatever directory
/// cona ran from; and `home` is not always the real one — tests and the
/// per-scope probes pass a synthetic root, which an unfiltered env var would
/// escape, making detection read the developer's actual `~/.config`.
pub(super) fn xdg_config(home: &Path) -> PathBuf {
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
    /// Only the guide-file loop reads this; Claude's hand-written block labels
    /// several targets ("claude skill" / "claude hooks" / …), which no single
    /// per-agent string could express.
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

    /// The content a `Presence::Exists` guide file carries. Everyone gets
    /// GUIDE_MD verbatim; Cursor wraps it in `.mdc` frontmatter because its
    /// rule loader needs `alwaysApply` to inject the guide unprompted.
    pub fn guide_body(self) -> String {
        match self {
            AgentName::Cursor => format!(
                "---\ndescription: cona — token-efficient code navigation\nalwaysApply: true\n---\n\n{GUIDE_MD}"
            ),
            _ => GUIDE_MD.to_string(),
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
pub(super) fn has_marker(p: &Path) -> bool {
    std::fs::read_to_string(p).is_ok_and(|c| c.contains(crate::install::BLOCK_BEGIN))
}

/// Is the cona Claude Code plugin enabled for sessions in this project?
/// The plugin ships hooks + skill + MCP in one payload, so with it enabled the
/// installer's settings.json hooks, skill file, and project `.mcp.json` entry
/// are pure duplicates — every session would run each hook and inject the
/// SessionStart context twice. Plugins can be enabled in the global or the
/// project settings.json; either counts. An unreadable or invalid settings
/// file counts as "no plugin", so a broken file degrades to a normal install,
/// never to a silently skipped one.
pub(crate) fn claude_plugin_enabled(project_root: &Path, home: &Path) -> bool {
    [
        home.join(".claude/settings.json"),
        project_root.join(".claude/settings.json"),
    ]
    .iter()
    .any(|p| {
        std::fs::read_to_string(p)
            .map(|t| plugin_enabled_in(&t))
            .unwrap_or(false)
    })
}

/// The parse half of `claude_plugin_enabled`, split out for tests: does this
/// settings.json enable a cona plugin (`enabledPlugins` key `cona` or
/// `cona@<marketplace>` set to true)?
pub(super) fn plugin_enabled_in(settings_json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(settings_json) else {
        return false;
    };
    v["enabledPlugins"].as_object().is_some_and(|plugins| {
        plugins.iter().any(|(key, on)| {
            (key == "cona" || key.starts_with("cona@")) && on.as_bool() == Some(true)
        })
    })
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
