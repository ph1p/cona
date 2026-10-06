//! Registering cona as an MCP server in each agent harness's config.
//!
//! Wires the commands the guide/skill blocks teach as native MCP tools
//! (`cona mcp`). Two config shapes cover every harness:
//!
//! * JSON with a server map under one top-level key — Claude Code (`.mcp.json`),
//!   Cursor, Gemini CLI, Windsurf, Qwen, Copilot. The key is NOT universal
//!   (`mcp` for OpenCode/Crush, `context_servers` for Zed), and a wrong key is
//!   a silent no-op. See `ServerKey`.
//! * TOML with an `[mcp_servers.cona]` table — Codex (`~/.codex/config.toml`).
//!
//! Both writers are idempotent and touch ONLY the `cona` entry; foreign
//! servers, keys and (JSON, via `preserve_order`) key order stay untouched.

use super::{write_if_changed, Change};
use anyhow::{anyhow, bail, Result};
use std::path::Path;

/// The MCP server name cona registers under. Uninstall matches on it, so it
/// must stay stable.
pub const SERVER_NAME: &str = "cona";

/// The top-level key a harness keeps its MCP server map under. A wrong key
/// does not error — the harness never sees the server — so each agent names
/// its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServerKey {
    /// `{"mcpServers": {…}}` — the majority spelling.
    McpServers,
    /// `{"mcp": {…}}` — OpenCode, Crush.
    Mcp,
    /// `{"context_servers": {…}}` — Zed.
    ContextServers,
}

impl ServerKey {
    fn as_str(self) -> &'static str {
        match self {
            ServerKey::McpServers => "mcpServers",
            ServerKey::Mcp => "mcp",
            ServerKey::ContextServers => "context_servers",
        }
    }

    /// The entry shape under that key. Most take the stdio triple;
    /// OpenCode/Crush tag the transport `"local"` with `command` as an ARRAY
    /// (binary first); Zed adds `source: "custom"`.
    ///
    /// `exe` is the ABSOLUTE path (`agents::agent_exe`): an agent launched from
    /// a GUI often lacks the installing shell's `PATH`.
    fn entry(self, exe: &str) -> serde_json::Value {
        match self {
            ServerKey::McpServers => serde_json::json!({
                "type": "stdio",
                "command": exe,
                "args": ["mcp"],
            }),
            ServerKey::Mcp => serde_json::json!({
                "type": "local",
                "command": [exe, "mcp"],
                "enabled": true,
            }),
            ServerKey::ContextServers => serde_json::json!({
                "source": "custom",
                "command": exe,
                "args": ["mcp"],
            }),
        }
    }
}

/// Add/remove the cona entry in a JSON config carrying an `mcpServers` object.
///
/// Only `mcpServers.cona` is touched; `preserve_order` keeps the user's key
/// order. An unparsable config is an error, not an overwrite — same rule as
/// `claude_hooks` on settings.json (invariant 6).
///
/// Default-key shorthand for the tests; production uses `json_server_keyed`.
#[cfg(test)]
pub fn json_server(path: &Path, exe: &str, install: bool) -> Result<Change> {
    json_server_keyed(path, exe, install, ServerKey::McpServers)
}

/// `json_server` for a harness that spells the server map differently
/// (`ServerKey`). Same guarantees; only the key and entry shape move.
pub fn json_server_keyed(path: &Path, exe: &str, install: bool, key: ServerKey) -> Result<Change> {
    let existing = std::fs::read_to_string(path).ok();
    if !install && existing.is_none() {
        return Ok(Change::Unchanged);
    }
    let raw = existing.unwrap_or_else(|| "{}".into());
    // An empty file is a valid start, not a parse error (`touch .mcp.json`).
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
    let name = key.as_str();
    // Uninstall never creates the map: such a config stays byte-identical.
    if !install && !root.get(name).is_some_and(|v| v.is_object()) {
        return Ok(Change::Unchanged);
    }
    let servers = root
        .as_object_mut()
        .unwrap()
        .entry(name)
        .or_insert_with(|| serde_json::json!({}));
    let Some(servers) = servers.as_object_mut() else {
        bail!("{} '{name}' is not an object", path.display());
    };
    if install {
        servers.insert(SERVER_NAME.into(), key.entry(exe));
    } else if servers.remove(SERVER_NAME).is_none() {
        return Ok(Change::Unchanged);
    } else if servers.is_empty() {
        // leave no empty scaffold behind in a file we may have created
        root.as_object_mut().unwrap().remove(name);
    }
    // A file left as bare `{}` after uninstall was ours — remove it.
    if !install && root.as_object().is_some_and(|o| o.is_empty()) {
        std::fs::remove_file(path)?;
        return Ok(Change::Updated);
    }
    write_if_changed(path, &format!("{}\n", serde_json::to_string_pretty(&root)?))
}

