use super::*;
use crate::ToolOutput;
tool!(
    Multiedit,
    spec(
        "multiedit",
        "Apply ordered string replacements to one UTF-8 file atomically. Every edit must succeed before the file is written. Matching rules are identical to edit.",
        json!({"path":{"type":"string"},"edits":{"type":"array","minItems":1,"items":{"type":"object","properties":{"old_string":{"type":"string","minLength":1},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["old_string","new_string"],"additionalProperties":false}}}),
        &["path", "edits"],
        Effects::WRITE
    ),
    run
);
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let path = path(&input, &cx)?;
    let old = regular_text(&path).map_err(io)?;
    let mut new = old.clone();
    let edits =
        input.get("edits").and_then(Value::as_array).filter(|e| !e.is_empty()).ok_or_else(|| ToolError::invalid("edits must be a nonempty array"))?;
    for edit in edits {
        new = super::edit::replace(&new, edit)?;
    }
    atomic_write(&path, &new).map_err(io)?;
    Ok(ToolOutput::text(format!("Edited {}", path.display())).with_metadata(diff(&path, &old, &new)))
}
