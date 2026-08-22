//! Selection and status surfaces: which agents an invocation targets
//! (`AgentSel`), the status table, and the interactive checklist that diffs
//! checked-now vs installed-before into a per-scope `ScopePlan`.

use super::registry::*;
use super::*;
use crate::ui;
use anyhow::{anyhow, Result};
use std::path::Path;

/// Which agents a given invocation targets. Encodes the selection rule in ONE
/// place: explicit names (or `--all`) override detection; with neither, an
/// agent is configured only when its config is detected on disk. Uninstall
/// runs every requested agent regardless of detection (so a leftover config
/// is always removable).
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
    // One scope of the same checklist `cona setup` shows — same pre-checks
    // (installed OR detected), same diff, same refresh-on-still-checked.
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
/// plans. THE interactive manage surface — `cona setup` (both scopes) and
/// `cona agents` (one scope) share it, so the two can never drift in
/// pre-check policy or diff semantics. A row starts checked when the agent is
/// already installed in that scope, else when it is merely detected on disk
/// (the first-run suggestion). Unchecking an installed agent is a REMOVAL —
/// the picker doubles as the manage surface, so it must be able to take
/// integrations away, not only add them. `None` = user cancelled.
pub fn pick_agents(
    root: &Path,
    do_project: bool,
    do_global: bool,
) -> Result<Option<(ScopePlan, ScopePlan)>> {
    let home = dirs::home_dir().unwrap_or_default();

    // `items[ordinal]` = the (agent, global, was_installed) that item row maps
    // back to; the ordinal is exactly what `multiselect` hands back for
    // checked rows. Descriptions carry the row's current state — a pre-checked
    // box alone can't tell "already installed (uncheck = remove)" apart from
    // "detected, suggested".
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
            // Diff checked-now against installed-before: newly on → add,
            // newly off → remove. Still-on agents are re-installed too —
            // idempotent, and it refreshes marker blocks after a version bump.
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