/// Markers around the cona table in a TOML config. A parse/re-emit would drop
/// the user's comments and formatting, so the table is a marked block appended
/// at the end, like the markdown guides.
const TOML_BEGIN: &str = "# cona:begin (managed by cona — do not edit)";
const TOML_END: &str = "# cona:end";

/// Render the marked `[mcp_servers.cona]` block for a Codex-style config.
fn toml_block(exe: &str) -> String {
    // TOML basic strings take backslash escapes; escape Windows paths.
    let esc = exe.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{TOML_BEGIN}\n[mcp_servers.{SERVER_NAME}]\ncommand = \"{esc}\"\nargs = [\"mcp\"]\n{TOML_END}\n")
}

/// Strip the cona block (and the blank tail it leaves) from a TOML config.
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
/// Foreign tables are never touched.
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
    // Strip our old block (so a moved binary self-heals) and append after the
    // foreign config; stripping is a no-op without a block, so one path.
    let head = existing.map(|b| strip_toml_block(&b)).unwrap_or_default();
    let head = head.trim_end();
    let updated = if head.is_empty() {
        block
    } else {
        format!("{head}\n\n{block}")
    };
    write_if_changed(path, &updated)
}

/// Is cona registered as an MCP server in this config file (JSON key or TOML
/// marker)?
///
/// Substring, not a parse: this is on the auto-refresh hot path (every command
/// → `maybe_refresh_project_config` → `project_has_cona` → `installed`), where
/// parsing up to eight configs is real cost for a boolean. Same trade as
/// `Presence::Needle`; the writers still parse properly.
pub fn registered(path: &Path) -> bool {
    let Ok(body) = std::fs::read_to_string(path) else {
        return false;
    };
    if path.extension().and_then(|e| e.to_str()) == Some("toml") {
        return body.contains(TOML_BEGIN);
    }
    // Any of the three map spellings qualifies, keeping this a pure path→bool
    // probe; a config only carries the one its own harness reads.
    let keyed = [
        ServerKey::McpServers,
        ServerKey::Mcp,
        ServerKey::ContextServers,
    ]
    .iter()
    .any(|k| body.contains(k.as_str()));
    keyed && body.contains(&format!("\"{SERVER_NAME}\""))
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

    /// Each spelling must round-trip under its OWN key and be seen by
    /// `registered()`. A wrong key is silent, so only an assertion catches it.
    #[test]
    fn alternate_server_keys_round_trip_under_their_own_name() {
        for (key, name) in [
            (ServerKey::Mcp, "mcp"),
            (ServerKey::ContextServers, "context_servers"),
            (ServerKey::McpServers, "mcpServers"),
        ] {
            let dir = tmp(&format!("key-{name}"));
            let p = dir.join("config.json");
            std::fs::write(&p, r#"{"theme":"dark"}"#).unwrap();

            assert_eq!(
                json_server_keyed(&p, "/bin/cona", true, key).unwrap(),
                Change::Updated
            );
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            assert!(
                v[name][SERVER_NAME].is_object(),
                "{name}: entry not written under its own key"
            );
            assert!(registered(&p), "{name}: registered() missed the entry");
            // Foreign keys survive.
            assert_eq!(v["theme"], "dark");

            // Uninstall strips the entry AND the now-empty map.
            assert_eq!(
                json_server_keyed(&p, "/bin/cona", false, key).unwrap(),
                Change::Updated
            );
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            assert!(v.get(name).is_none(), "{name}: empty map left behind");
            assert_eq!(v["theme"], "dark");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A wrong entry shape fails as silently as a wrong key.
    #[test]
    fn entry_shapes_match_each_harness_contract() {
        let stdio = ServerKey::McpServers.entry("/bin/cona");
        assert_eq!(stdio["type"], "stdio");
        assert_eq!(stdio["command"], "/bin/cona");
        assert_eq!(stdio["args"], serde_json::json!(["mcp"]));

        let local = ServerKey::Mcp.entry("/bin/cona");
        assert_eq!(local["type"], "local");
        assert_eq!(local["command"], serde_json::json!(["/bin/cona", "mcp"]));
        assert_eq!(local["enabled"], true);

        let zed = ServerKey::ContextServers.entry("/bin/cona");
        assert_eq!(zed["source"], "custom");
        assert_eq!(zed["command"], "/bin/cona");
        assert_eq!(zed["args"], serde_json::json!(["mcp"]));
    }

    /// Uninstall must never CREATE the server map.
    #[test]
    fn uninstall_on_a_config_without_our_key_is_a_no_op() {
        let dir = tmp("nokey");
        let p = dir.join("settings.json");
        let before = "{\n  \"theme\": \"dark\"\n}\n";
        std::fs::write(&p, before).unwrap();
        for key in [
            ServerKey::McpServers,
            ServerKey::Mcp,
            ServerKey::ContextServers,
        ] {
            assert_eq!(
                json_server_keyed(&p, "/bin/cona", false, key).unwrap(),
                Change::Unchanged
            );
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
