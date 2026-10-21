use super::*;
use crate::ToolOutput;
tool!(
    Ls,
    spec(
        "ls",
        "List immediate directory children, respecting .gitignore and optional ignore globs. Directories end in '/'. Hidden files are included.",
        json!({"path":{"type":"string"},"ignore":{"type":"array","items":{"type":"string"}}}),
        &[],
        Effects::READ
    ),
    run
);
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let mut entries = walk(&input, &cx, Some(1))?;
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    crate::output::limit(
        entries
            .into_iter()
            .filter(|e| e.depth() > 0)
            .map(|e| format!("{}{}\n", e.file_name().to_string_lossy(), if e.file_type().is_some_and(|t| t.is_dir()) { "/" } else { "" }))
            .collect(),
        &cx,
    )
}
