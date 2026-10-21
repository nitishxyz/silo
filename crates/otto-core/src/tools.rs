use crate::session::Context;
use otto_tools::{Effects, Loading, OutputPart, Tool, ToolCx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

pub(crate) struct SessionTool {
    spec: ToolSpec,
    cx: Arc<Context>,
}
pub(crate) async fn loaded(cx: &Context) -> crate::Result<BTreeSet<String>> {
    let id = cx.row.id.clone();
    cx.runtime
        .db(move |s| {
            let row = s.session(&id)?.ok_or(crate::Error::Closed)?;
            Ok(serde_json::from_value(row.meta["loaded_tools"].clone()).unwrap_or_default())
        })
        .await
}

fn deferred(cx: &Context) -> Vec<ToolSpec> {
    let family = serde_json::to_value(cx.model.info().caps.tool_family).unwrap();
    let mut all = otto_tools::registry::tools_for_family(family.as_str().unwrap_or("generic"));
    all.extend(cx.runtime.extra_tools());
    all.into_iter()
        .filter_map(|tool| {
            let spec = tool.spec();
            (spec.loading == Loading::Deferred
                && !cx.row.meta["tools"].as_array().is_some_and(|grant| !grant.iter().any(|v| v.as_str() == Some(&spec.name))))
            .then(|| spec.clone())
        })
        .collect()
}

pub(crate) fn granted(cx: &Context, spec: &ToolSpec) -> bool {
    // Sessions saved before the loader existed still need access to it.
    !cx.row.meta["tools"].as_array().is_some_and(|grant| !grant.iter().any(|v| v.as_str() == Some(&spec.name)))
        || (spec.name == "load_tools" && spec.source == otto_tools::Source::Builtin && !deferred(cx).is_empty())
}

pub(crate) async fn infos(cx: Arc<Context>) -> crate::Result<Vec<otto_proto::ToolInfo>> {
    let family = serde_json::to_value(cx.model.info().caps.tool_family)?.as_str().unwrap_or("generic").to_owned();
    let mut all = otto_tools::registry::tools_for_family(&family);
    all.extend(tools(cx.clone(), &loaded(&cx).await?));
    all.extend(cx.runtime.extra_tools());
    Ok(all
        .into_iter()
        .filter_map(|tool| {
            let spec = tool.spec();
            if !granted(&cx, spec) {
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
pub(crate) fn tools(cx: Arc<Context>, loaded: &BTreeSet<String>) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = [("skill","Load a skill by name",json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]})),
     ("subagent","Delegate, list, status, message, or stop child agents",json!({"type":"object","properties":{"action":{"type":"string","enum":["delegate","list","status","message","stop"]},"task":{"type":"string"},"session_id":{"type":"string"},"agent":{"type":"string"},"delivery":{"type":"string","enum":["queue","interrupt"]}},"required":["action"]}))]
    .into_iter().map(|(name,description,input_schema)|Arc::new(SessionTool{spec:ToolSpec{name:name.into(),description:description.into(),input_schema,output_schema:None,effects:if name=="subagent"{Effects::PROCESS}else{Effects::READ},sensitive:vec![],loading:Default::default(),mode:Default::default(),presentations:vec![],source:Default::default(),exclusive:name=="subagent"},cx:cx.clone()}) as Arc<dyn Tool>).collect();
    let pending: Vec<_> = deferred(&cx).into_iter().filter(|s| !loaded.contains(&s.name)).collect();
    if !pending.is_empty() {
        let mut description = "Load deferred tool schemas for the next model call. Call with {\"names\": [\"tool_name\"]}, or {\"query\": \"browser\"} to load all name/description matches (case-insensitive substring). Available tools:\n".to_owned();
        for spec in pending {
            let sentence = spec.description.split_inclusive(['.', '!', '?']).next().unwrap_or("");
            description.push_str(&format!("{}: {}\n", spec.name, sentence.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
        tools.push(Arc::new(SessionTool {
            spec: ToolSpec {
                name: "load_tools".into(), description,
                input_schema: json!({"type":"object","properties":{"names":{"type":"array","items":{"type":"string"}},"query":{"type":"string"}},"anyOf":[{"required":["names"]},{"required":["query"]}]}),
                output_schema: None, effects: Effects::READ, sensitive: vec![], loading: Loading::Eager,
                mode: Default::default(), presentations: vec![], source: Default::default(), exclusive: false,
            }, cx,
        }));
    }
    tools
}
#[async_trait::async_trait]
impl Tool for SessionTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, _cx: ToolCx) -> std::result::Result<ToolOutput, ToolError> {
        match self.spec.name.as_str() {
            "load_tools" => {
                let specs = deferred(&self.cx);
                let names: Vec<String> = input
                    .get("names")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|_| ToolError::invalid("names must be an array of strings"))?
                    .unwrap_or_default();
                let query = input
                    .get("query")
                    .map(|q| q.as_str().map(str::to_lowercase).ok_or_else(|| ToolError::invalid("query must be a string")))
                    .transpose()?;
                if input.get("names").is_none() && query.is_none() {
                    return Err(ToolError::invalid("names or query required"));
                }
                let unknown: Vec<_> = names.iter().filter(|name| !specs.iter().any(|s| &s.name == *name)).cloned().collect();
                let selected: BTreeSet<_> = specs
                    .into_iter()
                    .filter(|s| {
                        names.contains(&s.name)
                            || query.as_ref().is_some_and(|q| s.name.to_lowercase().contains(q) || s.description.to_lowercase().contains(q))
                    })
                    .map(|s| s.name)
                    .collect();
                let id = self.cx.row.id.clone();
                let added = selected.clone();
                self.cx
                    .runtime
                    .db(move |store| {
                        let mut row = store.session(&id)?.ok_or(crate::Error::Closed)?;
                        let mut loaded: BTreeSet<String> = serde_json::from_value(row.meta["loaded_tools"].clone()).unwrap_or_default();
                        loaded.extend(added);
                        row.meta["loaded_tools"] = json!(loaded);
                        store.update_session(&id, otto_store::SessionUpdate { meta: Some(row.meta), ..Default::default() })?;
                        Ok(())
                    })
                    .await
                    .map_err(|e| ToolError::failed(e.to_string()))?;
                let mut output = ToolOutput::text(json!({"loaded": selected}).to_string());
                if !unknown.is_empty() {
                    output.content.push(OutputPart::Text { text: format!("Unknown or ungranted deferred tools: {}", unknown.join(", ")) });
                }
                Ok(output)
            }
            "skill" => {
                let name = input["name"].as_str().ok_or_else(|| ToolError::invalid("name required"))?;
                let skill = crate::skills::discover(&self.cx.root, &self.cx.runtime.0.config.user_config_dir)
                    .into_iter()
                    .find(|s| s.name == name)
                    .ok_or_else(|| ToolError::invalid("unknown skill"))?;
                Ok(ToolOutput::text(tokio::fs::read_to_string(skill.path).await.map_err(|e| ToolError::failed(e.to_string()))?))
            }
            _ => crate::subagents::call(&self.cx, input).await.map(ToolOutput::text).map_err(|e| ToolError::failed(e.to_string())),
        }
    }
}
