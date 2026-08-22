//! MCP stdio handshake + tool-schema shape.

#[test]
fn mcp_stdio_handshake_and_tools_list() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let data = std::env::temp_dir().join(format!("cona-data-mcp-{}", std::process::id()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_cona"))
        .arg("mcp")
        .env("CONA_DATA_DIR", &data)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"more","arguments":{}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"find","arguments":{"name":"hello"}}}"#,
                "\n",
                // after `more`, the extended tools must appear in tools/list —
                // a client may only call what tools/list returned
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/list"}"#,
                "\n",
            )
            .as_bytes(),
        )
        .unwrap();
    drop(stdin); // EOF ends the serve loop
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let all: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // Server-initiated notifications are interleaved with the replies; keep them
    // apart so the replies stay addressable by request order.
    let notes: Vec<&str> = all
        .iter()
        .filter(|m| m.get("id").is_none())
        .map(|m| m["method"].as_str().unwrap_or(""))
        .collect();
    let lines: Vec<serde_json::Value> = all
        .iter()
        .filter(|m| m.get("id").is_some())
        .cloned()
        .collect();
    assert_eq!(lines.len(), 5); // our own notifications/initialized gets no reply
                                // Unlocking the extended tier MUST announce itself: a client that is never
                                // told to re-list can never call the tools `more` just revealed.
    assert!(
        notes.contains(&"notifications/tools/list_changed"),
        "no list_changed after `more`: {notes:?}"
    );
    assert_eq!(
        lines[0]["result"]["capabilities"]["tools"]["listChanged"],
        true
    );
    assert_eq!(lines[0]["result"]["serverInfo"]["name"], "cona");
    // a supported protocol version is echoed back verbatim (negotiation)
    assert_eq!(lines[0]["result"]["protocolVersion"], "2025-03-26");
    assert!(lines[0]["result"]["serverInfo"]["title"].is_string());
    // initialize carries the server preamble teaching the cona workflow
    let instructions = lines[0]["result"]["instructions"]
        .as_str()
        .expect("initialize result should carry instructions");
    assert!(
        instructions.contains("outline") && instructions.contains("show"),
        "{instructions:?}"
    );
    let tools: Vec<&str> = lines[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        tools.contains(&"find") && tools.contains(&"edit"),
        "{tools:?}"
    );
    // Progressive disclosure: tools/list carries the core tier plus the `more`
    // gate, NOT the full set — the schemas are re-sent on every request, so the
    // advanced tail is disclosed on demand instead.
    assert!(
        tools.contains(&"more"),
        "missing disclosure gate: {tools:?}"
    );
    assert!(
        tools.len() < 12,
        "tools/list should stay small, got {}: {tools:?}",
        tools.len()
    );

    // Parity is total once expanded, and the EXPANDED tools/list is what proves
    // it: `more`'s text payload is advisory, but only a listed tool is callable.
    let expanded: Vec<&str> = lines[4]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let more = lines[2]["result"]["content"][0]["text"].as_str().unwrap();
    let gated: serde_json::Value = serde_json::from_str(&more[more.find('[').unwrap()..]).unwrap();
    let gated: Vec<&str> = gated
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for t in [
        "insert",
        "batch_edit",
        "check",
        "impact",
        "callers",
        "callees",
        "path",
        "deps",
        "shape",
        "entries",
        "tests",
        "note",
    ] {
        assert!(gated.contains(&t), "missing MCP tool {t}: {gated:?}");
        assert!(
            expanded.contains(&t),
            "{t} described by `more` but absent from the expanded tools/list, \
             so no client can call it: {expanded:?}"
        );
    }
    // A tool is disclosed by exactly one tier, never both.
    for t in &tools {
        assert!(!gated.contains(t), "tool {t} disclosed twice");
    }
    // Every core tool survives expansion, and the spent gate is retired.
    for t in &tools {
        if *t == "more" {
            assert!(!expanded.contains(t), "spent `more` gate still listed");
        } else {
            assert!(expanded.contains(t), "core tool {t} lost on expansion");
        }
    }

    // behaviour annotations: read-only queries vs writing tools. Core tools come
    // from tools/list, gated ones from the `more` payload — annotations must
    // survive disclosure, since that is the only place a client ever sees them.
    let ann = |name: &str| -> serde_json::Value {
        let from_list = lines[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .cloned();
        let from_more = || {
            serde_json::from_str::<serde_json::Value>(&more[more.find('[').unwrap()..])
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .cloned()
                .unwrap()
        };
        from_list.unwrap_or_else(from_more)["annotations"].clone()
    };
    assert_eq!(ann("show")["readOnlyHint"], true);
    assert_eq!(ann("edit")["readOnlyHint"], false);
    assert_eq!(ann("edit")["destructiveHint"], true);
    assert_eq!(ann("insert")["destructiveHint"], false);

    // A declared outputSchema is a contract: the tool must return matching
    // structuredContent, so schema and payload are asserted together.
    let find_tool = lines[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "find")
        .unwrap()
        .clone();
    assert_eq!(find_tool["outputSchema"]["type"], "object");
    assert!(find_tool["outputSchema"]["properties"]["symbols"].is_object());
    assert!(
        lines[3]["result"]["structuredContent"]["symbols"].is_array(),
        "find must return structuredContent: {:?}",
        lines[3]["result"]
    );
}

/// A malformed inputSchema is not a soft failure: clients reject the whole
/// tools/list, so ONE bad tool silently removes every cona tool from the
/// session. Validate the shape of all of them, both tiers.
#[test]
fn every_mcp_tool_schema_is_well_formed() {
    for expanded in [false, true] {
        for t in cona::commands::mcp_server::mcp_tools(expanded) {
            let name = t["name"].as_str().expect("tool needs a name");
            let schema = &t["inputSchema"];
            assert_eq!(schema["type"], "object", "{name}: inputSchema not object");
            let props = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{name}: properties missing/not an object"));
            for (prop, spec) in props {
                let spec = spec
                    .as_object()
                    .unwrap_or_else(|| panic!("{name}.{prop} is not a schema object: {spec}"));
                assert!(
                    spec.contains_key("type"),
                    "{name}.{prop} has no declared type"
                );
            }
            // `required` must name declared properties, or a client can reject a
            // call it has no way to satisfy.
            for r in t["inputSchema"]["required"].as_array().unwrap() {
                let r = r.as_str().unwrap();
                assert!(props.contains_key(r), "{name}: required {r} not declared");
            }
            if let Some(out) = t.get("outputSchema") {
                assert_eq!(out["type"], "object", "{name}: outputSchema not object");
            }
        }
    }
}
