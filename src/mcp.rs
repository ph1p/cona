//! MCP server framing — hand-rolled newline-delimited stdio JSON-RPC 2.0, no
//! SDK. Pure logic over a generic reader/writer so it is unit-testable.

use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// Protocol versions cona speaks, newest first — the newest is the answer to
/// an unknown request.
pub const SUPPORTED_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Negotiate per spec: echo the client's version if supported, else answer
/// with our latest (the client decides whether to proceed).
pub fn negotiate_protocol(requested: Option<&str>) -> &'static str {
    // one pass: return the &'static entry, else fall back to our latest
    requested
        .and_then(|v| SUPPORTED_PROTOCOLS.iter().copied().find(|&s| s == v))
        .unwrap_or(SUPPORTED_PROTOCOLS[0])
}

/// Schema builder for one tools/list entry (no behaviour annotations).
pub fn tool(name: &str, desc: &str, props: Value, req: &[&str]) -> Value {
    tool_annotated(name, desc, props, req, None)
}

/// Schema builder with an MCP `annotations` object (readOnly/destructive/
/// idempotent hints). `None` omits the field.
pub fn tool_annotated(
    name: &str,
    desc: &str,
    props: Value,
    req: &[&str],
    annotations: Option<Value>,
) -> Value {
    let mut v = json!({
        "name": name,
        "description": desc,
        "inputSchema": {"type": "object", "properties": props, "required": req}
    });
    if let Some(a) = annotations {
        v["annotations"] = a;
    }
    v
}

/// One tool's result: the text plus, when the tool declares an `outputSchema`,
/// the machine-readable form as `structuredContent`.
///
/// Text is never optional: the spec keeps `content` authoritative and
/// `structuredContent` is an addition, so a structured tool emits BOTH —
/// dropping text breaks every client that only reads content blocks.
pub struct ToolOut {
    pub text: String,
    pub structured: Option<Value>,
    /// Set by the disclosure gate: this call revealed the extended tools, so
    /// tools/list must start returning them and the client must be told.
    pub expand: bool,
}

impl ToolOut {
    /// Text-only result (no `outputSchema` on this tool).
    pub fn text(text: String) -> Self {
        Self {
            text,
            structured: None,
            expand: false,
        }
    }

    /// Text plus the structured payload matching the tool's `outputSchema`.
    pub fn structured(text: String, structured: Value) -> Self {
        Self {
            text,
            structured: Some(structured),
            expand: false,
        }
    }

    /// Mark this result as the one that unlocks the extended toolset.
    pub fn expanding(mut self) -> Self {
        self.expand = true;
        self
    }
}

impl From<String> for ToolOut {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

/// Attach an `outputSchema` to a tool entry built by `tool`/`tool_annotated`.
///
/// A declared schema is a CONTRACT: the server must then return conforming
/// `structuredContent`, so only wrap tools whose dispatch produces it.
pub fn with_output_schema(mut tool: Value, schema: Value) -> Value {
    tool["outputSchema"] = schema;
    tool
}

/// `outputSchema` for list-of-rows tools. MCP requires an object at the top
/// level, so the array rides in a named field.
pub fn rows_schema(field: &str, item_props: Value, desc: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            field: {"type": "array", "description": desc,
                    "items": {"type": "object", "properties": item_props}}
        },
        "required": [field]
    })
}

