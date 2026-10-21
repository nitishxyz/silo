use super::*;
use crate::ToolOutput;
tool!(
    UpdateTodos,
    spec(
        "update_todos",
        "Show the user a short task plan for multi-step work; skip it for simple requests. Each call replaces the whole list. Keep exactly one item in_progress, mark items completed as soon as they are done, and update the list when the scope changes. Steps are short imperative phrases.",
        json!({"todos":{"type":"array","items":{"type":"object","properties":{"step":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed","cancelled"]}},"required":["step","status"],"additionalProperties":false}}}),
        &["todos"],
        Effects::NONE
    ),
    run
);
async fn run(input: Value, _cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let todos = input.get("todos").and_then(Value::as_array).ok_or_else(|| ToolError::invalid("todos must be an array"))?;
    let mut done = 0;
    for todo in todos {
        if todo.get("step").and_then(Value::as_str).is_none_or(|s| s.trim().is_empty()) {
            return Err(ToolError::invalid("each todo needs a non-empty step"));
        }
        match todo.get("status").and_then(Value::as_str) {
            Some("completed") => done += 1,
            Some("pending" | "in_progress" | "cancelled") => {}
            _ => return Err(ToolError::invalid("status must be pending, in_progress, completed or cancelled")),
        }
    }
    Ok(ToolOutput::text(format!("{done}/{} done", todos.len())))
}
