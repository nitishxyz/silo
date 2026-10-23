//! Canonical requests to Cloud Code Assist `streamGenerateContent` envelopes
//! (Gemini `contents`, as the Antigravity CLI sends them).
use crate::{CallKind, Effort, ErrorClass, Item, LlmError, Media, MediaSource, Part, Reasoning, Request, ResultPart, Role, ToolChoice, ToolKind};
use rand::Rng;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub const PROVIDER: &str = "antigravity";
const CONTINUE: &str = "Continue the active task using the available instructions and context.";

fn uuid(mut b: [u8; 16], version: u8) -> String {
    b[6] = (b[6] & 0x0f) | (version << 4);
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}
fn stable_uuid(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    uuid(digest[..16].try_into().expect("16 bytes"), 5)
}
fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    uuid(bytes, 4)
}

/// The conversation, trajectory and (signed 64-bit) session ids of a request:
/// stable for one otto session, random without one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ids {
    pub conversation: String,
    pub trajectory: String,
    pub session: String,
}
impl Ids {
    #[must_use]
    pub fn new(session: Option<&str>) -> Self {
        match session.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Self {
                conversation: stable_uuid(&format!("antigravity:conv:session:{s}")),
                trajectory: stable_uuid(&format!("antigravity:traj:session:{s}")),
                session: s.parse::<i64>().map_or_else(
                    |_| {
                        let digest = Sha256::digest(format!("antigravity:session:{s}").as_bytes());
                        i64::from_le_bytes(digest[..8].try_into().expect("8 bytes")).to_string()
                    },
                    |n| n.to_string(),
                ),
            },
            None => Self { conversation: random_uuid(), trajectory: random_uuid(), session: rand::random::<i64>().to_string() },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Off,
    Low,
    Medium,
    High,
}
fn level(reasoning: Option<Reasoning>) -> Level {
    match reasoning {
        None | Some(Reasoning::Effort(Effort::None) | Reasoning::Budget(0)) => Level::Off,
        Some(Reasoning::Effort(Effort::Minimal | Effort::Low) | Reasoning::Budget(1..=2048)) => Level::Low,
        Some(Reasoning::Effort(Effort::Medium) | Reasoning::Budget(2049..=8192)) => Level::Medium,
        Some(_) => Level::High,
    }
}

/// `generationConfig.thinkingConfig` for `model` (the Antigravity CLI's budgets per family);
/// `None` when the model takes none.
#[must_use]
pub fn thinking(model: &str, reasoning: Option<Reasoning>) -> Option<Value> {
    let level = level(reasoning);
    let wire = |budget: i64| Some(json!({"includeThoughts": level != Level::Off, "thinkingBudget": if level == Level::Off { 0 } else { budget }}));
    let pick = |low: i64, medium: i64, high: i64| match level {
        Level::Off | Level::Low => low,
        Level::Medium => medium,
        Level::High => high,
    };
    if model.starts_with("claude-") {
        wire(1024)
    } else if model.starts_with("gpt-oss-") {
        wire(8192)
    } else if model.starts_with("gemini-3.5-flash") || model == "gemini-3-flash-agent" {
        wire(pick(1000, 4000, 10_000))
    } else if model.starts_with("gemini-3.1-pro") || model == "gemini-pro-agent" {
        wire(pick(1001, 1001, 10_001))
    } else if model.contains("argon") || model.starts_with("gemini-4") {
        // Argon rejects a zero budget and any under 1000.
        (level != Level::Off).then(|| json!({"includeThoughts": true, "thinkingBudget": pick(1000, 4000, -1)}))
    } else if model.starts_with("gemini-") {
        wire(pick(1000, 4000, -1))
    } else {
        None
    }
}

const MAP_KEYS: [&str; 4] = ["properties", "patternProperties", "dependentSchemas", "dependencies"];
const SCHEMA_KEYS: [&str; 16] = [
    "additionalItems",
    "additionalProperties",
    "contains",
    "contentSchema",
    "else",
    "if",
    "items",
    "not",
    "propertyNames",
    "then",
    "unevaluatedItems",
    "unevaluatedProperties",
    "allOf",
    "anyOf",
    "oneOf",
    "prefixItems",
];
const META_KEYS: [&str; 8] = ["$schema", "$id", "$anchor", "$dynamicAnchor", "$vocabulary", "$comment", "$defs", "definitions"];
/// The protobuf `Schema` fields of the Claude / GPT-OSS custom-tool bridge; anything else is a 400.
const LEGACY_KEYS: [&str; 6] = ["type", "description", "properties", "required", "items", "enum"];

struct Walk<'a> {
    root: &'a Value,
    nodes: usize,
}
impl Walk<'_> {
    /// `schema` with local `$ref`s inlined and metadata (`$schema`, `$defs`, …) dropped.
    fn clean(&mut self, schema: &Value, refs: &mut Vec<String>, depth: usize) -> Result<Value, String> {
        self.nodes += 1;
        if depth > 64 || self.nodes > 10_000 {
            return Err("schema expansion is too large".into());
        }
        match schema {
            Value::Array(items) => items.iter().map(|item| self.clean(item, refs, depth + 1)).collect::<Result<Vec<_>, _>>().map(Value::Array),
            Value::Object(object) => {
                if let Some(target) = object.get("$ref").and_then(Value::as_str) {
                    if refs.iter().any(|r| r == target) {
                        return Err(format!("circular reference {target}"));
                    }
                    let found = target
                        .strip_prefix('#')
                        .and_then(|pointer| self.root.pointer(pointer))
                        .ok_or_else(|| format!("unresolved reference {target}"))?;
                    refs.push(target.into());
                    let resolved = self.clean(found, refs, depth + 1);
                    refs.pop();
                    let mut resolved = resolved?;
                    let mut siblings = object.clone();
                    siblings.remove("$ref");
                    if let (Value::Object(base), Value::Object(extra)) = (&mut resolved, self.clean(&Value::Object(siblings), refs, depth + 1)?) {
                        base.extend(extra);
                    }
                    return Ok(resolved);
                }
                let mut out = Map::new();
                for (key, value) in object {
                    if META_KEYS.contains(&key.as_str()) {
                        continue;
                    }
                    let value = match value {
                        Value::Object(map) if MAP_KEYS.contains(&key.as_str()) => Value::Object(
                            map.iter()
                                .map(|(name, schema)| Ok((name.clone(), self.clean(schema, refs, depth + 1)?)))
                                .collect::<Result<Map<_, _>, String>>()?,
                        ),
                        _ if SCHEMA_KEYS.contains(&key.as_str()) => self.clean(value, refs, depth + 1)?,
                        _ => value.clone(),
                    };
                    out.insert(key.clone(), value);
                }
                Ok(Value::Object(out))
            }
            other => Ok(other.clone()),
        }
    }
}

