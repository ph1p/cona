//! Plugin payload consistency: skill, manifests, hook matchers, versions.

use std::path::Path;

/// The plugin ships its own copy of the skill because Claude Code loads it from
/// `plugin/skills/cona/SKILL.md`, while the installer bakes the root file in via
/// `include_str!`. Two sources for one text drift silently — pin them equal.
#[test]
fn plugin_skill_matches_the_canonical_one() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let canonical = std::fs::read_to_string(root.join("SKILL.md")).expect("root SKILL.md");
    let plugin =
        std::fs::read_to_string(root.join("plugin/skills/cona/SKILL.md")).expect("plugin SKILL.md");
    assert_eq!(
        canonical, plugin,
        "plugin/skills/cona/SKILL.md drifted from SKILL.md — \
         re-run: cp SKILL.md plugin/skills/cona/SKILL.md"
    );
}

/// The redirect tier only runs on tool calls the matcher admits, and the matcher
/// is written down twice: once derived from hook.rs (settings.json, via the
/// installer) and once by hand in the plugin's hooks.json. A drift between them
/// is invisible — the hook simply stops firing on that distribution path,
/// silently, with no error anywhere.
/// Load a repo-relative JSON file for the plugin-consistency tests below.
fn read_json(p: &str) -> serde_json::Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(
        &std::fs::read_to_string(root.join(p)).unwrap_or_else(|e| panic!("{p}: {e}")),
    )
    .unwrap_or_else(|e| panic!("{p} is not valid JSON: {e}"))
}

#[test]
fn plugin_hook_matcher_matches_the_installer() {
    let hooks = read_json("plugin/hooks/hooks.json");
    let pre = hooks["hooks"]["PreToolUse"]
        .as_array()
        .and_then(|a| a.first())
        .expect("a PreToolUse entry");
    assert_eq!(
        pre["matcher"].as_str(),
        Some(cona::hook::PRETOOL_MATCHER.as_str()),
        "plugin/hooks/hooks.json PreToolUse matcher drifted from \
         hook::PRETOOL_MATCHER"
    );
}

/// One plugin directory serves both harnesses: Claude Code reads
/// `.claude-plugin/plugin.json`, Codex reads `.codex-plugin/plugin.json`, and
/// both point at the SAME skills/hooks/mcp payload. If the two manifests
/// disagree about what they are describing, one harness ships something the
/// other does not.
#[test]
fn both_plugin_manifests_describe_the_same_plugin() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let claude = read_json("plugin/.claude-plugin/plugin.json");
    let codex = read_json("plugin/.codex-plugin/plugin.json");
    for key in ["name", "version", "description", "homepage", "license"] {
        assert_eq!(
            claude[key], codex[key],
            "plugin manifests disagree on `{key}`"
        );
    }
    // The Codex manifest names the shared payload explicitly (Claude Code
    // discovers it by convention), so those paths must actually exist.
    for key in ["skills", "mcpServers", "hooks"] {
        let rel = codex[key].as_str().unwrap_or_else(|| {
            panic!(".codex-plugin/plugin.json is missing `{key}`");
        });
        let path = root.join("plugin").join(rel.trim_start_matches("./"));
        assert!(path.exists(), "`{key}` points at a missing path: {rel}");
    }
}

/// Codex snapshots an installed plugin into `~/.codex/plugins/cache/<mkt>/
/// <plugin>/<version>/` — a manifest version frozen while the crate moves puts
/// every payload change into the SAME cache dir, defeating the re-add story.
/// release-plz bumps only Cargo.toml; `scripts/sync-plugin-version.sh` (run by
/// release-plz.yml) carries the bump into the manifests, and this test pins
/// the result so a missed sync fails CI instead of drifting silently.
#[test]
fn plugin_versions_match_the_crate() {
    let crate_ver = env!("CARGO_PKG_VERSION");
    for (p, ptr) in [
        ("plugin/.claude-plugin/plugin.json", "/version"),
        ("plugin/.codex-plugin/plugin.json", "/version"),
        (".claude-plugin/marketplace.json", "/plugins/0/version"),
    ] {
        assert_eq!(
            read_json(p).pointer(ptr).and_then(|v| v.as_str()),
            Some(crate_ver),
            "{p} version drifted from Cargo.toml — re-run: scripts/sync-plugin-version.sh"
        );
    }
}

/// Beyond the PreToolUse matcher (pinned above), the installer and the plugin
/// write the SAME hook set in two places: the PostToolUse reindex matcher, the
/// SessionStart command shape, and the re-nudge shell gate. A drift means one
/// distribution path silently stops firing that hook — pin the load-bearing
/// pieces of each entry.
#[test]
fn plugin_hooks_match_the_installer() {
    let hooks = read_json("plugin/hooks/hooks.json");

    let session = hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .expect("a SessionStart command");
    assert!(
        session.contains("index --quiet --session-start"),
        "plugin SessionStart lost the --session-start command: {session}"
    );

    let post = hooks["hooks"]["PostToolUse"]
        .as_array()
        .expect("PostToolUse entries");
    let reindex = post
        .iter()
        .find(|e| e["matcher"].is_string())
        .expect("a matched PostToolUse reindex entry");
    assert_eq!(
        reindex["matcher"].as_str(),
        Some(cona::hook::POSTTOOL_MATCHER),
        "plugin PostToolUse matcher drifted from hook::POSTTOOL_MATCHER"
    );
    assert!(
        reindex["hooks"][0]["command"]
            .as_str()
            .is_some_and(|c| c.contains("index --quiet") && !c.contains("--session-start")),
        "plugin PostToolUse reindex command drifted"
    );

    let renudge = post
        .iter()
        .find(|e| !e["matcher"].is_string())
        .expect("an unmatched PostToolUse re-nudge entry");
    assert!(
        renudge["hooks"][0]["command"]
            .as_str()
            .is_some_and(|c| c.contains(r#"[ "${CONA_RENUDGE_EVERY:-0}" -gt 0 ]"#)
                && c.contains("hook PostToolUse")),
        "plugin re-nudge entry lost its shell gate or command"
    );
}
