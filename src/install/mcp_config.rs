//! Registering cona as an MCP server in each agent harness's config.
//!
//! The guide/skill blocks teach an agent to *shell out* to `cona`; this wires
//! the same commands in as native MCP tools (`cona mcp`) for the harnesses that
//! speak MCP. Two config shapes cover every one of them:
//!
//! * JSON with an `mcpServers` object — Claude Code (`.mcp.json`, the
//!   checked-in project scope), Cursor (`.cursor/mcp.json`), Gemini CLI
//!   (`.gemini/settings.json`). Codex is the exception (TOML, below).
//! * TOML with an `[mcp_servers.cona]` table — Codex (`~/.codex/config.toml`).
//!
//! Both writers are idempotent and surgical: they touch ONLY the `cona` entry
//! and leave every foreign server, key and (for JSON, via serde_json's
//! `preserve_order`) key order untouched. Uninstall removes exactly that entry.

use super::{write_if_changed, Change};
use anyhow::{anyhow, bail, Result};
use std::path::Path;

/// The MCP server name cona registers under. Also the identity uninstall
/// matches on, so it must stay stable.
pub const SERVER_NAME: &str = "cona";

/// The stdio server entry every harness gets: `<cona binary> mcp`. `exe` is the
/// resolved absolute path (see `agents::agent_exe`) so the harness does not
/// depend on cona being on ITS `PATH` — an agent launched from a GUI often has
/// a different environment than the shell cona was installed from.
fn server_entry(exe: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "stdio",
        "command": exe,
        "args": ["mcp"],
    })
}

/// Add/remove the cona entry in a JSON config carrying an `mcpServers` object.
/// Returns the `Change` so callers can report Created/Updated/Unchanged like
/// every other install target.
///
/// Foreign content is preserved: an existing file is parsed and re-serialized
/// (serde_json's `preserve_order` keeps the user's key order), and only the
/// `mcpServers.cona` key is touched. An unparsable config is an error rather
/// than an overwrite — same rule as `claude_hooks` on settings.json
/// (invariant 6: never clobber foreign file content).
pub fn json_server(path: &Path, exe: &str, install: bool) -> Result<Change> {
    let existing = std::fs::read_to_string(path).ok();
    if !install && existing.is_none() {
        return Ok(Change::Unchanged);
    }
    let raw = existing.unwrap_or_else(|| "{}".into());
    // An empty (or whitespace-only) file is a valid starting point, not a
    // parse error — `touch .mcp.json` is a thing users do.
    let mut root: serde_json::Value = if raw.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&raw).map_err(|e| {
            anyhow!(
                "{} is not valid JSON ({e}) — fix it or add the cona MCP server manually",
                path.display()
            )
        })?
    };
    if !root.is_object() {
        bail!("{} top level is not an object", path.display());
    }
    let servers = root
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let Some(servers) = servers.as_object_mut() else {
        bail!("{} 'mcpServers' is not an object", path.display());
    };
    if install {
        servers.insert(SERVER_NAME.into(), server_entry(exe));
    } else if servers.remove(SERVER_NAME).is_none() {
        return Ok(Change::Unchanged);
    } else if servers.is_empty() {
        // leave no empty scaffold behind in a file we may have created
        root.as_object_mut().unwrap().remove("mcpServers");
    }
    // A file that would be left with nothing but `{}` after uninstall was ours
    // to begin with — remove it rather than littering an empty config.
    if !install && root.as_object().is_some_and(|o| o.is_empty()) {
        std::fs::remove_file(path)?;
        return Ok(Change::Updated);
    }
    write_if_changed(path, &format!("{}\n", serde_json::to_string_pretty(&root)?))
}

/// Marker comments delimiting the cona table in a TOML config. TOML has no
/// dependency-free structural editor here, and a whole-file parse/re-emit would
/// discard the user's comments and formatting — so the cona table is a marked
/// block appended at the end, the same discipline as the markdown guides.
const TOML_BEGIN: &str = "# cona:begin (managed by cona — do not edit)";
const TOML_END: &str = "# cona:end";

/// Render the marked `[mcp_servers.cona]` block for a Codex-style config.
fn toml_block(exe: &str) -> String {
    // TOML basic strings take backslash escapes — a Windows path would
    // otherwise smuggle escape sequences into the value.
    let esc = exe.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{TOML_BEGIN}\n[mcp_servers.{SERVER_NAME}]\ncommand = \"{esc}\"\nargs = [\"mcp\"]\n{TOML_END}\n")
}

