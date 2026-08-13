//! MCP server framing — hand-rolled stdio JSON-RPC 2.0, newline-delimited,
//! no SDK dependency. Pure protocol logic over generic reader/writer so it is
//! unit-testable; the binary wires stdin/stdout and the tool dispatch in.

use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// Protocol versions cona speaks, newest first. The newest is what we
/// advertise when a client asks for something we don't know.
pub const SUPPORTED_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Negotiate the protocol version per spec: echo the client's request when we
/// support it, otherwise answer with our latest supported version (the client
/// then decides whether to proceed or disconnect).
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

/// Schema builder that also carries a MCP `annotations` object (behaviour
/// hints: readOnlyHint / destructiveHint / idempotentHint). `None` omits the
/// field.
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

/// One tool's result: the human/agent-readable text plus, when the tool
/// declares an `outputSchema`, the machine-readable form echoed as
/// `structuredContent`.
///
/// Text is never optional. The MCP spec keeps `content` authoritative and
/// treats `structuredContent` as an addition for clients that want to skip
/// re-parsing a render, so a structured tool must emit BOTH — dropping the text
/// would break every client that only reads content blocks.
pub struct ToolOut {
    pub text: String,
    pub structured: Option<Value>,
}

impl ToolOut {
    /// Text-only result (no `outputSchema` on this tool).
    pub fn text(text: String) -> Self {
        Self {
            text,
            structured: None,
        }
    }

    /// Text plus the structured payload matching the tool's `outputSchema`.
    pub fn structured(text: String, structured: Value) -> Self {
        Self {
            text,
            structured: Some(structured),
        }
    }
}

impl From<String> for ToolOut {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

/// Attach an `outputSchema` to a tool entry built by `tool`/`tool_annotated`.
///
/// A declared schema is a CONTRACT: per spec the server must then return
/// `structuredContent` conforming to it, so only wrap tools whose dispatch
/// actually produces the matching payload.
pub fn with_output_schema(mut tool: Value, schema: Value) -> Value {
    tool["outputSchema"] = schema;
    tool
}

/// `outputSchema` for the tools that return a list of rows. MCP requires the
/// top level of an output schema to be an object, so the array rides in a
/// named field rather than being the root.
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

/// Serve until the reader closes. `tools` is the tools/list payload;
/// `call(name, args)` runs one tool and returns its text. Tool failures
/// become results with isError — never protocol errors. `instructions` is
/// the optional MCP server preamble echoed in the initialize result (how to
/// use the server); `None` omits the field.
pub fn serve<R: BufRead, W: Write>(
    reader: R,
    mut writer: W,
    tools: &[Value],
    instructions: Option<&str>,
    mut call: impl FnMut(&str, &Value) -> Result<ToolOut>,
) -> Result<()> {
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
                    "capabilities": {"tools": {}},
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
            "tools/list" => reply(&mut writer, id, Ok(json!({"tools": tools})))?,
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let result = match call(name, &args) {
                    Ok(out) => {
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
        let tools = vec![tool("echo", "echo text", json!({}), &[])];
        serve(
            input.as_bytes(),
            &mut out,
            &tools,
            instructions,
            |name, args| {
                if name == "echo" {
                    Ok(ToolOut::text(
                        args["text"].as_str().unwrap_or("").to_string(),
                    ))
                } else {
                    Err(anyhow!("unknown tool '{name}'"))
                }
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
}
