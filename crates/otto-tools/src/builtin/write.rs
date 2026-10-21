use super::*;
use crate::ToolOutput;
tool!(
    Write,
    spec(
        "write",
        "Create or replace a UTF-8 file, creating parent directories. Read existing files first; prefer edit for targeted changes. Returns unified diff and line counts in metadata.",
        json!({"path":{"type":"string"},"content":{"type":"string"}}),
        &["path", "content"],
        Effects::WRITE
    ),
    run
);
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let path = path(&input, &cx)?;
    let new = string(&input, "content")?;
    let old = match regular_text(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io(e)),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    atomic_write(&path, new).map_err(io)?;
    Ok(ToolOutput::text(format!("Wrote {}", path.display())).with_metadata(diff(&path, &old, new)))
}
