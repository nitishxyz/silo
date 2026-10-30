use super::*;
use crate::ToolOutput;
pub(crate) fn properties() -> Value {
    json!({"path":{"type":"string"},"old_string":{"type":"string","minLength":1},"new_string":{"type":"string"},"replace_all":{"type":"boolean","default":false}})
}
fn edit_spec() -> crate::ToolSpec {
    spec(
        "edit",
        "Replace an exact string in an existing UTF-8 file. Supply enough context for a unique match, or set replace_all. If exact text is absent, a unique whole-line whitespace-normalized match is accepted; non-whitespace differences are never guessed.",
        properties(),
        &["path", "old_string", "new_string"],
        Effects::WRITE,
    )
}
tool!(Edit, edit_spec(), run);
pub(crate) fn replace(text: &str, input: &Value) -> Result<String, ToolError> {
    let old = string(input, "old_string")?;
    let new = string(input, "new_string")?;
    if old.is_empty() {
        return Err(ToolError::invalid("old_string must not be empty"));
    }
    let all = input.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
    let count = text.match_indices(old).count();
    if count > 1 && !all {
        return Err(ToolError::invalid(format!("Found {count} matches; provide unique context or replace_all")));
    }
    if count > 0 {
        return Ok(if all { text.replace(old, new) } else { text.replacen(old, new, 1) });
    }
    let normalize = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let wanted: Vec<_> = old.lines().map(normalize).collect();
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    let mut spans = Vec::new();
    let mut start = 0;
    for i in 0..lines.len() {
        if i + wanted.len() <= lines.len() && !wanted.is_empty() && lines[i..i + wanted.len()].iter().map(|s| normalize(s)).eq(wanted.iter().cloned())
        {
            let mut end = start + lines[i..i + wanted.len()].iter().map(|s| s.len()).sum::<usize>();
            if !old.ends_with('\n') && text[..end].ends_with('\n') {
                end -= 1;
                if text[..end].ends_with('\r') {
                    end -= 1;
                }
            }
            spans.push((start, end));
        }
        start += lines[i].len();
    }
    if spans.len() != 1 {
        return Err(ToolError::invalid(format!("No exact match; found {} whitespace-normalized matches (requires one)", spans.len())));
    }
    let (start, end) = spans[0];
    Ok(format!("{}{new}{}", &text[..start], &text[end..]))
}
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let path = path(&input, &cx)?;
    let old = regular_text(&path).map_err(io)?;
    let new = replace(&old, &input)?;
    atomic_write(&path, &new).map_err(io)?;
    super::undo_edit::record(&cx, &path, Some(&old), Some(&new)).await;
    Ok(ToolOutput::text(format!("Edited {}", path.display())).with_metadata(diff(&path, &old, &new)))
}
