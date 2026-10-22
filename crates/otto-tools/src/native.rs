//! Provider-defined client tools (Anthropic `text_editor`/`bash`, OpenAI
//! `apply_patch`/`shell`). Models were trained on their input shapes; otto maps
//! each call onto a built-in tool, so policy, hooks and the journal see the
//! built-in's canonical input.
use crate::{ToolCx, ToolError, ToolOutput, policy::resolve_path};
use serde_json::{Value, json};

/// Provider type of Anthropic's editor (model-facing name `str_replace_based_edit_tool`).
pub const TEXT_EDITOR: &str = "text_editor_20250728";
/// Provider type of Anthropic's shell (model-facing name `bash`).
pub const BASH: &str = "bash_20250124";

fn text<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| ToolError::invalid(format!("{key} must be a string")))
}

/// The built-in tool and its canonical input for a native call to `native`.
/// # Errors
/// Unknown commands or missing fields.
pub fn canonical(native: &str, input: &Value, cx: &ToolCx) -> Result<(String, Value), ToolError> {
    match native {
        t if t.starts_with("text_editor_") => editor(input, cx),
        t if t.starts_with("bash_") => {
            if input["restart"] == true {
                // Every shell call is a fresh process; there is no session to restart.
                return Ok(("shell".into(), json!({"command":"echo 'Shell restarted.'"})));
            }
            Ok(("shell".into(), json!({"command":text(input, "command")?})))
        }
        "apply_patch" => Ok(("apply_patch".into(), json!({"input":patch(input)?}))),
        "shell" => {
            let commands: Vec<&str> = input["commands"].as_array().map(|c| c.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            if commands.is_empty() {
                return Err(ToolError::invalid("commands must be a nonempty array of strings"));
            }
            let mut shell = json!({"command":commands.join("\n")});
            if let Some(timeout) = input["timeout_ms"].as_u64().filter(|t| *t > 0) {
                shell["timeout_ms"] = json!(timeout);
            }
            Ok(("shell".into(), shell))
        }
        other => Err(ToolError::invalid(format!("unsupported native tool {other}"))),
    }
}

fn editor(input: &Value, cx: &ToolCx) -> Result<(String, Value), ToolError> {
    let path = text(input, "path")?;
    match text(input, "command")? {
        "view" => {
            if resolve_path(&cx.cwd, &cx.project_root, path)?.path.is_dir() {
                return Ok(("ls".into(), json!({"path":path})));
            }
            let mut read = json!({"path":path});
            if let Some(range) = input["view_range"].as_array() {
                let start = range.first().and_then(Value::as_i64).unwrap_or(1).max(1);
                read["offset"] = json!(start);
                if let Some(end) = range.get(1).and_then(Value::as_i64).filter(|end| *end >= start) {
                    read["limit"] = json!(end - start + 1);
                }
            }
            Ok(("read".into(), read))
        }
        "create" => Ok(("write".into(), json!({"path":path,"content":text(input, "file_text")?}))),
        "str_replace" => {
            Ok(("edit".into(), json!({"path":path,"old_string":text(input, "old_str")?,"new_string":input["new_str"].as_str().unwrap_or_default()})))
        }
        "insert" => {
            let line = input["insert_line"].as_u64().ok_or_else(|| ToolError::invalid("insert_line must be a nonnegative integer"))? as usize;
            let new = text(input, "insert_text")?;
            let resolved = resolve_path(&cx.cwd, &cx.project_root, path)?;
            let old = crate::builtin::regular_text(&resolved.path).map_err(crate::builtin::io)?;
            let lines: Vec<&str> = old.split_inclusive('\n').collect();
            if line > lines.len() {
                return Err(ToolError::invalid(format!("insert_line {line} is past the end ({} lines)", lines.len())));
            }
            let mut content: String = lines[..line].concat();
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(new);
            if !new.ends_with('\n') && line < lines.len() {
                content.push('\n');
            }
            content.push_str(&lines[line..].concat());
            Ok(("write".into(), json!({"path":path,"content":content})))
        }
        "undo_edit" => Err(ToolError::invalid("undo_edit is not supported; edit the file again")),
        other => Err(ToolError::invalid(format!("unknown command {other}"))),
    }
}

/// An OpenAI `apply_patch` operation as a Codex patch for the built-in tool.
fn patch(operation: &Value) -> Result<String, ToolError> {
    let path = text(operation, "path")?;
    let diff = operation["diff"].as_str().unwrap_or_default().trim_end_matches('\n');
    let body = match text(operation, "type")? {
        "create_file" => {
            let lines: Vec<String> = diff.lines().map(|l| if l.starts_with('+') { l.to_owned() } else { format!("+{l}") }).collect();
            format!("*** Add File: {path}\n{}", lines.join("\n"))
        }
        "update_file" => format!("*** Update File: {path}\n{diff}"),
        "delete_file" => format!("*** Delete File: {path}"),
        other => return Err(ToolError::invalid(format!("unknown operation {other}"))),
    };
    Ok(format!("*** Begin Patch\n{}\n*** End Patch", body.trim_end_matches('\n')))
}

/// What the model gets back for a native call, when it differs from the
/// built-in's text: OpenAI `shell` takes the documented output array.
#[must_use]
pub fn result_text(native: &str, output: &ToolOutput) -> Option<String> {
    if native != "shell" {
        return None;
    }
    let outcome = match &output.metadata["termination"] {
        Value::String(kind) if kind == "timeout" => json!({"type":"timeout"}),
        _ => json!({"type":"exit","exit_code":output.metadata["exit_code"].as_i64().unwrap_or(i64::from(output.is_error))}),
    };
    Some(json!([{"stdout":output.joined_text(),"stderr":"","outcome":outcome}]).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_commands_map_to_builtins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\n").unwrap();
        let cx = ToolCx::new("c", dir.path());
        let call = |input: Value| canonical(TEXT_EDITOR, &input, &cx).unwrap();
        assert_eq!(call(json!({"command":"view","path":"f.txt","view_range":[2,-1]})), ("read".into(), json!({"path":"f.txt","offset":2})));
        assert_eq!(call(json!({"command":"view","path":"."})).0, "ls");
        assert_eq!(call(json!({"command":"create","path":"n","file_text":"x"})), ("write".into(), json!({"path":"n","content":"x"})));
        assert_eq!(call(json!({"command":"str_replace","path":"f.txt","old_str":"a","new_str":"z"})).0, "edit");
        assert_eq!(call(json!({"command":"insert","path":"f.txt","insert_line":1,"insert_text":"mid"})).1["content"], "a\nmid\nb\n");
        assert_eq!(call(json!({"command":"insert","path":"f.txt","insert_line":0,"insert_text":"top\n"})).1["content"], "top\na\nb\n");
        assert!(canonical(TEXT_EDITOR, &json!({"command":"undo_edit","path":"f.txt"}), &cx).is_err());
        assert_eq!(canonical(BASH, &json!({"command":"ls"}), &cx).unwrap(), ("shell".into(), json!({"command":"ls"})));
    }

    #[test]
    fn openai_operations_become_patches_and_shell_results_arrays() {
        let cx = ToolCx::new("c", ".");
        let (tool, input) = canonical("apply_patch", &json!({"type":"update_file","path":"a.py","diff":"@@\n-x\n+y\n"}), &cx).unwrap();
        assert_eq!(tool, "apply_patch");
        assert_eq!(input["input"], "*** Begin Patch\n*** Update File: a.py\n@@\n-x\n+y\n*** End Patch");
        let (_, input) = canonical("apply_patch", &json!({"type":"create_file","path":"n.py","diff":"one\n+two"}), &cx).unwrap();
        assert_eq!(input["input"], "*** Begin Patch\n*** Add File: n.py\n+one\n+two\n*** End Patch");
        let (_, input) = canonical("apply_patch", &json!({"type":"delete_file","path":"n.py"}), &cx).unwrap();
        assert_eq!(input["input"], "*** Begin Patch\n*** Delete File: n.py\n*** End Patch");
        let (tool, input) = canonical("shell", &json!({"commands":["ls","pwd"],"timeout_ms":5000}), &cx).unwrap();
        assert_eq!((tool.as_str(), input), ("shell", json!({"command":"ls\npwd","timeout_ms":5000})));
        let output = ToolOutput::text("out").with_metadata(json!({"exit_code":3}));
        let array: Value = serde_json::from_str(&result_text("shell", &output).unwrap()).unwrap();
        assert_eq!(array, json!([{"stdout":"out","stderr":"","outcome":{"type":"exit","exit_code":3}}]));
        assert!(result_text("apply_patch", &output).is_none());
    }
}
