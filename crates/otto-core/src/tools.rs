use crate::session::{Context, Live};
use otto_tools::{Effects, Loading, OutputPart, Tool, ToolCx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Deferred tools listed by name in the `load_tools` description; the rest are found by query.
const INDEX_LIMIT: usize = 60;
/// Most tools one `load_tools` query loads.
const QUERY_LIMIT: usize = 12;

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

/// `name` or a `prefix*` pattern (`*` alone matches everything).
pub(crate) fn pattern(pattern: &str, name: &str) -> bool {
    pattern.strip_suffix('*').map_or(pattern == name, |prefix| name.starts_with(prefix))
}

/// Built-ins for the model's family, registered extras, and project sources (MCP).
pub(crate) async fn available(cx: &Context) -> Vec<Arc<dyn Tool>> {
    let mut all = cx.runtime.builtins(&cx.live().family());
    all.extend(cx.runtime.extra_tools());
    all.extend(cx.runtime.source_tools(&cx.root).await);
    all
}

/// `copy_attachment` comes with `write` and `web_search` with `web_fetch`: a grant fixed before
/// they existed still reaches them.
fn implied(name: &str) -> Option<&'static str> {
    match name {
        "copy_attachment" => Some("write"),
        "web_search" => Some("web_fetch"),
        _ => None,
    }
}

fn allowed(live: &Live, name: &str) -> bool {
    let names = [Some(name), implied(name)];
    let list =
        |key: &str| live.row.meta[key].as_array().map(|a| a.iter().filter_map(Value::as_str).any(|p| names.iter().flatten().any(|n| pattern(p, n))));
    list("tools").unwrap_or(true) && !list("denied_tools").unwrap_or(false)
}

fn deferred(cx: &Context, all: &[Arc<dyn Tool>]) -> Vec<ToolSpec> {
    let live = cx.live();
    all.iter().map(|t| t.spec()).filter(|s| s.loading == Loading::Deferred && allowed(&live, &s.name)).cloned().collect()
}

/// `load_tools` only exists while it has something to load, and it only
/// loads granted tools, so it needs no grant of its own (older sessions lack one).
pub(crate) fn granted(live: &Live, spec: &ToolSpec) -> bool {
    allowed(live, &spec.name) || (spec.name == "load_tools" && spec.source == otto_tools::Source::Builtin)
}

