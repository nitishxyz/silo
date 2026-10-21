//! Line-delimited MCP over stdio.

use serde_json::{Value, json};
use silo_tools::{Part, Tools};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

/// Serve until stdin closes.
///
/// # Errors
/// Stdio failures.
pub async fn serve(tools: Tools) -> anyhow::Result<()> {
    let tools = Arc::new(tools);
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let (tx, mut rx) = mpsc::channel::<Value>(64);
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(value) = rx.recv().await {
            stdout.write_all(format!("{value}\n").as_bytes()).await?;
            stdout.flush().await?;
        }
        Ok::<_, std::io::Error>(())
    });
    let mut tasks = tokio::task::JoinSet::new();
    let mut pending = HashMap::<String, CancellationToken>::new();
    loop {
        tokio::select! {
            done = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok(id)) = done { pending.remove(&id); }
            }
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                let request: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(error) => { tx.send(error_response(Value::Null, -32700, &error.to_string())).await?; continue; }
                };
                let id = request.get("id").cloned();
                let method = request["method"].as_str();
                if request["jsonrpc"] != "2.0" || method.is_none() {
                    tx.send(error_response(id.unwrap_or(Value::Null), -32600, "invalid request")).await?;
                    continue;
                }
                if method == Some("notifications/cancelled") {
                    if let Some(cancel) = pending.get(&request["params"]["requestId"].to_string()) { cancel.cancel(); }
                    continue;
                }
                let Some(id) = id else { continue };
                let result = match method.unwrap() {
                    "initialize" => {
                        let requested = request["params"]["protocolVersion"].as_str().unwrap_or_default();
                        let version = if ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"].contains(&requested) { requested } else { "2025-06-18" };
                        json!({"protocolVersion": version, "capabilities":{"tools":{}}, "serverInfo":{"name":"silo", "version":env!("CARGO_PKG_VERSION")}})
                    }
                    "ping" => json!({}),
                    "tools/list" => json!({"tools": tools.descriptors().into_iter().map(|d| json!({"name":d.name,"description":d.description,"inputSchema":d.input_schema,"annotations":{"readOnlyHint":d.effects.read && !d.effects.write && !d.effects.process}})).collect::<Vec<_>>()}),
                    "tools/call" => {
                        let Some(name) = request["params"]["name"].as_str().map(str::to_owned) else { tx.send(error_response(id, -32602, "name is required")).await?; continue; };
                        if !tools.descriptors().iter().any(|d| d.name == name) { tx.send(error_response(id, -32602, "unknown tool")).await?; continue; }
                        let input = request["params"].get("arguments").cloned().unwrap_or_else(|| json!({}));
                        let cancel = CancellationToken::new();
                        pending.insert(id.to_string(), cancel.clone());
                        let (tools, tx) = (tools.clone(), tx.clone());
                        tasks.spawn(async move {
                            let result = match tools.call(&name, input, cancel).await {
                                Ok(result) => json!({"content":result.content.into_iter().map(|part| match part {
                                    Part::Text(text) => json!({"type":"text","text":text}),
                                    Part::Json(value) => json!({"type":"text","text":value.to_string()}),
                                    Part::Image { mime, data_base64 } => json!({"type":"image","mimeType":mime,"data":data_base64}),
                                }).collect::<Vec<_>>(),"isError":result.is_error}),
                                Err(error) => json!({"content":[{"type":"text","text":format!("{error:#}")}],"isError":true}),
                            };
                            let _ = tx.send(json!({"jsonrpc":"2.0","id":id,"result":result})).await;
                            id.to_string()
                        });
                        continue;
                    }
                    _ => { tx.send(error_response(id, -32601, "method not found")).await?; continue; }
                };
                tx.send(json!({"jsonrpc":"2.0","id":id,"result":result})).await?;
            }
        }
    }
    for cancel in pending.values() {
        cancel.cancel();
    }
    while tasks.join_next().await.is_some() {}
    drop(tx);
    writer.await??;
    Ok(())
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
