//! The agent roster: `AgentName` and every per-agent fact (config paths,
//! detection, MCP key), plus presence probes and the detected/installed sets.

use super::apply::subagent_defs;
use super::*;
use crate::install::mcp_config;
use std::path::{Path, PathBuf};

/// THE XDG config root for the harnesses that live under one (OpenCode, Zed,
/// Crush): `$XDG_CONFIG_HOME` when set, else `~/.config`.
///
/// The env var is honoured only when absolute AND under the `home` asked about.
/// A relative/empty value is spec-invalid and would resolve against the cwd;
/// and tests/per-scope probes pass a synthetic `home`, which an unfiltered env
/// var would escape into the developer's real `~/.config`.
pub(super) fn xdg_config(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.starts_with(home))
        .unwrap_or_else(|| home.join(".config"))
}

/// The agents `cmd_agents` knows how to configure. A `clap::ValueEnum`, so the
/// CLI validates names at parse time and `--all` / `want()` derive from the
/// SAME variant set — no hand-kept string list to drift.
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
            // The generic bucket owns the PROJECT AGENTS.md most harnesses read,
            // plus Codex's global copy (~/.codex/AGENTS.md is Codex's alone).
            // Harnesses with a distinct global path have their own entries.
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
    /// always count as present. THE one detection source (cmd_agents' gating
    /// and the setup picker).
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
            // Never detected at project scope: the Agents bucket already covers
            // the project AGENTS.md.
            AgentName::Pi => global && home.join(".pi").exists(),
            // These read the project AGENTS.md the generic bucket writes; a
            // distinct global path (and sometimes MCP shape) makes them separate.
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
            // Project: the checked-in instructions file; global: the CLI's dir.
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

    /// Every file a cona install leaves a trace in for this scope — guide
    /// targets PLUS the MCP entry — each tagged with how to detect cona there.
    /// Answers "is cona installed here?": a scope whose ONLY trace is the server
    /// entry must still count for `project_has_cona` and the status ✓.
    /// NOT the same question as `config_paths`.
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
    /// MCP entry. Answers "can this scope configure the agent?"
    /// (`agents_in_scope`, the n/a status cells); an MCP-only agent would be
    /// offered in the picker and then get nothing. Empty = no target here
    /// (e.g. Pi at project scope).
    pub fn config_paths(
        self,
        project_root: &Path,
        home: &Path,
        global: bool,
    ) -> Vec<(PathBuf, Presence)> {
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
            // OpenCode / Zed read the PROJECT AGENTS.md the generic bucket owns
            // (two writers would fight over one marker block), so at project
            // scope they contribute only their MCP entry.
            AgentName::Opencode if global => vec![(
                xdg_config(home).join("opencode/AGENTS.md"),
                Presence::Marker,
            )],
            AgentName::Zed if global => {
                vec![(xdg_config(home).join("zed/AGENTS.md"), Presence::Marker)]
            }
            AgentName::Opencode | AgentName::Zed => vec![],
            // Windsurf: project rule file is ours alone; the global memories
            // file is shared, so a marker block.
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

    /// Where this agent reads MCP server definitions from in this scope; `None`
    /// = no MCP config we own there. THE single source of MCP targets (install,
    /// uninstall, status).
    ///
    /// Claude Code has only a project target (`.mcp.json`): its user-scope
    /// servers live in `~/.claude.json`, live session state cona won't rewrite.
    pub fn mcp_path(self, project_root: &Path, home: &Path, global: bool) -> Option<PathBuf> {
        let base = if global { home } else { project_root };
        match self {
            AgentName::Claude if !global => Some(project_root.join(".mcp.json")),
            AgentName::Claude => None,
            // Codex speaks TOML; project scope applies only to trusted projects
            // but is harmless to write.
            AgentName::Agents => Some(base.join(".codex/config.toml")),
            AgentName::Cursor => Some(base.join(".cursor/mcp.json")),
            AgentName::Gemini => Some(base.join(".gemini/settings.json")),
            // pi.dev's MCP config shape isn't ours to guess — guide only.
            AgentName::Pi => None,
            AgentName::Opencode if global => Some(xdg_config(home).join("opencode/opencode.json")),
            AgentName::Opencode => Some(project_root.join("opencode.json")),
            // Windsurf has no documented per-project MCP file.
            AgentName::Windsurf if global => Some(home.join(".codeium/windsurf/mcp_config.json")),
            AgentName::Windsurf => None,
            AgentName::Zed if global => Some(xdg_config(home).join("zed/settings.json")),
            AgentName::Zed => Some(project_root.join(".zed/settings.json")),
            AgentName::Qwen => Some(base.join(".qwen/settings.json")),
            AgentName::Crush if global => Some(xdg_config(home).join("crush/crush.json")),
            AgentName::Crush => Some(project_root.join(".crush.json")),
            // The VS Code extension's `.vscode/mcp.json` is IDE-managed, not ours.
            AgentName::Copilot if global => Some(home.join(".copilot/mcp-config.json")),
            AgentName::Copilot => None,
        }
    }

    /// The top-level key + entry shape this harness expects MCP servers under:
    /// mostly `mcpServers`, but OpenCode/Crush use `mcp` (`"local"` transport,
    /// argv array) and Zed `context_servers`. A wrong key fails silently — the
    /// harness never sees the server.
    pub fn mcp_key(self) -> mcp_config::ServerKey {
        match self {
            AgentName::Opencode | AgentName::Crush => mcp_config::ServerKey::Mcp,
            AgentName::Zed => mcp_config::ServerKey::ContextServers,
            _ => mcp_config::ServerKey::McpServers,
        }
    }

    /// The status-line label for this agent's guide target. Must fit
    /// `Mark::render`'s `LABEL_COL` or the row misaligns
    /// (`label_widths_fit_the_column`). Only the guide-file loop reads this;
    /// Claude's block labels its several targets itself.
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

    /// Content of a `Presence::Exists` guide file: GUIDE_MD, wrapped in `.mdc`
    /// frontmatter for Cursor (needs `alwaysApply` to inject it unprompted).
    pub fn guide_body(self) -> String {
        match self {
            AgentName::Cursor => format!(
                "---\ndescription: cona — token-efficient code navigation\nalwaysApply: true\n---\n\n{GUIDE_MD}"
            ),
            _ => GUIDE_MD.to_string(),
        }
    }

    /// Is cona wired into this agent for the given scope? Reflects an actual
    /// install, not mere presence of the agent (`detected`).
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
    /// no fixed path names these, so the probe scans (recursive, bounded).
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

/// Does the cona Claude Code plugin cover the sessions this SCOPE serves?
/// Where it is enabled, the installer's hooks, skill file and `.mcp.json`
/// entry are duplicates — every hook would fire twice.
///
/// Project scope is covered by a plugin enabled in either settings file, the
/// global scope only by the GLOBAL file — a plugin enabled in one repo must not
/// strip the home-level hooks other repos rely on. An unreadable/invalid file
/// counts as "no plugin", degrading to a normal install.
pub(crate) fn claude_plugin_enabled(project_root: &Path, home: &Path, global: bool) -> bool {
    let global_file = home.join(".claude/settings.json");
    let project_file = project_root.join(".claude/settings.json");
    let files: &[&Path] = if global {
        &[&global_file]
    } else {
        &[&global_file, &project_file]
    };
    files.iter().any(|p| {
        std::fs::read_to_string(p)
            .map(|t| plugin_enabled_in(&t))
            .unwrap_or(false)
    })
}

/// Parse half of `claude_plugin_enabled`: is `enabledPlugins` key `cona` or
/// `cona@<marketplace>` set to true?
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
/// THE single traversal of `AgentName::ALL × scopes × mcp_path`, shared by
/// `agents status` and `doctor`, so a new agent shows up in both from its
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
/// target set. Upgrades re-sync what IS installed, never what merely COULD be:
/// a detected-but-never-selected agent must not gain config from an upgrade.
pub fn installed_agents(project_root: &Path, home: &Path, global: bool) -> Vec<AgentName> {
    AgentName::ALL
        .into_iter()
        .filter(|a| a.installed(project_root, home, global))
        .collect()
}

/// Agents with a config target in this scope. THE scope-eligibility rule
/// (setup picker, interactive command, status), so a scope-less agent (Pi at
/// project scope) is filtered in ONE place.
pub fn agents_in_scope(project_root: &Path, home: &Path, global: bool) -> Vec<AgentName> {
    AgentName::ALL
        .into_iter()
        .filter(|a| !a.config_paths(project_root, home, global).is_empty())
        .collect()
}
