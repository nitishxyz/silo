//! The fake's JSON-RPC methods and tools.
use super::{Inner, Req};
use serde_json::{Value, json};

fn tools(grown: bool) -> Vec<Value> {
    let object = json!({"type":"object"});
    let mut tools = vec![
        json!({"name":"echo","description":"Echo the text back.","annotations":{"readOnlyHint":true},
            "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}),
        json!({"name":"grow","description":"Add a tool.","inputSchema":object}),
        json!({"name":"ask","description":"Send the client a request (`method`) and return its reply.","inputSchema":object}),
    ];
    if grown {
        tools.push(json!({"name":"extra","description":"Added later.","inputSchema":object}));
    }
    tools
}

impl Inner {
    /// The reply to a POSTed message; `None` for notifications and the client's replies.
    pub(crate) async fn post(&self, req: &Req) -> Option<Value> {
        let message: Value = serde_json::from_slice(&req.body).ok()?;
        let method = message["method"].as_str().map(String::from);
        let Some(method) = method else {
            let id = message["id"].as_str().map_or_else(|| message["id"].to_string(), String::from);
            self.state().responses.insert(id, message);
            return None;
        };
        self.state().methods.push(method.clone());
        let id = message.get("id")?.clone();
        let params = &message["params"];
        let result = match method.as_str() {
            "initialize" => json!({"protocolVersion":"2025-06-18","serverInfo":{"name":"fake","version":"1"},
                "capabilities":{"tools":{"listChanged":true},"resources":{},"prompts":{}}}),
            "ping" => json!({}),
            "tools/list" => json!({"tools":tools(self.state().grown)}),
            "tools/call" => self.call(params).await,
            "resources/list" => json!({"resources":[{"uri":"file:///notes.txt","name":"notes","mimeType":"text/plain"}]}),
            "resources/read" => json!({"contents":[{"uri":params["uri"],"mimeType":"text/plain","text":"hello notes"}]}),
            "prompts/list" => json!({"prompts":[{"name":"review","description":"Review code","arguments":[{"name":"path","required":true}]}]}),
            "prompts/get" => json!({"messages":[{"role":"user","content":{"type":"text",
                "text":format!("Review {}", params["arguments"]["path"].as_str().unwrap_or_default())}}]}),
            _ => return Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"no such method"}})),
        };
        Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
    }

    async fn call(&self, params: &Value) -> Value {
        let text = |t: String| json!({"content":[{"type":"text","text":t}]});
        match params["name"].as_str().unwrap_or_default() {
            "echo" => text(params["arguments"]["text"].as_str().unwrap_or_default().to_owned()),
            "grow" => {
                self.state().grown = true;
                self.push(json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}));
                text("grown".into())
            }
            "ask" => {
                let id = format!("server-{}", self.state().log.len() + 1);
                let method = params["arguments"]["method"].as_str().unwrap_or("ping");
                self.push(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params["arguments"]["params"]}));
                text(self.reply(&id).await.to_string())
            }
            _ => json!({"content":[{"type":"text","text":"unknown tool"}],"isError":true}),
        }
    }
}
