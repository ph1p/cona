//! Selection and status surfaces: which agents an invocation targets
//! (`AgentSel`), the status table, and the interactive checklist that diffs
//! checked-now vs installed-before into a per-scope `ScopePlan`.

use super::registry::*;
use super::*;
use crate::ui;
use anyhow::{anyhow, Result};
use std::path::Path;

/// Which agents an invocation targets — THE selection rule: explicit names (or
/// `--all`) override detection; with neither, install only detected agents.
/// Uninstall ignores detection, so a leftover config is always removable.
pub(super) struct AgentSel {
    pub(super) names: Vec<AgentName>,
    pub(super) all: bool,
    pub(super) install: bool,
}

impl AgentSel {
    /// Should this agent be acted on? `detected` = its config dir/file exists.
    pub(super) fn want(&self, name: AgentName, detected: bool) -> bool {
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

/// `cona agents status` — what is wired where, per agent and scope (✓ / – /
/// n/a), plus the copy-paste commands to add or remove one.
pub fn cmd_agents_status(project_root: &Path) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    println!("{}\n", ui::bold("cona agents"));

    // One table row per agent: name, scope cells (guide + mcp), description.
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
        // MCP: on when registered in EITHER scope, n/a without an MCP config.
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

/// Interactive add/remove for single agents: a pre-checked checklist; newly
/// checked agents are installed, newly unchecked ones removed. TTY-only;
/// callers gate on that.
pub fn cmd_agents_interactive(project_root: &Path, global: bool) -> Result<()> {
    // One scope of the same checklist `cona setup` shows.
    let Some((proj, glob)) = pick_agents(project_root, !global, global)? else {
        println!("{}", ui::dim("cancelled — nothing changed"));
        return Ok(());
    };
    let plan = if global { glob } else { proj };
    if plan.add.is_empty() && plan.remove.is_empty() {
        println!("{}", ui::dim("no changes"));
        return Ok(());
    }
    if !plan.remove.is_empty() {
        cmd_agents(project_root, "uninstall", &plan.remove, false, global)?;
    }
    if !plan.add.is_empty() {
        cmd_agents(project_root, "install", &plan.add, false, global)?;
    }
    Ok(())
}

/// The agents to add and to remove within ONE scope, as decided by the picker.
#[derive(Default)]
pub struct ScopePlan {
    pub add: Vec<AgentName>,
    pub remove: Vec<AgentName>,
}

/// ONE agent checklist across the requested scopes, diffed into per-scope
/// plans. THE interactive manage surface, shared by `cona setup` (both scopes)
/// and `cona agents` (one scope) so pre-check policy and diff can't drift. A
/// row starts checked when installed, else when detected (first-run
/// suggestion). Unchecking an installed agent is a REMOVAL. `None` = cancelled.
pub fn pick_agents(
    root: &Path,
    do_project: bool,
    do_global: bool,
) -> Result<Option<(ScopePlan, ScopePlan)>> {
    let home = dirs::home_dir().unwrap_or_default();

    // `items[ordinal]` = (agent, global, was_installed) for the item row whose
    // ordinal `multiselect` returns. Descriptions carry the row's state, since
    // a checked box alone can't tell "installed" from "detected, suggested".
    let mut rows: Vec<ui::Row> = Vec::new();
    let mut items: Vec<(AgentName, bool, bool)> = Vec::new();
    for (global, header) in [
        (false, "PROJECT — this repo"),
        (true, "HOME — global configs (~/.claude, ~/.codex, …)"),
    ] {
        if (global && !do_global) || (!global && !do_project) {
            continue;
        }
        let scoped = agents_in_scope(root, &home, global);
        if scoped.is_empty() {
            continue;
        }
        if !rows.is_empty() {
            rows.push(ui::Row::Header("")); // spacer between sections
        }
        rows.push(ui::Row::Header(header));
        for a in scoped {
            let was = a.installed(root, &home, global);
            let on = was || a.detected(root, &home, global);
            rows.push(ui::Row::Item(a.slug(), a.state_desc(was, on).into(), on));
            items.push((a, global, was));
        }
    }

    match ui::multiselect("configure cona agents", &rows)? {
        None => Ok(None),
        Some(picked) => {
            // Newly on → add, newly off → remove. Still-on agents are
            // re-installed too (idempotent; refreshes stale marker blocks).
            let now_on: std::collections::HashSet<usize> = picked.into_iter().collect();
            let (mut proj, mut glob) = (ScopePlan::default(), ScopePlan::default());
            for (i, &(agent, global, was)) in items.iter().enumerate() {
                let plan = if global { &mut glob } else { &mut proj };
                if now_on.contains(&i) {
                    plan.add.push(agent);
                } else if was {
                    plan.remove.push(agent);
                }
            }
            Ok(Some((proj, glob)))
        }
    }
}
