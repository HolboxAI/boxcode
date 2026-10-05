//! A stand-in MCP server for the test suite.
//!
//! Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout, which is exactly what
//! `src/mcp.rs` expects. It exists so the client's connect -> initialize ->
//! tools/list path can be exercised with no Docker, no network and no
//! credentials.
//!
//! Nothing in `src/mcp.rs` knows this binary exists: it is reached only through
//! the `command`/`args` of a server config, so replacing it with a real server
//! is a change to that config, not to the client. See `docs/MCP-transport.md`.
//!
//! Deliberately test-only: this is not a useful MCP server.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

fn main() {
    // stdout carries protocol only; the client skips any line that is not JSON,
    // but diagnostics still belong on stderr so a stray print can never be
    // mistaken for a reply.
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = msg.get("id").cloned();
        let method = msg
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();

        // A notification has no id and gets no reply -- e.g.
        // notifications/initialized.
        if id.is_none() {
            continue;
        }
        if std::env::var_os("MCP_FAKE_SILENT").is_some() {
            continue;
        }

        let reply = match method.as_str() {
            "initialize" => {
                if std::env::var_os("MCP_FAKE_FAIL_INIT").is_some() {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32603, "message": "fake initialize failure" }
                    })
                } else {
                    // Echo the client's requested version rather than guessing:
                    // the client rejects versions it does not support.
                    let version = msg
                        .get("params")
                        .and_then(|p| p.get("protocolVersion"))
                        .cloned()
                        .unwrap_or_else(|| json!("2026-07-28"));
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": version,
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mcp-fake-server", "version": "0.0.0" }
                        }
                    })
                }
            }
            "tools/list" => {
                let names =
                    std::env::var("MCP_FAKE_TOOLS").unwrap_or_else(|_| "echo,add".to_string());
                let tools: Vec<Value> = if std::env::var_os("MCP_FAKE_NO_TOOLS").is_some() {
                    Vec::new()
                } else {
                    names
                        .split(',')
                        .map(str::trim)
                        .filter(|n| !n.is_empty())
                        .map(|n| {
                            json!({
                                "name": n,
                                "description": match std::env::var("MCP_FAKE_ECHO_ENV") {
                                    Ok(k) => format!(
                                        "fake tool {n} env {k}={}",
                                        std::env::var(&k).unwrap_or_default()
                                    ),
                                    Err(_) => format!("fake tool {n}"),
                                },
                                "inputSchema": {
                                    "type": "object",
                                    "properties": { "text": { "type": "string" } },
                                    "required": ["text"]
                                }
                            })
                        })
                        .collect()
                };
                json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })
            }
            "tools/call" => {
                let name = msg
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = msg
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": format!("{name}:{args}") }],
                        "isError": false
                    }
                })
            }
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            other => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            }),
        };

        let mut encoded = serde_json::to_vec(&reply).expect("encode reply");
        encoded.push(b'\n');
        if stdout.write_all(&encoded).and_then(|_| stdout.flush()).is_err() {
            break;
        }
        let _ = writeln!(io::stderr(), "mcp-fake-server: handled {method}");
    }
}
