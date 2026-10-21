use super::*;
use crate::ToolOutput;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{SearcherBuilder, sinks::UTF8};
tool!(
    Search,
    spec(
        "search",
        "Search UTF-8 file contents using a regular expression, without a shell. Respects .gitignore; optional glob filters paths. Returns file:line:text and stops at max_results (default 100). Binary files are skipped.",
        json!({"path":{"type":"string"},"pattern":{"type":"string"},"glob":{"type":"string"},"case_insensitive":{"type":"boolean"},"max_results":{"type":"integer","minimum":1},"ignore":{"type":"array","items":{"type":"string"}}}),
        &["pattern"],
        Effects::READ
    ),
    run
);
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(input.get("case_insensitive").and_then(Value::as_bool).unwrap_or(false))
        .build(string(&input, "pattern")?)
        .map_err(|e| ToolError::invalid(e.to_string()))?;
    let filter = input
        .get("glob")
        .and_then(Value::as_str)
        .map(|g| globset::Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| ToolError::invalid(e.to_string()))?;
    let root = crate::policy::resolve_path(&cx.cwd, &cx.project_root, input.get("path").and_then(Value::as_str).unwrap_or("."))?.path;
    let max = input.get("max_results").and_then(Value::as_u64).unwrap_or(100).max(1) as usize;
    let mut count = 0;
    let mut text = String::new();
    let mut entries = walk(&input, &cx, None)?;
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    for entry in entries {
        if !entry.file_type().is_some_and(|t| t.is_file())
            || filter.as_ref().is_some_and(|f| !f.is_match(entry.path().strip_prefix(&root).unwrap_or(entry.path())))
        {
            continue;
        }
        SearcherBuilder::new()
            .line_number(true)
            .binary_detection(grep_searcher::BinaryDetection::quit(0))
            .build()
            .search_path(
                &matcher,
                entry.path(),
                UTF8(|line, content| {
                    if cx.cancel.is_cancelled() {
                        return Ok(false);
                    }
                    text.push_str(&format!("{}:{line}:{}\n", entry.path().display(), content.trim_end_matches('\n')));
                    count += 1;
                    Ok(count < max)
                }),
            )
            .map_err(io)?;
        if count >= max {
            break;
        }
    }
    crate::output::limit(text, &cx)
}
