//! A minimal stdio MCP server used to exercise the bridge (`src/bridge.rs`).
//!
//! It speaks newline-delimited JSON-RPC on stdin/stdout and exposes a single
//! `mock_echo` tool that echoes its `message`, returning both a text block and a
//! structured `{ echoed }` result. Run indirectly via codexify's `mcpServers`
//! config; not meant to be used on its own.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();

        // Notifications (no id) get no response.
        if id.is_none() {
            continue;
        }

        let result: Value = match method {
            "initialize" => {
                let version = msg
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("2025-06-18")
                    .to_string();
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "mock-mcp", "version": "0.1.0" }
                })
            }
            "tools/list" => json!({
                "tools": [{
                    "name": "mock_echo",
                    "description": "Echoes the given message back.",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "message": { "type": "string", "description": "Text to echo." } },
                        "required": ["message"]
                    },
                    "outputSchema": {
                        "type": "object",
                        "properties": { "echoed": { "type": "string" } },
                        "required": ["echoed"]
                    }
                }]
            }),
            "tools/call" => {
                let name = msg
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let message = msg
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .and_then(|a| a.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");
                if name == "mock_echo" {
                    json!({
                        "content": [{ "type": "text", "text": format!("echo: {message}") }],
                        "structuredContent": { "echoed": message },
                        "isError": false
                    })
                } else {
                    send_error(&mut out, id.clone(), -32601, "unknown tool");
                    continue;
                }
            }
            _ => {
                send_error(&mut out, id.clone(), -32601, "method not found");
                continue;
            }
        };

        let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        writeln!(out, "{response}").ok();
        out.flush().ok();
    }
}

fn send_error(out: &mut impl Write, id: Option<Value>, code: i64, message: &str) {
    let response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    });
    writeln!(out, "{response}").ok();
    out.flush().ok();
}
