use super::apply::*;
use super::registry::*;
use super::select::*;
use super::*;
use std::path::PathBuf;

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
        format!("# my project\n{}\nguide\n", crate::install::BLOCK_BEGIN),
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
    let first = cmd_agents_q(&dir, "install", &[AgentName::Claude], false, false, true).unwrap();
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
        second = cmd_agents_q(&dir, "install", &[AgentName::Claude], false, false, true).unwrap();
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
        format!("{}\nguide\n", crate::install::BLOCK_BEGIN),
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
            crate::install::BLOCK_BEGIN
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
            label.len() <= crate::install::LABEL_COL,
            "{} label {label:?} is {} chars, over the {}-char column",
            a.slug(),
            label.len(),
            crate::install::LABEL_COL
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
    prune_empty_dirs(&deep, &tmp.join("proj"));
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
    prune_empty_dirs(&ours, &tmp.join("proj2"));
    assert!(
        ours.parent().unwrap().exists(),
        "a directory still holding the user's file must survive"
    );

    // Anchored at the file's own dir: nothing above it may be touched.
    let shallow = tmp.join("proj3/only.md");
    std::fs::create_dir_all(shallow.parent().unwrap()).unwrap();
    std::fs::write(&shallow, "x").unwrap();
    std::fs::remove_file(&shallow).unwrap();
    prune_empty_dirs(&shallow, &tmp.join("proj3"));
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
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
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
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
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
#[test]
fn plugin_enabled_in_detects_only_an_enabled_cona_plugin() {
    assert!(plugin_enabled_in(
        r#"{"enabledPlugins":{"cona@cona":true}}"#
    ));
    assert!(plugin_enabled_in(
        r#"{"enabledPlugins":{"other@x":false,"cona@some-marketplace":true}}"#
    ));
    assert!(
        !plugin_enabled_in(r#"{"enabledPlugins":{"cona@cona":false}}"#),
        "a disabled plugin provides nothing"
    );
    assert!(
        !plugin_enabled_in(r#"{"enabledPlugins":{"corona@cona":true}}"#),
        "only `cona` / `cona@…` keys count, not other plugins that contain the word"
    );
    assert!(!plugin_enabled_in(r#"{"enabledPlugins":{}}"#));
    assert!(!plugin_enabled_in("{}"));
    assert!(
        !plugin_enabled_in("not json"),
        "an invalid settings file degrades to a normal install, never a skipped one"
    );
}

#[test]
fn claude_plugin_enabled_reads_global_and_project_settings() {
    let tmp = std::env::temp_dir().join(format!("cona-plugin-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let (home, proj) = (tmp.join("home"), tmp.join("proj"));
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(proj.join(".claude")).unwrap();
    assert!(!claude_plugin_enabled(&proj, &home), "no settings at all");

    let on = r#"{"enabledPlugins":{"cona@cona":true}}"#;
    std::fs::write(home.join(".claude/settings.json"), on).unwrap();
    assert!(claude_plugin_enabled(&proj, &home), "global settings count");

    std::fs::remove_file(home.join(".claude/settings.json")).unwrap();
    std::fs::write(proj.join(".claude/settings.json"), on).unwrap();
    assert!(
        claude_plugin_enabled(&proj, &home),
        "project settings count too"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
