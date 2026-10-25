//! Method discovery with draft 2020-12 JSON Schemas and executable examples.

use schemars::{Schema, schema_for};
use serde_json::{Value, json};

use crate::{control::*, model::*};

/// Description, schemas and example for a known method.
#[must_use]
pub fn describe(name: &str) -> Option<MethodDescription> {
    let (_, description) = METHODS.iter().find(|(n, _)| *n == name)?;
    let method = examples().into_iter().find(|m| method_name(m) == name)?;
    Some(MethodDescription {
        description: (*description).into(),
        params_schema: params_schema(name)?,
        result_schema: result_schema(&method).to_value(),
        example: serde_json::to_value(method).ok()?,
    })
}

#[must_use]
pub fn method_name(method: &Method) -> String {
    serde_json::to_value(method).ok().and_then(|v| v["method"].as_str().map(str::to_owned)).unwrap_or_default()
}

#[must_use]
pub fn schema_names() -> Vec<String> {
    variants(&schema_for!(Method).to_value()).filter_map(|v| tag(v).map(str::to_owned)).collect()
}

fn variants(root: &Value) -> impl Iterator<Item = &Value> {
    ["oneOf", "anyOf"].into_iter().filter_map(|k| root.get(k)?.as_array()).flatten()
}

fn tag(variant: &Value) -> Option<&str> {
    let method = variant.get("properties")?.get("method")?;
    method.get("const").or_else(|| method.get("enum")?.get(0))?.as_str()
}

fn params_schema(name: &str) -> Option<Value> {
    let root = schema_for!(Method).to_value();
    let variant = variants(&root).find(|v| tag(v) == Some(name))?;
    let mut params = variant.get("properties")?.get("params").cloned().unwrap_or_else(|| json!({"type": "object"}));
    if let (Some(defs), Some(object)) = (root.get("$defs"), params.as_object_mut()) {
        object.insert("$defs".into(), defs.clone());
    }
    params.as_object_mut()?.insert("$schema".into(), root["$schema"].clone());
    Some(params)
}

fn result_schema(method: &Method) -> Schema {
    use Method as M;
    match method {
        M::ServerInfo {} => schema_for!(ServerInfo),
        M::ApiList {} => schema_for!(Vec<ApiMethod>),
        M::ApiDescribe { .. } => schema_for!(MethodDescription),
        M::ProjectOpen { .. } => schema_for!(ProjectInfo),
        M::ProjectList {} => schema_for!(Vec<ProjectInfo>),
        M::SessionCreate { .. } | M::SessionFork { .. } | M::SessionUpdate { .. } => schema_for!(SessionInfo),
        M::SessionList { .. } => schema_for!(Vec<SessionInfo>),
        M::SessionView { .. } => schema_for!(SessionView),
        M::SessionSubscribe { .. } => schema_for!(Subscribed),
        M::SessionPrompt { .. } => schema_for!(PromptAccepted),
        M::AttachmentPut { .. } => schema_for!(AttachmentInfo),
        M::ModelList { .. } => schema_for!(Vec<otto_llm::ModelInfo>),
        M::AuthStatus {} => schema_for!(Vec<AuthStatus>),
        M::AuthLogin { .. } => schema_for!(LoginResult),
        M::AuthLoginComplete { .. } => schema_for!(AuthStatus),
        M::ToolsList { .. } => schema_for!(Vec<ToolInfo>),
        M::AgentsList {} => schema_for!(Vec<AgentInfo>),
        M::AgentsGet { .. } | M::AgentsSave { .. } => schema_for!(AgentDef),
        M::ToolsCatalog { .. } => schema_for!(Vec<ToolCatalogEntry>),
        M::SkillsList { .. } => schema_for!(Vec<SkillInfo>),
        M::CommandsList { .. } => schema_for!(Vec<CommandInfo>),
        M::FilesComplete { .. } => schema_for!(Vec<FileMatch>),
        M::ConfigGet { .. } => schema_for!(ConfigValue),
        M::PlanUsage { .. } => schema_for!(Vec<ProviderPlan>),
        M::JobList { .. } => schema_for!(Vec<JobInfo>),
        M::ServerShutdown {}
        | M::Ping {}
        | M::SessionUnsubscribe { .. }
        | M::SessionRename { .. }
        | M::SessionSetMode { .. }
        | M::SessionArchive { .. }
        | M::SessionQueueRemove { .. }
        | M::SessionQueueUpdate { .. }
        | M::SessionCancel { .. }
        | M::SessionHandBack { .. }
        | M::SessionCompact { .. }
        | M::ShellDetach { .. }
        | M::JobKill { .. }
        | M::ApprovalAnswer { .. }
        | M::QuestionAnswer { .. }
        | M::AuthLoginCancel { .. }
        | M::AuthLogout { .. }
        | M::AgentsDelete { .. }
        | M::ConfigSet { .. } => schema_for!(Empty),
    }
}