/// Read-only tool annotation: safe to call, no side effects, repeatable.
pub fn read_only(title: &str) -> Option<Value> {
    Some(
        json!({"title": title, "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false}),
    )
}

/// Writing tool annotation: mutates files, not read-only. `destructive` marks
/// tools that can overwrite existing content.
pub fn writes(title: &str, destructive: bool) -> Option<Value> {
    Some(
        json!({"title": title, "readOnlyHint": false, "destructiveHint": destructive, "openWorldHint": false}),
    )
}

/// Serve until the reader closes. `call(name, args)` runs one tool; tool
/// failures become isError results, never protocol errors. `instructions` is
/// the optional preamble in the initialize result (`None` omits it).
///
/// `tools(expanded)` builds tools/list for the disclosure tier (`false` = core,
/// `true` = all). A result with [`ToolOut::expand`] flips the connection and
/// emits `notifications/tools/list_changed` — the ONLY way a client learns of
/// the extra tools, since it may only call what tools/list returned. Hence
/// also `listChanged`: without the notification a client never re-lists.
pub fn serve<R: BufRead, W: Write>(
    reader: R,
    mut writer: W,
    tools: impl Fn(bool) -> Vec<Value>,
    instructions: Option<&str>,
    mut call: impl FnMut(&str, &Value) -> Result<ToolOut>,
) -> Result<()> {
    let mut expanded = false;
    let reply = |w: &mut W, id: Value, body: Result<Value, (i64, String)>| -> Result<()> {
        let msg = match body {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        serde_json::to_writer(&mut *w, &msg)?;
        w.write_all(b"\n")?;
        w.flush()?;
        Ok(())
    };

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            // JSON-RPC 2.0: a parse error must be answered (-32700, id null) —
            // silently dropping it leaves the client waiting forever on its id
            reply(
                &mut writer,
                Value::Null,
                Err((-32700, "parse error".into())),
            )?;
            continue;
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        let Some(id) = msg.get("id").cloned() else {
            continue; // notification (e.g. notifications/initialized) — no reply
        };
        match method {
            "initialize" => {
                let proto =
                    negotiate_protocol(params.get("protocolVersion").and_then(|v| v.as_str()));
                let mut result = json!({
                    "protocolVersion": proto,
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {
                        "name": "cona",
                        "title": "cona — code navigation",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                });
                if let Some(instructions) = instructions {
                    result["instructions"] = json!(instructions);
                }
                reply(&mut writer, id, Ok(result))?;
            }
            "ping" => reply(&mut writer, id, Ok(json!({})))?,
            "tools/list" => reply(&mut writer, id, Ok(json!({"tools": tools(expanded)})))?,
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let mut newly_expanded = false;
                let result = match call(name, &args) {
                    Ok(out) => {
                        newly_expanded = out.expand && !expanded;
                        expanded |= out.expand;
                        let mut r = json!({"content": [{"type": "text", "text": out.text}]});
                        if let Some(sc) = out.structured {
                            r["structuredContent"] = sc;
                        }
                        r
                    }
                    Err(e) => json!({
                        "content": [{"type": "text", "text": format!("error: {e}")}],
                        "isError": true
                    }),
                };
                reply(&mut writer, id, Ok(result))?;
                // After the result, so a client that re-lists on the notification
                // sees the call it triggered already answered.
                if newly_expanded {
                    serde_json::to_writer(
                        &mut writer,
                        &json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}),
                    )?;
                    writer.write_all(b"\n")?;
                    writer.flush()?;
                }
            }
            other => reply(
                &mut writer,
                id,
                Err((-32601, format!("method not found: {other}"))),
            )?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn run(input: &str) -> Vec<Value> {
        run_with(input, None)
    }

    fn run_with(input: &str, instructions: Option<&str>) -> Vec<Value> {
        let mut out = Vec::new();
        // Two tiers, so the disclosure path is exercised: `gate` unlocks `extra`.
        let tools = |expanded: bool| {
            let mut t = vec![
                tool("echo", "echo text", json!({}), &[]),
                tool("gate", "unlock more", json!({}), &[]),
            ];
            if expanded {
                t.push(tool("extra", "unlocked tool", json!({}), &[]));
            }
            t
        };
        serve(
            input.as_bytes(),
            &mut out,
            tools,
            instructions,
            |name, args| match name {
                "echo" => Ok(ToolOut::text(
                    args["text"].as_str().unwrap_or("").to_string(),
                )),
                "gate" => Ok(ToolOut::text("unlocked".into()).expanding()),
                _ => Err(anyhow!("unknown tool '{name}'")),
            },
        )
        .unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn handshake_notifications_and_calls() {
        let replies = run(concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"}}}"#,
            "\n",
        ));
        assert_eq!(replies.len(), 3); // notification gets no reply
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "cona");
        assert_eq!(replies[1]["result"]["tools"][0]["name"], "echo");
        assert_eq!(replies[2]["result"]["content"][0]["text"], "hi");
        // no instructions passed → field omitted
        assert!(replies[0]["result"].get("instructions").is_none());
    }

    #[test]
    fn initialize_carries_instructions_when_present() {
        let replies = run_with(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
                "\n",
            ),
            Some("use outline then show"),
        );
        assert_eq!(
            replies[0]["result"]["instructions"],
            "use outline then show"
        );
    }

    #[test]
    fn protocol_version_negotiation() {
        // supported version → echoed
        assert_eq!(negotiate_protocol(Some("2025-03-26")), "2025-03-26");
        assert_eq!(negotiate_protocol(Some("2024-11-05")), "2024-11-05");
        assert_eq!(negotiate_protocol(Some("2025-11-25")), "2025-11-25");
        // unknown / missing → our latest supported
        assert_eq!(negotiate_protocol(Some("1.0.0")), "2025-11-25");
        assert_eq!(negotiate_protocol(None), "2025-11-25");
    }

    #[test]
    fn initialize_downgrades_unsupported_version_and_reports_title() {
        let replies = run_with(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"9999-01-01"}}"#,
                "\n",
            ),
            None,
        );
        // never echo an unsupported version — answer with our latest
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(
            replies[0]["result"]["serverInfo"]["title"],
            "cona — code navigation"
        );
    }

    #[test]
    fn tool_error_is_iserror_result_not_protocol_error() {
        let replies = run(concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"nope"}}"#,
            "\n",
        ));
        assert_eq!(replies[0]["result"]["isError"], true);
        assert!(replies[0].get("error").is_none());
    }

    #[test]
    fn malformed_json_is_32700_unknown_method_is_32601() {
        let replies = run(concat!(
            "this is not json\n",
            r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#,
            "\n",
        ));
        // JSON-RPC 2.0: parse errors are answered (-32700, id null), not dropped
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["error"]["code"], -32700);
        assert_eq!(replies[0]["id"], serde_json::Value::Null);
        assert_eq!(replies[1]["error"]["code"], -32601);
    }

    #[test]
    fn disclosure_gate_expands_tools_list_and_notifies() {
        let msgs = run(concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"gate"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
            "\n",
        ));
        let names = |m: &Value| -> Vec<String> {
            m["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect()
        };
        let replies: Vec<&Value> = msgs.iter().filter(|m| m.get("id").is_some()).collect();
        // before: the gate is listed, what it unlocks is not
        assert!(!names(replies[0]).contains(&"extra".to_string()));
        // after: the unlocked tool is listed, so a client may call it
        assert!(names(replies[2]).contains(&"extra".to_string()));

        // and the client was TOLD to re-list, else it never sees `extra`
        let notes: Vec<&str> = msgs
            .iter()
            .filter(|m| m.get("id").is_none())
            .map(|m| m["method"].as_str().unwrap())
            .collect();
        assert_eq!(notes, vec!["notifications/tools/list_changed"]);
    }

    #[test]
    fn disclosure_notification_follows_the_result_and_fires_once() {
        let msgs = run(concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"gate"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"gate"}}"#,
            "\n",
        ));
        // the result comes first: a client that re-lists on the notification finds
        // the call that triggered it already answered
        assert_eq!(msgs[0]["id"], 1);
        assert_eq!(msgs[1]["method"], "notifications/tools/list_changed");
        // already expanded — re-calling the gate must not re-notify
        assert_eq!(
            msgs.iter()
                .filter(|m| m["method"] == "notifications/tools/list_changed")
                .count(),
            1
        );
    }

    #[test]
    fn tool_props_are_a_property_map_not_a_schema_fragment() {
        // `props` nests under "properties": a schema fragment like
        // {"type":"object"} would declare a property named "type" — invalid,
        // and clients then drop the ENTIRE tools/list. Each value must be an object.
        for t in [
            tool("a", "d", json!({}), &[]),
            tool("b", "d", json!({"x": {"type": "string"}}), &["x"]),
        ] {
            let props = t["inputSchema"]["properties"].as_object().unwrap();
            for (name, spec) in props {
                assert!(
                    spec.is_object(),
                    "property {name} is not a schema object: {spec}"
                );
                assert!(
                    !matches!(name.as_str(), "type" | "additionalProperties" | "required"),
                    "schema keyword {name} leaked into the property map"
                );
            }
        }
    }
}