pub(crate) async fn infos(cx: Arc<Context>) -> crate::Result<Vec<otto_proto::ToolInfo>> {
    let live = cx.live();
    let family = live.family();
    let mut all = available(&cx).await;
    let session = tools(cx.clone(), &all, &loaded(&cx).await?);
    all.extend(session);
    Ok(all
        .into_iter()
        .filter_map(|tool| {
            let spec = tool.spec();
            if !granted(&live, spec) {
                return None;
            }
            let presentation = spec.presentation(&family);
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

fn first_sentence(text: &str) -> String {
    text.split_inclusive(['.', '!', '?']).next().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Rank deferred tools for a query: every word must appear in the name or
/// description; name hits weigh more. Best matches first.
pub(crate) fn search(specs: &[ToolSpec], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    let words: Vec<&str> = query.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| !w.is_empty()).collect();
    let mut scored: Vec<(usize, &ToolSpec)> = specs
        .iter()
        .filter_map(|s| {
            let (name, description) = (s.name.to_lowercase(), s.description.to_lowercase());
            let mut score = if name.contains(query.trim()) { 10 } else { 0 };
            for word in &words {
                match (name.contains(word), description.contains(word)) {
                    (true, _) => score += 3,
                    (false, true) => score += 1,
                    (false, false) => return None,
                }
            }
            (score > 0 || (words.is_empty() && !query.trim().is_empty() && description.contains(query.trim()))).then_some((score, s))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().take(QUERY_LIMIT).map(|(_, s)| s.name.clone()).collect()
}

pub(crate) fn tools(cx: Arc<Context>, all: &[Arc<dyn Tool>], loaded: &BTreeSet<String>) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = [
        ("skill", "Load a skill's instructions by name", json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]})),
        (
            "subagent",
            "Run child agents in the background. delegate {task, agent?, session_id? to reuse a child} returns at once; the child's result arrives later as a system message. Also list, status, message {session_id, message, delivery: queue|steer|interrupt}, stop, retry.",
            json!({"type":"object","properties":{
                "action":{"type":"string","enum":["delegate","list","status","message","stop","retry"]},
                "task":{"type":"string"},"message":{"type":"string"},"session_id":{"type":"string"},"agent":{"type":"string"},
                "delivery":{"type":"string","enum":["queue","steer","interrupt"]}},"required":["action"]}),
        ),
    ]
    .into_iter()
    .map(|(name, description, input_schema)| {
        Arc::new(SessionTool {
            spec: ToolSpec {
                name: name.into(),
                description: description.into(),
                input_schema,
                output_schema: None,
                effects: if name == "subagent" { Effects::PROCESS } else { Effects::READ },
                sensitive: vec![],
                loading: Default::default(),
                mode: Default::default(),
                presentations: vec![],
                source: Default::default(),
                exclusive: name == "subagent",
            },
            cx: cx.clone(),
        }) as Arc<dyn Tool>
    })
    .collect();
    let pending: Vec<_> = deferred(&cx, all).into_iter().filter(|s| !loaded.contains(&s.name)).collect();
    if !pending.is_empty() {
        let mut description = "Load deferred tool schemas for the next model call. Call with {\"names\": [\"tool_name\"]} (a trailing * loads every tool with that prefix, e.g. \"mcp__github__*\"), or {\"query\": \"open browser tab\"} to load the best name/description matches. Available tools:\n".to_owned();
        for spec in pending.iter().take(INDEX_LIMIT) {
            description.push_str(&format!("{}: {}\n", spec.name, first_sentence(&spec.description)));
        }
        if pending.len() > INDEX_LIMIT {
            description.push_str(&format!("…and {} more; find them with a query.\n", pending.len() - INDEX_LIMIT));
        }
        tools.push(Arc::new(SessionTool {
            spec: ToolSpec {
                name: "load_tools".into(),
                description,
                input_schema: json!({"type":"object","properties":{"names":{"type":"array","items":{"type":"string"}},"query":{"type":"string"}}}),
                output_schema: None,
                effects: Effects::READ,
                sensitive: vec![],
                loading: Loading::Eager,
                mode: Default::default(),
                presentations: vec![],
                source: Default::default(),
                exclusive: false,
            },
            cx,
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
                let specs = deferred(&self.cx, &available(&self.cx).await);
                let names: Vec<String> = input
                    .get("names")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|_| ToolError::invalid("names must be an array of strings"))?
                    .unwrap_or_default();
                let query = input
                    .get("query")
                    .map(|q| q.as_str().map(str::to_owned).ok_or_else(|| ToolError::invalid("query must be a string")))
                    .transpose()?;
                if input.get("names").is_none() && query.is_none() {
                    return Err(ToolError::invalid("names or query required"));
                }
                let unknown: Vec<_> = names.iter().filter(|name| !specs.iter().any(|s| pattern(name, &s.name))).cloned().collect();
                let mut selected: BTreeSet<String> =
                    specs.iter().filter(|s| names.iter().any(|n| pattern(n, &s.name))).map(|s| s.name.clone()).collect();
                if let Some(query) = &query {
                    selected.extend(search(&specs, query));
                }
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
                let mut output = ToolOutput::text(
                    json!({"loaded": selected.iter().map(|name| json!({"name":name,"description":specs.iter().find(|s| &s.name == name).map(|s| first_sentence(&s.description))})).collect::<Vec<_>>()})
                        .to_string(),
                );
                if !unknown.is_empty() {
                    output.content.push(OutputPart::Text { text: format!("Unknown or ungranted deferred tools: {}", unknown.join(", ")) });
                }
                if selected.is_empty() && query.is_some() {
                    output.content.push(OutputPart::Text { text: "No deferred tool matched; try other words or names from the list.".into() });
                }
                Ok(output)
            }
            "skill" => {
                let name = input["name"].as_str().ok_or_else(|| ToolError::invalid("name required"))?;
                let skill = crate::skills::discover(&self.cx.root, &self.cx.runtime.0.config.user_config_dir)
                    .into_iter()
                    .find(|s| s.name == name)
                    .ok_or_else(|| ToolError::invalid("unknown skill"))?;
                Ok(ToolOutput::text(crate::skills::load(&skill).map_err(|e| ToolError::failed(e.to_string()))?))
            }
            _ => crate::subagents::call(&self.cx, input).await.map(ToolOutput::text).map_err(|e| ToolError::failed(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    fn spec(name: &str, description: &str) -> otto_tools::ToolSpec {
        serde_json::from_value(serde_json::json!({"name":name,"description":description,"input_schema":{"type":"object"},"loading":"deferred"}))
            .unwrap()
    }
    #[test]
    fn search_ranks_name_hits_and_requires_every_word() {
        let specs = [
            spec("browser_click", "Click an element in a page."),
            spec("page_info", "Browser page title."),
            spec("mcp__gh__issue", "Create an issue."),
        ];
        assert_eq!(super::search(&specs, "browser"), ["browser_click", "page_info"]);
        assert_eq!(super::search(&specs, "browser click"), ["browser_click"]);
        assert!(super::search(&specs, "unrelated").is_empty());
        assert!(super::pattern("mcp__gh__*", "mcp__gh__issue") && super::pattern("*", "x") && !super::pattern("read", "reader"));
    }
}
