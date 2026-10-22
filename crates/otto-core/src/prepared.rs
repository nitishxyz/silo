use crate::{Error, Result, session::Context};
use otto_tools::{Tool, ToolCx, policy::Prepared};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) fn public(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            object.remove("__otto_paths");
            for child in object.values_mut() {
                *child = public(child.take());
            }
        }
        Value::Array(values) => {
            for child in values {
                *child = public(child.take());
            }
        }
        _ => {}
    }
    value
}
pub(crate) async fn prepare(cx: &Context, tool: Arc<dyn Tool>, input: Value, tcx: ToolCx) -> Result<Prepared> {
    let input = public(input);
    let live = cx.live();
    let family = live.family();
    let presentation = tool.spec().presentation(&family, &live.model.info().caps.native_tools);
    let schema = presentation.and_then(|p| p.input_schema.as_ref()).unwrap_or(&tool.spec().input_schema);
    // Freeform presentations have a string contract rather than the canonical object schema.
    if presentation.is_some_and(|p| p.freeform.is_some()) && input.is_string() {
    } else {
        let validator = jsonschema::validator_for(schema).map_err(|e| Error::Invalid(format!("invalid tool schema: {e}")))?;
        validator.validate(&input).map_err(|e| Error::Invalid(format!("invalid tool input: {e}")))?;
    }
    cx.state(move |s| {
        let mut prepared = s.policy.prepare(tool.spec(), &input, &tcx)?;
        if s.policy.mode == otto_tools::policy::ApprovalMode::Auto && matches!(prepared.decision, otto_tools::policy::Decision::Ask { .. }) {
            prepared.decision = otto_tools::policy::Decision::Allow;
        }
        Ok::<_, otto_tools::ToolError>(prepared)
    })
    .await?
    .map_err(|e| Error::Invalid(e.to_string()))
}
pub(crate) fn card(tool: Option<&Arc<dyn Tool>>, original: &Value, effective: &Value, prepared: Option<&Prepared>) -> Value {
    let redact = |value: &Value| public(tool.map_or_else(|| value.clone(), |t| otto_tools::policy::redact(value, &t.spec().sensitive)));
    let resolved=prepared.map(|p|p.resolved.iter().map(|r|json!({"path":r.path,"location":match r.location{otto_tools::policy::Location::Project=>"project",otto_tools::policy::Location::Outside=>"outside"}})).collect::<Vec<_>>()).unwrap_or_default();
    json!({"original_input":redact(original),"approved_input":redact(effective),"resolved_paths":resolved})
}
