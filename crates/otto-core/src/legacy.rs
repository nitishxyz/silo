//! Sessions from before every provider got otto's own tools hold calls to
//! provider-defined ones (Claude's `bash` / `str_replace_based_edit_tool`,
//! GPT's `shell_call` / `apply_patch_call` items). Their history reads as
//! calls to ours, so providers and clients only ever see our tools.
use otto_llm::{CallKind, Item, Part};
use serde_json::{Value, json};

pub(crate) fn items(mut items: Vec<Item>) -> Vec<Item> {
    for part in items.iter_mut().flat_map(|i| &mut i.parts) {
        if let Part::ToolCall { name, input, kind, opaque, .. } = part {
            let raw = opaque.as_ref().and_then(|o| o.data["type"].as_str()).map(str::to_owned);
            if let Some((tool, mapped)) = call(name, input, raw.as_deref()) {
                *kind = if mapped.is_string() { CallKind::Custom } else { CallKind::Function };
                (*name, *input, *opaque) = (tool.into(), mapped, None);
            }
        }
    }
    items
}

fn call(name: &str, input: &Value, raw: Option<&str>) -> Option<(&'static str, Value)> {
    let text = |key: &str| input[key].as_str().unwrap_or_default().to_owned();
    Some(match (name, raw) {
        ("bash", _) => ("shell", json!({"command":text("command")})),
        ("str_replace_based_edit_tool", _) => match input["command"].as_str() {
            Some("view") => {
                let mut read = json!({"path":text("path")});
                if let Some(start) = input["view_range"][0].as_i64().filter(|s| *s > 0) {
                    read["offset"] = json!(start);
                    if let Some(end) = input["view_range"][1].as_i64().filter(|e| *e >= start) {
                        read["limit"] = json!(end - start + 1);
                    }
                }
                ("read", read)
            }
            Some("create") => ("write", json!({"path":text("path"),"content":text("file_text")})),
            Some("str_replace") => ("edit", json!({"path":text("path"),"old_string":text("old_str"),"new_string":text("new_str")})),
            Some("undo_edit") => ("undo_edit", json!({"path":text("path")})),
            _ => ("edit", input.clone()),
        },
        // Today's shell calls are already one `shell` entry per command (`otto-llm`).
        (_, Some("shell_call")) if input["commands"].is_array() => {
            let commands: Vec<&str> = input["commands"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            ("shell", json!({"command":commands.join("\n")}))
        }
        (_, Some("apply_patch_call")) => ("apply_patch", Value::String(patch(input))),
        _ => return None,
    })
}

/// An OpenAI `apply_patch` operation as the Codex patch our tool takes.
fn patch(operation: &Value) -> String {
    let path = operation["path"].as_str().unwrap_or_default();
    let diff = operation["diff"].as_str().unwrap_or_default().trim_end_matches('\n');
    let body = match operation["type"].as_str() {
        Some("create_file") => {
            format!(
                "*** Add File: {path}\n{}",
                diff.lines().map(|l| if l.starts_with('+') { l.to_owned() } else { format!("+{l}") }).collect::<Vec<_>>().join("\n")
            )
        }
        Some("delete_file") => format!("*** Delete File: {path}"),
        _ => format!("*** Update File: {path}\n{diff}"),
    };
    format!("*** Begin Patch\n{}\n*** End Patch", body.trim_end_matches('\n'))
}