/// Strip the cona block (and any trailing blank run it leaves) from a TOML
/// config. Pure — tested. Returns the config without the block.
fn strip_toml_block(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut skipping = false;
    for line in body.lines() {
        if line.trim_end() == TOML_BEGIN {
            skipping = true;
            continue;
        }
        if skipping {
            if line.trim_end() == TOML_END {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    // collapse the blank tail the removal may leave behind
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Add/remove the `[mcp_servers.cona]` block in a Codex-style TOML config.
/// Idempotent: an existing block is replaced in place (so a moved binary
/// self-heals), foreign tables are never touched.
pub fn toml_server(path: &Path, exe: &str, install: bool) -> Result<Change> {
    let existing = std::fs::read_to_string(path).ok();
    if !install {
        let Some(body) = existing else {
            return Ok(Change::Unchanged);
        };
        if !body.contains(TOML_BEGIN) {
            return Ok(Change::Unchanged);
        }
        let stripped = strip_toml_block(&body);
        if stripped.trim().is_empty() {
            std::fs::remove_file(path)?;
            return Ok(Change::Updated);
        }
        return write_if_changed(path, &stripped);
    }
    let block = toml_block(exe);
    // Strip any block we already own (replace in place, so a moved binary
    // self-heals) and append after whatever foreign config remains. Stripping
    // is a no-op on a config that has no cona block, so both cases are one path.
    let head = existing.map(|b| strip_toml_block(&b)).unwrap_or_default();
    let head = head.trim_end();
    let updated = if head.is_empty() {
        block
    } else {
        format!("{head}\n\n{block}")
    };
    write_if_changed(path, &updated)
}

/// Is cona registered as an MCP server in this config file? One probe for both
/// shapes — the JSON files carry a `"cona"` key under `mcpServers`, the TOML one
/// carries the marker.
///
/// Substring, not a parse: this sits on the auto-refresh hot path (every command
/// → `maybe_refresh_project_config` → `project_has_cona` → `installed`), where a
/// `serde_json` parse of up to eight harness configs per invocation is real cost
/// for a boolean. Same trade the existing `Presence::Needle` probe makes, and
/// the needles are specific — `"mcpServers"` plus a quoted `"cona"` key. The
/// writers still parse properly; only this yes/no answer is approximate.
pub fn registered(path: &Path) -> bool {
    let Ok(body) = std::fs::read_to_string(path) else {
        return false;
    };
    if path.extension().and_then(|e| e.to_str()) == Some("toml") {
        return body.contains(TOML_BEGIN);
    }
    body.contains("mcpServers") && body.contains(&format!("\"{SERVER_NAME}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cona-mcpcfg-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn json_install_is_idempotent_and_preserves_foreign_servers() {
        let dir = tmp("json");
        let p = dir.join(".mcp.json");
        std::fs::write(
            &p,
            r#"{"mcpServers":{"other":{"command":"x"}},"extra":true}"#,
        )
        .unwrap();
        assert_eq!(json_server(&p, "/bin/cona", true).unwrap(), Change::Updated);
        // second run with identical content changes nothing
        assert_eq!(
            json_server(&p, "/bin/cona", true).unwrap(),
            Change::Unchanged
        );
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["cona"]["command"], "/bin/cona");
        assert_eq!(v["mcpServers"]["cona"]["args"][0], "mcp");
        // foreign entries survive
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["extra"], true);
        // uninstall removes ONLY ours, file stays (foreign content remains)
        assert!(matches!(
            json_server(&p, "/bin/cona", false).unwrap(),
            Change::Updated
        ));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v["mcpServers"].get("cona").is_none());
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_uninstall_removes_a_file_that_held_only_cona() {
        let dir = tmp("jsonsolo");
        let p = dir.join(".mcp.json");
        json_server(&p, "/bin/cona", true).unwrap();
        assert!(p.exists());
        json_server(&p, "/bin/cona", false).unwrap();
        assert!(
            !p.exists(),
            "a file only we created must not be left behind"
        );
        // uninstall on a missing file is a no-op, never an error
        assert_eq!(
            json_server(&p, "/bin/cona", false).unwrap(),
            Change::Unchanged
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_refuses_to_clobber_invalid_config() {
        let dir = tmp("jsonbad");
        let p = dir.join(".mcp.json");
        std::fs::write(&p, "{not json").unwrap();
        assert!(json_server(&p, "/bin/cona", true).is_err());
        // untouched
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_accepts_an_empty_file() {
        let dir = tmp("jsonempty");
        let p = dir.join(".mcp.json");
        std::fs::write(&p, "   \n").unwrap();
        assert!(json_server(&p, "/bin/cona", true).is_ok());
        assert!(registered(&p));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toml_block_roundtrips_and_keeps_foreign_tables() {
        let dir = tmp("toml");
        let p = dir.join("config.toml");
        std::fs::write(
            &p,
            "model = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();
        assert_eq!(toml_server(&p, "/bin/cona", true).unwrap(), Change::Updated);
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("[mcp_servers.cona]"));
        assert!(body.contains("command = \"/bin/cona\""));
        assert!(body.contains("[mcp_servers.other]"));
        assert!(body.starts_with("model = \"o3\""));
        assert!(registered(&p));
        // re-install with the same exe is a no-op
        assert_eq!(
            toml_server(&p, "/bin/cona", true).unwrap(),
            Change::Unchanged
        );
        // a moved binary self-heals in place (one block, new path)
        toml_server(&p, "/opt/cona", true).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert_eq!(body.matches("[mcp_servers.cona]").count(), 1);
        assert!(body.contains("/opt/cona"));
        // uninstall leaves the foreign config intact
        toml_server(&p, "/opt/cona", false).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(!body.contains("cona"));
        assert!(body.contains("[mcp_servers.other]"));
        assert!(!registered(&p));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toml_uninstall_removes_a_file_that_held_only_cona() {
        let dir = tmp("tomlsolo");
        let p = dir.join("config.toml");
        toml_server(&p, "/bin/cona", true).unwrap();
        toml_server(&p, "/bin/cona", false).unwrap();
        assert!(!p.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toml_paths_with_backslashes_stay_escaped() {
        let block = toml_block(r"C:\bin\cona.exe");
        assert!(block.contains(r#"command = "C:\\bin\\cona.exe""#));
    }

    #[test]
    fn registered_is_false_for_missing_or_foreign_config() {
        let dir = tmp("probe");
        assert!(!registered(&dir.join("nope.json")));
        let p = dir.join("other.json");
        std::fs::write(&p, r#"{"mcpServers":{"other":{}}}"#).unwrap();
        assert!(!registered(&p));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
