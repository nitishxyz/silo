//! Tiny line-delimited stdio MCP server for otto-mcp's tests.
//!
//! Tools: `echo` (read-only, deliberately awkward schema), `slow` (waits for
//! `notifications/cancelled`), `crash` (exits), `grow` (adds `extra` and
//! sends `tools/list_changed`). `OTTO_MCP_FIXTURE_TOOLS=n` adds `tool_0..n`.
use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn send(out: &mut impl Write, value: &Value) {
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn main() {
    let extra_count: usize = std::env::var("OTTO_MCP_FIXTURE_TOOLS").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
    let mut grown = false;
    let mut slow: Option<Value> = None;
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
        let id = request.get("id").cloned();
        let method = request["method"].as_str().unwrap_or_default();
        if method == "notifications/cancelled" {
            if let Some(slow_id) = slow.take() {
                send(&mut out, &json!({"jsonrpc":"2.0","id":slow_id,"result":{"content":[{"type":"text","text":"cancelled"}],"isError":true}}));
            }
            continue;
        }
        let Some(id) = id else { continue };
        let result = match method {
            "initialize" => {
                json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"fixture","version":"1"}})
            }
            "ping" => json!({}),
            "tools/list" => {
                let mut tools = vec![
                    json!({"name":"echo","description":"Echo the text back.","annotations":{"readOnlyHint":true},
                        "inputSchema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object",
                            "anyOf":[{"properties":{"text":{"type":"string"}}}],"required":["text","ghost"],
                            "properties":{"tags":{"type":"array"},"ref":{"$ref":"#/$defs/missing"}}}}),
                    json!({"name":"slow","description":"Wait until cancelled.","inputSchema":{"type":"object"}}),
                    json!({"name":"crash","description":"Exit the server.","inputSchema":{"type":"object"}}),
                    json!({"name":"grow","description":"Add a tool.","inputSchema":{"type":"object"}}),
                ];
                if grown {
                    tools.push(json!({"name":"extra","description":"Added later.","inputSchema":{"type":"object"}}));
                }
                tools.extend(
                    (0..extra_count)
                        .map(|i| json!({"name":format!("tool_{i}"),"description":format!("Generated tool {i}."),"inputSchema":{"type":"object"}})),
                );
                json!({"tools":tools})
            }
            "tools/call" => match request["params"]["name"].as_str().unwrap_or_default() {
                "echo" => json!({"content":[{"type":"text","text":request["params"]["arguments"]["text"].as_str().unwrap_or_default()}]}),
                "slow" => {
                    slow = Some(id);
                    continue;
                }
                "crash" => std::process::exit(3),
                "grow" => {
                    grown = true;
                    send(&mut out, &json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}));
                    json!({"content":[{"type":"text","text":"grown"}]})
                }
                name => json!({"content":[{"type":"text","text":format!("called {name}")}]}),
            },
            _ => {
                send(&mut out, &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}}));
                continue;
            }
        };
        send(&mut out, &json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
}
