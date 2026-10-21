use super::*;
use crate::ToolOutput;
tool!(
    Tree,
    spec(
        "tree",
        "Show an indented directory tree respecting .gitignore and ignore globs. Depth defaults to 3 and is capped at 20; hidden files are included.",
        json!({"path":{"type":"string"},"depth":{"type":"integer","minimum":1,"maximum":20},"ignore":{"type":"array","items":{"type":"string"}}}),
        &[],
        Effects::READ
    ),
    run
);
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let depth = input.get("depth").and_then(Value::as_u64).unwrap_or(3).clamp(1, 20) as usize;
    let mut entries = walk(&input, &cx, Some(depth))?;
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    crate::output::limit(
        entries
            .into_iter()
            .map(|e| {
                format!(
                    "{}{}{}\n",
                    "  ".repeat(e.depth()),
                    e.file_name().to_string_lossy(),
                    if e.file_type().is_some_and(|t| t.is_dir()) { "/" } else { "" }
                )
            })
            .collect(),
        &cx,
    )
}