/// A self-contained root object schema Gemini takes as `parametersJsonSchema`.
///
/// # Errors
/// Unresolvable, circular or oversized `$ref`s.
pub fn sanitize_schema(schema: &Value) -> Result<Value, String> {
    let cleaned = Walk { root: schema, nodes: 0 }.clean(schema, &mut Vec::new(), 0)?;
    Ok(match cleaned {
        Value::Object(mut object) => {
            if !object.contains_key("type") {
                object.insert("type".into(), json!("object"));
                object.entry("properties").or_insert_with(|| json!({}));
            }
            Value::Object(object)
        }
        _ => json!({"type":"object","properties":{}}),
    })
}

/// `schema` reduced to what the Claude / GPT-OSS bridge accepts (`type` unions keep their first non-null type).
#[must_use]
pub fn legacy_schema(schema: &Value) -> Value {
    match schema {
        Value::Array(items) => Value::Array(items.iter().map(legacy_schema).collect()),
        Value::Object(object) => {
            let mut out = Map::new();
            for (key, value) in object.iter().filter(|(key, _)| LEGACY_KEYS.contains(&key.as_str())) {
                match (key.as_str(), value) {
                    ("type", Value::String(_)) => {
                        out.insert(key.clone(), value.clone());
                    }
                    ("type", Value::Array(types)) => {
                        if let Some(first) = types.iter().find(|t| t.as_str().is_some_and(|t| t != "null")) {
                            out.insert(key.clone(), first.clone());
                        }
                    }
                    ("type", _) => {}
                    ("properties", Value::Object(props)) => {
                        out.insert(key.clone(), Value::Object(props.iter().map(|(name, schema)| (name.clone(), legacy_schema(schema))).collect()));
                    }
                    ("enum", Value::Array(values)) if !values.iter().all(Value::is_string) => {}
                    _ => {
                        out.insert(key.clone(), legacy_schema(value));
                    }
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// `tools` for the request: one `functionDeclarations` group. A tool whose schema cannot be
/// made self-contained is left out (with a warning) rather than failing the call.
fn tools(req: &Request, legacy: bool) -> Result<Option<Value>, LlmError> {
    let mut declarations = Vec::new();
    for tool in &req.tools {
        if !matches!(tool.kind, ToolKind::Function | ToolKind::Strict) {
            return Err(LlmError::new(ErrorClass::InvalidRequest, "Antigravity takes function tools only"));
        }
        let schema = match sanitize_schema(&tool.input_schema) {
            Ok(schema) => schema,
            Err(reason) => {
                tracing::warn!(tool = %tool.name, %reason, "skipping a tool Antigravity cannot take");
                continue;
            }
        };
        let mut declaration = json!({"name":tool.name,"description":tool.description});
        declaration[if legacy { "parameters" } else { "parametersJsonSchema" }] = if legacy { legacy_schema(&schema) } else { schema };
        declarations.push(declaration);
    }
    Ok((!declarations.is_empty()).then(|| json!([{"functionDeclarations": declarations}])))
}

/// A Gemini thought signature: standard base64.
fn valid_signature(s: &str) -> bool {
    let body = s.trim_end_matches('=');
    s.len().is_multiple_of(4)
        && s.len() - body.len() <= 2
        && !body.is_empty()
        && body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}
/// The signature of a call this model produced.
fn signature<'a>(opaque: Option<&'a crate::Opaque>, model: &str) -> Option<&'a str> {
    opaque
        .filter(|o| o.matches_provider(PROVIDER) && o.model == model)
        .and_then(|o| o.data["thoughtSignature"].as_str())
        .filter(|s| valid_signature(s))
}
/// Gemini 3 and later reject function calls replayed without their signature.
fn requires_signature(model: &str) -> bool {
    let digits: String = model.strip_prefix("gemini-").unwrap_or_default().chars().take_while(char::is_ascii_digit).collect();
    model.starts_with("gemini-") && !matches!(digits.parse::<u32>(), Ok(0..=2))
}
/// Ids Claude and GPT-OSS need on calls and results: `[A-Za-z0-9_-]`, at most 64.
fn sanitize_id(id: &str, name: &str) -> String {
    let cleaned: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(64).collect();
    if cleaned.is_empty() { format!("{name}_1") } else { cleaned }
}

fn media(media: &Media, gemini: bool) -> Result<Value, LlmError> {
    if !gemini && !media.mime.starts_with("image/") {
        return Ok(json!({"text": media.unsupported("Antigravity")}));
    }
    match &media.source {
        MediaSource::Base64 { data } => Ok(json!({"inlineData":{"mimeType":media.mime,"data":data}})),
        _ => Err(LlmError::new(ErrorClass::InvalidRequest, "Antigravity takes inline (base64) media only")),
    }
}

/// Consecutive turns of one role merge into one.
#[derive(Default)]
struct Turns(Vec<Value>);
impl Turns {
    fn push(&mut self, role: &str, parts: Vec<Value>) {
        if parts.is_empty() {
            return;
        }
        match self.0.last_mut().filter(|turn| turn["role"] == role) {
            Some(turn) => turn["parts"].as_array_mut().expect("parts").extend(parts),
            None => self.0.push(json!({"role":role,"parts":parts})),
        }
    }
    fn has_call(turn: &Value) -> bool {
        turn["parts"].as_array().is_some_and(|parts| parts.iter().any(|part| part.get("functionCall").is_some()))
    }
}

/// Whether `item`'s first function call carries a signature this model made.
fn signed(item: &Item, model: &str) -> bool {
    item.parts
        .iter()
        .find_map(|part| match part {
            Part::ToolCall { kind, opaque, .. } if *kind != CallKind::Hosted => Some(signature(opaque.as_ref(), model).is_some()),
            _ => None,
        })
        .unwrap_or(false)
}

/// `contents` for `req.items`.
///
/// # Errors
/// Media this wire cannot take, tool results without their call, and a transcript ending in an unanswered call.
pub fn contents(req: &Request) -> Result<Vec<Value>, LlmError> {
    let model = req.model.as_str();
    let gemini = model.starts_with("gemini-");
    let ids = !gemini && (model.starts_with("claude-") || model.starts_with("gpt-oss-"));
    let needs_signature = requires_signature(model);
    let mut turns = Turns::default();
    let mut names: HashMap<&str, &str> = HashMap::new();
    // Calls dropped for want of a signature: their results become plain observations.
    let mut dropped: HashMap<&str, String> = HashMap::new();
    for item in &req.items {
        let role = if item.role == Role::Assistant { "model" } else { "user" };
        let item_signed = signed(item, model);
        for part in &item.parts {
            match part {
                Part::Text { text, .. } if !text.trim().is_empty() => turns.push(role, vec![json!({"text":text})]),
                Part::Text { .. } | Part::Compaction { summary: None, .. } => {}
                Part::System { text } => turns.push("user", vec![json!({"text":format!("<system-reminder>\n{text}\n</system-reminder>")})]),
                Part::ToolsChanged { added, removed } => {
                    turns.push("user", vec![json!({"text":format!("Tools added: {}; removed: {}", added.join(", "), removed.join(", "))})]);
                }
                Part::Image(m) | Part::Document(m) | Part::Audio(m) => turns.push(role, vec![media(m, gemini)?]),
                Part::Compaction { summary: Some(text), .. } => turns.push("user", vec![json!({"text":text})]),
                Part::HostedResult { tool, payload, .. } => turns.push("user", vec![json!({"text":format!("{tool} result: {payload}")})]),
                Part::Reasoning { opaque: Some(o), .. } if o.matches_provider(PROVIDER) && o.model == model => {
                    turns.push("model", vec![o.data.clone()])
                }
                Part::Reasoning { .. } => {}
                Part::ToolCall { call_id, name, input, kind, opaque } => {
                    names.insert(call_id, name);
                    if *kind == CallKind::Hosted {
                        continue;
                    }
                    if needs_signature && !item_signed {
                        dropped.insert(call_id, input.to_string());
                        continue;
                    }
                    let mut call = json!({"name":name,"args": if input.is_object() { input.clone() } else { json!({"input":input}) }});
                    if ids {
                        call["id"] = json!(sanitize_id(call_id, name));
                    }
                    let mut part = json!({"functionCall":call});
                    if let Some(signature) = signature(opaque.as_ref(), model) {
                        part["thoughtSignature"] = json!(signature);
                    }
                    turns.push("model", vec![part]);
                }
                Part::ToolResult { call_id, content, is_error } => {
                    let text = content
                        .iter()
                        .filter_map(|p| if let ResultPart::Text { text } = p { Some(text.as_str()) } else { None })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let text = if text.is_empty() && *is_error { "Tool failed".to_owned() } else { text };
                    let attachments = content
                        .iter()
                        .filter_map(|p| match p {
                            ResultPart::Image(m) | ResultPart::Document(m) => Some(media(m, gemini)),
                            ResultPart::Text { .. } => None,
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let name = names
                        .get(call_id.as_str())
                        .ok_or_else(|| LlmError::new(ErrorClass::InvalidRequest, "a tool result has no call in the transcript"))?;
                    let first = if let Some(args) = dropped.get(call_id.as_str()) {
                        let label = if args == "{}" { format!("`{name}`") } else { format!("`{name}` ({args})") };
                        json!({"text":format!("[Observation from {label}:\n{text}]")})
                    } else {
                        let mut response = json!({"name":name,"response": if *is_error { json!({"error":text}) } else { json!({"output":text}) }});
                        if ids {
                            response["id"] = json!(sanitize_id(call_id, name));
                        }
                        json!({"functionResponse":response})
                    };
                    turns.push("user", std::iter::once(first).chain(attachments).collect());
                }
            }
        }
    }
    let mut turns = turns.0;
    // A function-call turn must directly follow a user turn (compaction can drop that boundary).
    let mut i = 0;
    while i < turns.len() {
        if turns[i]["role"] == "model" && Turns::has_call(&turns[i]) && (i == 0 || turns[i - 1]["role"] != "user") {
            turns.insert(i, json!({"role":"user","parts":[{"text":CONTINUE}]}));
            i += 1;
        }
        i += 1;
    }
    // The backend wants a natural-language user part somewhere, even on tool-only continuations.
    let has_text = turns.iter().any(|t| {
        t["role"] == "user" && t["parts"].as_array().is_some_and(|p| p.iter().any(|p| p["text"].as_str().is_some_and(|s| !s.trim().is_empty())))
    });
    if !has_text && !turns.is_empty() {
        match turns.iter_mut().find(|t| t["role"] == "user") {
            Some(turn) => turn["parts"].as_array_mut().expect("parts").push(json!({"text":CONTINUE})),
            None => turns.insert(0, json!({"role":"user","parts":[{"text":CONTINUE}]})),
        }
    }
    if let Some(last) = turns.last().filter(|t| t["role"] == "model") {
        if Turns::has_call(last) {
            return Err(LlmError::new(ErrorClass::InvalidRequest, "the transcript ends with a tool call that has no result"));
        }
        turns.push(json!({"role":"user","parts":[{"text":CONTINUE}]}));
    }
    Ok(turns)
}

/// What the envelope carries besides the model's own request.
pub struct Envelope<'a> {
    pub project: &'a str,
    pub ids: &'a Ids,
    /// Unix milliseconds, in the `requestId`.
    pub now_ms: u64,
    /// The model's output limit (a larger `maxOutputTokens` is a 400); 0 when unknown.
    pub max_output: u64,
}

/// The `v1internal:streamGenerateContent` body.
///
/// # Errors
/// Hosted and freeform tools, structured output, unsupported media and a transcript Gemini cannot start from.
pub fn request(req: &Request, envelope: &Envelope) -> Result<Value, LlmError> {
    if req.output_schema.is_some() {
        return Err(LlmError::new(ErrorClass::InvalidRequest, "Antigravity does not support structured output"));
    }
    let model = req.model.as_str();
    let claude = model.starts_with("claude-");
    let non_gemini = !model.starts_with("gemini-");
    let contents = contents(req)?;
    let mut inner = json!({"contents": contents});
    if !req.system.is_empty() {
        inner["systemInstruction"] = json!({"role":"user","parts": req.system.iter().map(|text| json!({"text":text})).collect::<Vec<_>>()});
    }
    let mut config = json!({});
    if let Some(t) = req.temperature {
        config["temperature"] = json!(t);
    }
    if let Some(p) = req.top_p {
        config["topP"] = json!(p);
    }
    if !req.stop.is_empty() {
        config["stopSequences"] = json!(req.stop);
    }
    if let Some(thinking) = thinking(model, req.reasoning) {
        config["thinkingConfig"] = thinking;
    }
    let max = [req.max_output.map(u64::from), (envelope.max_output > 0).then_some(envelope.max_output)].into_iter().flatten().min();
    if let Some(max) = max {
        config["maxOutputTokens"] = json!(max);
    }
    if config.as_object().is_some_and(|c| !c.is_empty()) {
        inner["generationConfig"] = config;
    }
    if let Some(tools) = tools(req, claude || model.starts_with("gpt-oss-"))? {
        inner["tools"] = tools;
        let mode = match &req.tool_choice {
            ToolChoice::Auto => None,
            ToolChoice::None => Some(json!({"mode":"NONE"})),
            ToolChoice::Required => Some(json!({"mode":"ANY"})),
            ToolChoice::Tool(name) => Some(json!({"mode":"ANY","allowedFunctionNames":[name]})),
        };
        if let Some(mode) = mode {
            inner["toolConfig"] = json!({"functionCallingConfig":mode});
        }
    }
    let ids = envelope.ids;
    let step = inner["contents"].as_array().map_or(1, Vec::len).max(1);
    let request_index = req.items.iter().filter(|item| item.role == Role::Assistant).count();
    let mut labels = json!({
        "last_step_index": (step - 1).to_string(),
        "request_id": format!("{}-{request_index}", ids.trajectory),
        "trajectory_id": ids.trajectory,
        "used_claude": claude.to_string(),
        "used_claude_conservative": claude.to_string(),
        "used_non_gemini_model": non_gemini.to_string(),
    });
    if step > 1 {
        labels["last_execution_id"] = json!(stable_uuid(&format!("antigravity:exec:{}:{}", ids.trajectory, step - 1)));
    }
    inner["sessionId"] = json!(ids.session);
    inner["labels"] = labels;
    let mut body = json!({
        "project": envelope.project,
        "model": model,
        "request": inner,
        "requestType": "agent",
        "userAgent": "antigravity",
        "requestId": format!("agent/{}/{}/{}/{step}", ids.conversation, envelope.now_ms, ids.trajectory),
    });
    if let Some(extra) = &req.extra {
        crate::openai::convert::merge(&mut body, extra);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Opaque, ToolDef};

    const SIG: &str = "c2lnbmF0dXJl";

    fn opaque(model: &str, data: Value) -> Option<Opaque> {
        Some(Opaque { provider: PROVIDER.into(), model: model.into(), data })
    }
    fn tool(name: &str, schema: Value) -> ToolDef {
        ToolDef { name: name.into(), description: "Read".into(), input_schema: schema, kind: ToolKind::Function, defer_loading: false }
    }
    fn call(id: &str, opaque: Option<Opaque>) -> Part {
        Part::ToolCall { call_id: id.into(), name: "read".into(), input: json!({"path":"a"}), kind: CallKind::Function, opaque }
    }
    fn result(id: &str, text: &str) -> Part {
        Part::ToolResult { call_id: id.into(), content: vec![ResultPart::text(text)], is_error: false }
    }
    fn ids() -> Ids {
        Ids { conversation: "conv".into(), trajectory: "traj".into(), session: "42".into() }
    }
    fn envelope(ids: &Ids) -> Envelope<'_> {
        Envelope { project: "proj", ids, now_ms: 1_700_000_000_000, max_output: 65536 }
    }

    #[test]
    fn golden_envelope() {
        let model = "gemini-3.8-flash-high";
        let req = Request {
            model: model.into(),
            system: vec!["Be terse.".into()],
            items: vec![
                Item::user_text("u", "Read it"),
                Item::new(
                    "a",
                    Role::Assistant,
                    vec![
                        Part::Reasoning {
                            summary: Some("plan".into()),
                            text: None,
                            opaque: opaque(model, json!({"thought":true,"text":"plan","thoughtSignature":SIG})),
                        },
                        call("c1", opaque(model, json!({"thoughtSignature":SIG}))),
                    ],
                ),
                Item::new("t", Role::Tool, vec![result("c1", "hello")]),
            ],
            tools: vec![tool("read", json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}))],
            reasoning: Some(Reasoning::Effort(Effort::High)),
            temperature: Some(0.5),
            max_output: Some(1000),
            ..Request::default()
        };
        let ids = ids();
        let expected = json!({
            "project": "proj",
            "model": model,
            "requestType": "agent",
            "userAgent": "antigravity",
            "requestId": "agent/conv/1700000000000/traj/3",
            "request": {
                "contents": [
                    {"role":"user","parts":[{"text":"Read it"}]},
                    {"role":"model","parts":[
                        {"thought":true,"text":"plan","thoughtSignature":SIG},
                        {"functionCall":{"name":"read","args":{"path":"a"}},"thoughtSignature":SIG}
                    ]},
                    {"role":"user","parts":[{"functionResponse":{"name":"read","response":{"output":"hello"}}}]}
                ],
                "systemInstruction": {"role":"user","parts":[{"text":"Be terse."}]},
                "generationConfig": {"temperature":0.5,"maxOutputTokens":1000,"thinkingConfig":{"includeThoughts":true,"thinkingBudget":-1}},
                "tools": [{"functionDeclarations":[{"name":"read","description":"Read","parametersJsonSchema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}]}],
                "sessionId": "42",
                "labels": {
                    "last_step_index": "2",
                    "request_id": "traj-1",
                    "trajectory_id": "traj",
                    "used_claude": "false",
                    "used_claude_conservative": "false",
                    "used_non_gemini_model": "false",
                    "last_execution_id": stable_uuid("antigravity:exec:traj:2")
                }
            }
        });
        assert_eq!(request(&req, &envelope(&ids)).unwrap(), expected);
    }

    #[test]
    fn claude_gets_ids_legacy_schemas_and_a_thinking_budget() {
        let req = Request {
            model: "claude-sonnet-4-6".into(),
            items: vec![
                Item::user_text("u", "go"),
                Item::new("a", Role::Assistant, vec![call("toolu.1", None)]),
                Item::new("t", Role::Tool, vec![result("toolu.1", "ok")]),
            ],
            tools: vec![tool(
                "read",
                json!({"$schema":"x","type":"object","properties":{"path":{"type":["string","null"],"format":"uri","description":"p"},"mode":{"enum":[1,2]}},"additionalProperties":false}),
            )],
            tool_choice: ToolChoice::Required,
            reasoning: Some(Reasoning::Effort(Effort::Low)),
            ..Request::default()
        };
        let body = request(&req, &envelope(&ids())).unwrap();
        let inner = &body["request"];
        assert_eq!(inner["contents"][1]["parts"][0]["functionCall"]["id"], "toolu_1");
        assert_eq!(inner["contents"][2]["parts"][0]["functionResponse"]["id"], "toolu_1");
        assert_eq!(
            inner["tools"][0]["functionDeclarations"][0]["parameters"],
            json!({"type":"object","properties":{"path":{"type":"string","description":"p"},"mode":{}}})
        );
        assert_eq!(inner["toolConfig"], json!({"functionCallingConfig":{"mode":"ANY"}}));
        assert_eq!(inner["generationConfig"]["thinkingConfig"], json!({"includeThoughts":true,"thinkingBudget":1024}));
        assert_eq!(inner["labels"]["used_claude"], "true");
        assert_eq!(inner["labels"]["used_non_gemini_model"], "true");
    }

    #[test]
    fn unsigned_gemini_3_calls_become_observations() {
        let req = Request {
            model: "gemini-3.8-flash-low".into(),
            items: vec![
                Item::user_text("u", "go"),
                Item::new("a", Role::Assistant, vec![call("c1", None)]),
                Item::new("t", Role::Tool, vec![result("c1", "ok")]),
            ],
            ..Request::default()
        };
        let contents = contents(&req).unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["parts"], json!([{"text":"go"},{"text":"[Observation from `read` ({\"path\":\"a\"}):\nok]"}]));
        // Gemini 2 takes unsigned calls.
        let old = Request { model: "gemini-2.5-flash".into(), ..req };
        assert_eq!(super::contents(&old).unwrap().len(), 3);
    }

    #[test]
    fn boundaries_are_repaired() {
        let model = "gemini-3.8-flash-low";
        let mut req = Request { model: model.into(), items: vec![Item::new("a", Role::Assistant, vec![Part::text("hi")])], ..Request::default() };
        let c = contents(&req).unwrap();
        let bridge = json!({"role":"user","parts":[{"text":CONTINUE}]});
        assert_eq!(c, [bridge.clone(), json!({"role":"model","parts":[{"text":"hi"}]}), bridge]);
        req.items =
            vec![Item::user_text("u", "go"), Item::new("a", Role::Assistant, vec![call("c1", opaque(model, json!({"thoughtSignature":SIG})))])];
        assert_eq!(contents(&req).unwrap_err().class, ErrorClass::InvalidRequest);
        req.output_schema = Some(json!({}));
        assert_eq!(request(&req, &envelope(&ids())).unwrap_err().class, ErrorClass::InvalidRequest);
    }

    #[test]
    fn schemas_are_made_self_contained() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$defs": {"Pos": {"type":"object","properties":{"x":{"type":"integer"}}}},
            "type": "object",
            "properties": {"at": {"$ref":"#/$defs/Pos","description":"where"}, "$id": {"type":"string"}}
        });
        assert_eq!(
            sanitize_schema(&schema).unwrap(),
            json!({"type":"object","properties":{"at":{"type":"object","properties":{"x":{"type":"integer"}},"description":"where"},"$id":{"type":"string"}}})
        );
        let circular = json!({"$defs":{"A":{"$ref":"#/$defs/A"}},"properties":{"a":{"$ref":"#/$defs/A"}}});
        assert!(sanitize_schema(&circular).is_err());
        assert!(sanitize_schema(&json!({"properties":{"a":{"$ref":"#/nope"}}})).is_err());
        assert_eq!(sanitize_schema(&Value::Null).unwrap(), json!({"type":"object","properties":{}}));
        assert_eq!(sanitize_schema(&json!({"properties":{}})).unwrap()["type"], "object");
        // A tool whose schema cannot be resolved is left out.
        let req = Request { tools: vec![tool("bad", json!({"properties":{"a":{"$ref":"#/nope"}}}))], ..Request::default() };
        assert_eq!(tools(&req, false).unwrap(), None);
    }

    #[test]
    fn thinking_budgets_follow_the_family() {
        let high = Some(Reasoning::Effort(Effort::High));
        let cfg = |model, r| thinking(model, r).map(|v| (v["includeThoughts"].as_bool().unwrap(), v["thinkingBudget"].as_i64().unwrap()));
        assert_eq!(cfg("gemini-3.8-flash-low", None), Some((false, 0)));
        assert_eq!(cfg("gemini-3.8-flash-low", Some(Reasoning::Effort(Effort::Medium))), Some((true, 4000)));
        assert_eq!(cfg("gemini-3.5-flash-low", high), Some((true, 10_000)));
        assert_eq!(cfg("gemini-pro-agent", Some(Reasoning::Effort(Effort::Low))), Some((true, 1001)));
        assert_eq!(cfg("gemini-pro-agent", high), Some((true, 10_001)));
        assert_eq!(cfg("gpt-oss-120b-medium", high), Some((true, 8192)));
        assert_eq!(cfg("gemini-4-argon", None), None);
        assert_eq!(cfg("gemini-4-argon", high), Some((true, -1)));
        assert_eq!(cfg("gemini-3.8-flash-low", Some(Reasoning::Budget(0))), Some((false, 0)));
        assert_eq!(cfg("something-else", high), None);
    }

    #[test]
    fn ids_are_stable_per_session() {
        let a = Ids::new(Some("ses_1"));
        assert_eq!(a, Ids::new(Some("ses_1")));
        assert_ne!(a, Ids::new(Some("ses_2")));
        assert_ne!(Ids::new(None), Ids::new(None));
        assert_eq!(Ids::new(Some("77")).session, "77");
        assert!(a.session.parse::<i64>().is_ok());
        assert_eq!(a.conversation.len(), 36);
        assert!(requires_signature("gemini-pro-agent") && requires_signature("gemini-3-flash-agent") && !requires_signature("gemini-2.5-pro"));
    }
}
