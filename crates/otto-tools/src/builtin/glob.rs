use super::*;
use crate::ToolOutput;
tool!(
    Glob,
    spec(
        "glob",
        "Find files by glob relative to path (default cwd), respecting .gitignore. Use **/*.rs for recursive matches. Results are newest-modified first with deterministic path tie-breaking.",
        json!({"path":{"type":"string"},"pattern":{"type":"string"},"max_results":{"type":"integer","minimum":1},"ignore":{"type":"array","items":{"type":"string"}}}),
        &["pattern"],
        Effects::READ
    ),
    run
);
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let matcher = globset::Glob::new(string(&input, "pattern")?).map_err(|e| ToolError::invalid(e.to_string()))?.compile_matcher();
    let root = crate::policy::resolve_path(&cx.cwd, &cx.project_root, input.get("path").and_then(Value::as_str).unwrap_or("."))?.path;
    let mut entries: Vec<_> = walk(&input, &cx, None)?
        .into_iter()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()) && matcher.is_match(e.path().strip_prefix(&root).unwrap_or(e.path())))
        .collect();
    entries.sort_by_cached_key(|e| (std::cmp::Reverse(e.metadata().ok().and_then(|m| m.modified().ok())), e.path().to_path_buf()));
    let text: String = entries
        .into_iter()
        .take(input.get("max_results").and_then(Value::as_u64).unwrap_or(1000) as usize)
        .map(|e| format!("{}\n", e.path().display()))
        .collect();
    crate::output::limit(if text.is_empty() { "No matches\n".into() } else { text }, &cx)
}
