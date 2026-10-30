//! A server's resources and prompts as tools: `mcp__<server>__list_resources`,
//! `mcp__<server>__read_resource` and `mcp__<server>__get_prompt` (its description lists
//! the prompts). A server tool of the same name wins.
use crate::{
    client::{Client, lock},
    server::{Server, content_part, resource_part, spec},
};
use async_trait::async_trait;
use otto_tools::{Effects, OutputPart, Tool, ToolCx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone, Copy)]
enum Kind {
    ListResources,
    ReadResource,
    GetPrompt,
}

struct Extra {
    kind: Kind,
    spec: ToolSpec,
    server: Arc<Server>,
}

fn prompt_line(prompt: &Value) -> String {
    let arguments: Vec<String> = prompt["arguments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| Some(format!("{}{}", a["name"].as_str()?, if a["required"] == true { "*" } else { "" })))
        .collect();
    let description: String = prompt["description"].as_str().unwrap_or_default().chars().take(200).collect();
    format!("- {}({}): {description}", prompt["name"].as_str().unwrap_or_default(), arguments.join(", "))
}

pub(crate) async fn tools(server: &Arc<Server>, client: &Arc<Client>, deferred: bool, taken: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
    let capabilities = lock(&client.capabilities).clone();
    let effects = Effects { read: true, network: server.config.is_http(), ..Effects::NONE };
    let mut extras = Vec::new();
    let mut add = |kind, name: &str, description: &str, schema: Value| {
        extras.push(Extra { kind, spec: spec(server, name, description, schema, effects, deferred), server: server.clone() });
    };
    if capabilities.get("resources").is_some_and(|c| !c.is_null()) {
        add(
            Kind::ListResources,
            "list_resources",
            "List the resources (files, documents, records) this server offers and its URI templates; read one with read_resource.",
            json!({"type":"object","properties":{}}),
        );
        add(
            Kind::ReadResource,
            "read_resource",
            "Read one of this server's resources by URI.",
            json!({"type":"object","properties":{"uri":{"type":"string","description":"The resource URI."}},"required":["uri"]}),
        );
    }
    if capabilities.get("prompts").is_some_and(|c| !c.is_null()) {
        let prompts = server.list(client, "prompts/list", "prompts").await.unwrap_or_default();
        let names: Vec<&str> = prompts.iter().filter_map(|p| p["name"].as_str()).collect();
        if !names.is_empty() {
            let lines: Vec<String> = prompts.iter().take(50).map(prompt_line).collect();
            let description = format!(
                "Get one of this server's prompt templates filled in with arguments (* = required): instructions to follow.\n{}",
                lines.join("\n")
            );
            add(
                Kind::GetPrompt,
                "get_prompt",
                &description,
                json!({"type":"object","properties":{"name":{"type":"string","enum":names},
                    "arguments":{"type":"object","additionalProperties":{"type":"string"}}},"required":["name"]}),
            );
        }
    }
    extras.into_iter().filter(|e| !taken.iter().any(|t| t.spec().name == e.spec.name)).map(|e| Arc::new(e) as Arc<dyn Tool>).collect()
}

impl Extra {
    async fn list_resources(&self, cx: &ToolCx) -> Result<Vec<OutputPart>, ToolError> {
        let mut lines = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let params = cursor.as_ref().map_or_else(|| json!({}), |c| json!({"cursor":c}));
            let result = self.server.request("resources/list", params, &cx.cancel).await?;
            for r in result["resources"].as_array().into_iter().flatten() {
                let (uri, name) = (r["uri"].as_str().unwrap_or_default(), r["name"].as_str().unwrap_or_default());
                let mime = r["mimeType"].as_str().map(|m| format!(" ({m})")).unwrap_or_default();
                let description = r["description"].as_str().map(|d| format!(": {d}")).unwrap_or_default();
                lines.push(format!("{uri} {name}{mime}{description}"));
            }
            cursor = result["nextCursor"].as_str().map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        // Templates are optional; a server without them answers with an error.
        if let Ok(result) = self.server.request("resources/templates/list", json!({}), &cx.cancel).await {
            for t in result["resourceTemplates"].as_array().into_iter().flatten() {
                let description = t["description"].as_str().map(|d| format!(": {d}")).unwrap_or_default();
                lines.push(format!(
                    "template {} {}{description}",
                    t["uriTemplate"].as_str().unwrap_or_default(),
                    t["name"].as_str().unwrap_or_default()
                ));
            }
        }
        let text = if lines.is_empty() { "No resources.".to_owned() } else { lines.join("\n") };
        Ok(vec![OutputPart::Text { text }])
    }

    async fn get_prompt(&self, input: &Value, cx: &ToolCx) -> Result<Vec<OutputPart>, ToolError> {
        let name = input["name"].as_str().ok_or_else(|| ToolError::invalid("name is required"))?;
        let arguments = if input["arguments"].is_object() { input["arguments"].clone() } else { json!({}) };
        let result = self.server.request("prompts/get", json!({"name":name,"arguments":arguments}), &cx.cancel).await?;
        let mut parts = Vec::new();
        if let Some(description) = result["description"].as_str() {
            parts.push(OutputPart::Text { text: description.to_owned() });
        }
        for message in result["messages"].as_array().into_iter().flatten() {
            parts.push(OutputPart::Text { text: format!("[{}]", message["role"].as_str().unwrap_or("user")) });
            parts.push(content_part(&message["content"]));
        }
        Ok(parts)
    }
}

#[async_trait]
impl Tool for Extra {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let content = match self.kind {
            Kind::ListResources => self.list_resources(&cx).await?,
            Kind::ReadResource => {
                let uri = input["uri"].as_str().ok_or_else(|| ToolError::invalid("uri is required"))?;
                let result = self.server.request("resources/read", json!({"uri":uri}), &cx.cancel).await?;
                result["contents"].as_array().into_iter().flatten().map(resource_part).collect()
            }
            Kind::GetPrompt => self.get_prompt(&input, &cx).await?,
        };
        Ok(ToolOutput { content, is_error: false, metadata: Value::Null, truncated: None })
    }
}
