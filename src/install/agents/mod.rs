//! Agent integration: inject the usage guide + skill + hooks + MCP entry into
//! agent configs — idempotent, marker-based, uninstallable.
//!
//! `AgentName` is the roster; every per-agent fact (where its config lives,
//! how to tell it is there, which MCP key it speaks) hangs off that enum rather
//! than a list repeated per call site, so adding a harness is one variant plus
//! its match arms. Deliberately NOT enumerated in prose here — a hand-kept list
//! in a doc comment is the first thing to go stale. `cona agents --help` prints
//! the live set.

use crate::db;
use std::path::Path;

/// Compact variant for CLAUDE.md / AGENTS.md / rule files.
pub const GUIDE_MD: &str = r#"## cona — code navigation (use FIRST, not as fallback)

In a cona-indexed repo, cona IS how you read and search code. It reads one
symbol instead of a whole file and searches identifier nodes instead of raw
text, so it replaces the generic read-the-file / grep-the-tree habit:

- Need ONE function/class/method → `cona show <Sym>`, or `cona context <Sym>`
  for source + callees + call sites. Do not read a whole file for one symbol.
- Searching for an identifier (definition, call site, usage) →
  `cona grep <name>` / `cona refs <Name>`. Do not run plain grep/rg over
  source trees for identifiers.
- Opening an unfamiliar file → `cona outline <file>` first; read the full
  file only if the outline shows you truly need every line.
- Orienting in an unknown repo → `cona tree --rank`.

Whole-file reads are for files you are about to rewrite, unindexed files, and
non-code (docs, configs, data). `cona edit <Sym>` writes syntax-verified.

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

mod apply;
mod registry;
mod select;
#[cfg(test)]
mod tests;

pub use apply::{cmd_agents, cmd_agents_q, project_has_cona};
pub(crate) use registry::claude_plugin_enabled;
pub use registry::{
    agents_in_scope, detected_agents, installed_agents, mcp_registrations, AgentName, Presence,
};
pub use select::{cmd_agents_interactive, cmd_agents_status, pick_agents, ScopePlan};
