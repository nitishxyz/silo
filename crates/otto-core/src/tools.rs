use crate::session::Context;
use otto_tools::{Effects, Tool, ToolCx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) struct SessionTool {
    spec: ToolSpec,
    cx: Arc<Context>,
}
pub(crate) fn infos(cx: Arc<Context>) -> crate::Result<Vec<otto_proto::ToolInfo>> {
    let family = serde_json::to_value(cx.model.info().caps.tool_family)?.as_str().unwrap_or("generic").to_owned();
    let mut all = otto_tools::registry::tools_for_family(&family);
    all.extend(tools(cx.clone()));
    Ok(all
        .into_iter()
        .filter_map(|tool| {
            let spec = tool.spec();
            if cx.row.meta["tools"].as_array().is_some_and(|grant| !grant.iter().any(|v| v.as_str() == Some(&spec.name))) {
                return None;
            }
            let presentation = spec.presentations.iter().find(|p| p.family == family);
            let effects = [
                ("read", spec.effects.read),
                ("write", spec.effects.write),
                ("process", spec.effects.process),
                ("network", spec.effects.network),
                ("secrets", spec.effects.secrets),
                ("external", spec.effects.external),
            ]
            .into_iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| name.into())
            .collect();
            let source = match &spec.source {
                otto_tools::Source::Builtin => "builtin".into(),
                otto_tools::Source::Silo => "silo".into(),
                otto_tools::Source::Mcp { server } => format!("mcp:{server}"),
                otto_tools::Source::Ext { plugin } => format!("ext:{plugin}"),
            };
            Some(otto_proto::ToolInfo {
                name: presentation.map_or_else(|| spec.name.clone(), |p| p.name.clone()),
                description: presentation.and_then(|p| p.description.clone()).unwrap_or_else(|| spec.description.clone()),
                source,
                effects,
            })
        })
        .collect())
}
pub(crate) fn tools(cx: Arc<Context>) -> Vec<Arc<dyn Tool>> {
    [("skill","Load a skill by name",json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]})),
     ("todos","Replace or inspect this session's todo list",json!({"type":"object","properties":{"items":{"type":"array","items":{"type":"object"}}}})),
     ("subagent","Delegate, list, status, message, or stop child agents",json!({"type":"object","properties":{"action":{"type":"string","enum":["delegate","list","status","message","stop"]},"task":{"type":"string"},"session_id":{"type":"string"},"agent":{"type":"string"},"delivery":{"type":"string","enum":["queue","interrupt"]}},"required":["action"]}))]
    .into_iter().map(|(name,description,input_schema)|Arc::new(SessionTool{spec:ToolSpec{name:name.into(),description:description.into(),input_schema,output_schema:None,effects:if name=="subagent"{Effects::PROCESS}else{Effects::READ},sensitive:vec![],loading:Default::default(),mode:Default::default(),presentations:vec![],source:Default::default(),exclusive:name=="subagent"},cx:cx.clone()}) as Arc<dyn Tool>).collect()
}
#[async_trait::async_trait]
impl Tool for SessionTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, _cx: ToolCx) -> std::result::Result<ToolOutput, ToolError> {
        match self.spec.name.as_str() {
            "skill" => {
                let name = input["name"].as_str().ok_or_else(|| ToolError::invalid("name required"))?;
                let skill = crate::skills::discover(&self.cx.root, &self.cx.runtime.0.config.user_config_dir)
                    .into_iter()
                    .find(|s| s.name == name)
                    .ok_or_else(|| ToolError::invalid("unknown skill"))?;
                Ok(ToolOutput::text(tokio::fs::read_to_string(skill.path).await.map_err(|e| ToolError::failed(e.to_string()))?))
            }
            "todos" => {
                if input.get("items").is_some_and(|items| !items.is_array()) {
                    return Err(ToolError::invalid("items must be an array"));
                }
                let todos = self
                    .cx
                    .state(move |s| {
                        if let Some(items) = input.get("items") {
                            s.todos = items.clone();
                        }
                        s.todos.to_string()
                    })
                    .await
                    .map_err(|e| ToolError::failed(e.to_string()))?;
                Ok(ToolOutput::text(todos))
            }
            _ => crate::subagents::call(&self.cx, input).await.map(ToolOutput::text).map_err(|e| ToolError::failed(e.to_string())),
        }
    }
}