/// One example for every method, in [`METHODS`] order.
#[must_use]
pub fn examples() -> Vec<Method> {
    use Method as M;
    vec![
        M::ServerInfo {},
        M::ServerShutdown {},
        M::ApiList {},
        M::ApiDescribe { method: "session.prompt".into() },
        M::Ping {},
        M::ProjectOpen { root: String::new(), cwd: Some("/work/project/src".into()) },
        M::ProjectList {},
        M::SessionCreate {
            project_root: Some("/work/project".into()),
            project_id: None,
            agent: None,
            approval_mode: None,
            model: None,
            provider: None,
            title: Some("Review".into()),
            parent: None,
            effort: None,
        },
        M::SessionList { project_id: None, include_archived: false },
        M::SessionView { session_id: "s1".into(), limit: None, before_seq: None },
        M::SessionSubscribe { session_id: "s1".into(), after_seq: Some(42) },
        M::SessionUnsubscribe { session_id: "s1".into() },
        M::SessionFork { session_id: "s1".into(), at_seq: None },
        M::SessionRename { session_id: "s1".into(), title: "Review".into() },
        M::SessionUpdate {
            session_id: "s1".into(),
            provider: None,
            model: Some("example-model".into()),
            agent: None,
            effort: Some(otto_llm::Effort::High),
            reset_effort: false,
        },
        M::SessionSetMode { session_id: "s1".into(), approval_mode: crate::model::ApprovalMode::AskOnEffects },
        M::SessionArchive { session_id: "s1".into() },
        M::SessionPrompt { session_id: "s1".into(), input: vec![InputPart::Text { text: "Review this project".into() }], delivery: Delivery::Queue },
        M::AttachmentPut {
            session_id: "s1".into(),
            name: "notes.txt".into(),
            mime: "text/plain".into(),
            data_base64: "aGVsbG8K".into(),
            append_to: None,
        },
        M::SessionQueueRemove { session_id: "s1".into(), input_id: "i1".into() },
        M::SessionQueueUpdate { session_id: "s1".into(), input_id: "i1".into(), delivery: Delivery::Steer },
        M::SessionCancel { session_id: "s1".into() },
        M::SessionHandBack { session_id: "s1".into() },
        M::SessionCompact { session_id: "s1".into() },
        M::ShellDetach { session_id: "s1".into(), call_id: "call1".into() },
        M::JobList { session_id: "s1".into() },
        M::JobKill { session_id: "s1".into(), job_id: "job-1".into() },
        M::ApprovalAnswer { session_id: "s1".into(), approval_id: "a1".into(), decision: ApprovalDecision::Allow, note: None },
        M::QuestionAnswer { session_id: "s1".into(), question_id: "q1".into(), answer: "Yes".into() },
        M::ModelList { provider: None },
        M::AuthStatus {},
        M::AuthLogin { provider: "openai".into(), mode: AuthMode::Subscription, api_key: None, device: true, base_url: None, paste: false },
        M::AuthLoginComplete { login_id: "login1".into(), code: None },
        M::AuthLoginCancel { login_id: "login1".into() },
        M::AuthLogout { provider: "openai".into() },
        M::ToolsList { session_id: Some("s1".into()) },
        M::AgentsList {},
        M::AgentsGet { name: "reviewer".into(), cwd: Some("/work/project".into()) },
        M::AgentsSave {
            agent: AgentDef {
                name: "reviewer".into(),
                description: "Reviews changes".into(),
                model: Some("anthropic/claude-sonnet-5-5".into()),
                effort: Some("high".into()),
                approval_mode: None,
                prompt: "Review the diff and report problems.".into(),
                tools: vec!["read".into(), "search".into()],
                loadable: vec!["mcp__github__*".into()],
                disallowed: vec![],
                scope: AgentScope::Project,
                overrides_builtin: false,
            },
            previous_name: None,
            cwd: Some("/work/project".into()),
        },
        M::AgentsDelete { name: "reviewer".into(), scope: AgentScope::Project, cwd: Some("/work/project".into()) },
        M::ToolsCatalog { cwd: Some("/work/project".into()) },
        M::SkillsList { project_root: Some("/work/project".into()) },
        M::CommandsList { project_root: Some("/work/project".into()) },
        M::FilesComplete { cwd: "/work/project".into(), query: "src/ma".into(), limit: 50 },
        M::ConfigGet { key: None },
        M::ConfigSet { key: "model".into(), value: json!("example-model") },
        M::PlanUsage { provider: Some("anthropic".into()) },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(schema: &Value, root: &Value, value: &Value) -> Result<(), String> {
        let fail = |why: &str| Err(format!("{why}: {value}"));
        if schema == &Value::Bool(false) {
            return fail("false schema");
        }
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let sub = root.pointer(reference.strip_prefix('#').ok_or("external ref")?).ok_or("missing ref")?;
            check(sub, root, value)?;
        }
        if let Some(types) = schema.get("type") {
            let types: Vec<_> = types.as_array().map_or_else(|| vec![types.as_str().unwrap()], |a| a.iter().filter_map(Value::as_str).collect());
            if !types.iter().any(|t| match *t {
                "null" => value.is_null(),
                "boolean" => value.is_boolean(),
                "integer" => value.is_i64() || value.is_u64(),
                "number" => value.is_number(),
                "string" => value.is_string(),
                "array" => value.is_array(),
                "object" => value.is_object(),
                _ => false,
            }) {
                return fail("type");
            }
        }
        if schema.get("const").is_some_and(|c| c != value) {
            return fail("const");
        }
        if schema.get("enum").and_then(Value::as_array).is_some_and(|a| !a.contains(value)) {
            return fail("enum");
        }
        if schema.get("minimum").and_then(Value::as_f64).zip(value.as_f64()).is_some_and(|(min, n)| n < min) {
            return fail("minimum");
        }
        if schema.get("maximum").and_then(Value::as_f64).zip(value.as_f64()).is_some_and(|(max, n)| n > max) {
            return fail("maximum");
        }
        if let Some(object) = value.as_object() {
            for key in schema.get("required").and_then(Value::as_array).into_iter().flatten() {
                if !object.contains_key(key.as_str().unwrap()) {
                    return fail("required");
                }
            }
            for (key, value) in object {
                if let Some(sub) = schema.get("properties").and_then(|p| p.get(key)).or_else(|| schema.get("additionalProperties")) {
                    check(sub, root, value)?;
                }
            }
        }
        if let Some(items) = value.as_array() {
            for (i, value) in items.iter().enumerate() {
                if let Some(sub) = schema.get("prefixItems").and_then(|p| p.get(i)).or_else(|| schema.get("items")) {
                    check(sub, root, value)?;
                }
            }
        }
        for sub in schema.get("allOf").and_then(Value::as_array).into_iter().flatten() {
            check(sub, root, value)?;
        }
        if schema.get("anyOf").and_then(Value::as_array).is_some_and(|a| !a.iter().any(|s| check(s, root, value).is_ok())) {
            return fail("anyOf");
        }
        if schema.get("oneOf").and_then(Value::as_array).is_some_and(|a| a.iter().filter(|s| check(s, root, value).is_ok()).count() != 1) {
            return fail("oneOf");
        }
        Ok(())
    }

    #[test]
    fn every_method_describes_and_its_example_validates() {
        let names: Vec<_> = METHODS.iter().map(|(n, _)| (*n).to_owned()).collect();
        assert_eq!(examples().iter().map(method_name).collect::<Vec<_>>(), names);
        let mut tags = schema_names();
        let mut sorted = names.clone();
        tags.sort();
        sorted.sort();
        assert_eq!(tags, sorted);
        for name in names {
            let d = describe(&name).unwrap();
            assert!(!d.description.is_empty());
            assert_eq!(d.example["method"], name);
            check(&d.params_schema, &d.params_schema, &d.example["params"]).unwrap_or_else(|e| panic!("{name}: {e}"));
            let request: Request = serde_json::from_value(json!({"id": 1, "method": name, "params": d.example["params"]})).unwrap();
            assert_eq!(method_name(&request.method), name);
            assert!(d.result_schema.is_object());
        }
        assert!(describe("unknown").is_none());
    }

    #[test]
    fn schemas_reject_bad_params_and_accept_results() {
        let schema = describe("session.prompt").unwrap().params_schema;
        assert!(check(&schema, &schema, &json!({"input": []})).is_err());
        assert!(check(&schema, &schema, &json!({"session_id": "s", "input": [], "delivery": "invalid"})).is_err());
        let schema = describe("ping").unwrap().result_schema;
        assert!(check(&schema, &schema, &json!({})).is_ok());
        let schema = describe("auth.login").unwrap().result_schema;
        assert!(check(&schema, &schema, &json!({"status": "pending", "url": "https://example.com", "login_id": "l"})).is_ok());
        assert!(check(&schema, &schema, &json!({"status": "pending"})).is_err());
    }
}
