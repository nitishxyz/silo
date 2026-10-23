pub mod apply_patch;
pub mod ask_user;
pub mod copy_attachment;
pub mod edit;
pub mod glob;
pub mod ls;
pub mod multiedit;
pub mod read;
pub mod render;
pub mod search;
pub mod shell;
pub mod tree;
pub mod undo_edit;
pub mod update_todos;
pub mod web_fetch;
pub mod web_search;
pub mod write;
use crate::{Effects, ToolCx, ToolError, ToolSpec};
use serde_json::{Value, json};

pub(crate) fn spec(name: &str, description: &str, mut properties: Value, required: &[&str], effects: Effects) -> ToolSpec {
    if let Some(path) = properties.get_mut("path").and_then(Value::as_object_mut) {
        path.entry("description").or_insert_with(|| "Relative to the working directory (absolute also works)".into());
    }
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        output_schema: None,
        effects,
        sensitive: vec![],
        loading: Default::default(),
        mode: Default::default(),
        presentations: vec![],
        source: Default::default(),
        exclusive: effects.write,
    }
}
pub(crate) fn string<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| ToolError::invalid(format!("{key} must be a string")))
}
pub(crate) fn path(input: &Value, cx: &ToolCx) -> Result<std::path::PathBuf, ToolError> {
    crate::policy::validate_prepared(input, cx)?;
    Ok(crate::policy::resolve_path(&cx.cwd, &cx.project_root, string(input, "path")?)?.path)
}
pub(crate) fn regular_open(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Target is not a regular file"));
    }
    Ok(file)
}
pub(crate) fn regular_text(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut text = String::new();
    regular_open(path)?.read_to_string(&mut text)?;
    Ok(text)
}
pub(crate) fn io(e: std::io::Error) -> ToolError {
    ToolError::failed(e.to_string())
}
pub(crate) fn stage(path: &std::path::Path, text: &str) -> std::io::Result<tempfile::NamedTempFile> {
    use std::io::Write;
    match regular_open(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    if let Ok(metadata) = std::fs::metadata(path) {
        file.as_file().set_permissions(metadata.permissions())?;
    }
    file.write_all(text.as_bytes())?;
    file.as_file().sync_all()?;
    Ok(file)
}
pub(crate) fn atomic_write(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    stage(path, text)?.persist(path).map_err(|e| e.error)?;
    Ok(())
}
pub(crate) fn diff(path: &std::path::Path, old: &str, new: &str) -> Value {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            _ => {}
        }
    }
    json!({"path":path,"added":added,"removed":removed,"diff":diff.unified_diff().header(&path.display().to_string(), &path.display().to_string()).to_string()})
}
macro_rules! tool {
    ($name:ident, $spec:expr, $run:path) => {
        pub struct $name {
            spec: crate::ToolSpec,
        }
        impl Default for $name {
            fn default() -> Self {
                Self { spec: $spec }
            }
        }
        #[async_trait::async_trait]
        impl crate::Tool for $name {
            fn spec(&self) -> &crate::ToolSpec {
                &self.spec
            }
            async fn call(&self, input: serde_json::Value, cx: crate::ToolCx) -> Result<crate::ToolOutput, crate::ToolError> {
                crate::policy::validate_prepared(&input, &cx)?;
                $run(input, cx).await
            }
        }
    };
}
pub(crate) use tool;

pub(crate) fn walk(input: &Value, cx: &ToolCx, depth: Option<usize>) -> Result<Vec<ignore::DirEntry>, ToolError> {
    let raw = input.get("path").and_then(Value::as_str).unwrap_or(".");
    let root = crate::policy::resolve_path(&cx.cwd, &cx.project_root, raw)?.path;
    let mut overrides = ignore::overrides::OverrideBuilder::new(&root);
    if let Some(patterns) = input.get("ignore").and_then(Value::as_array) {
        for pattern in patterns {
            overrides
                .add(&format!("!{}", pattern.as_str().ok_or_else(|| ToolError::invalid("ignore must contain strings"))?))
                .map_err(|e| ToolError::invalid(e.to_string()))?;
        }
    }
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .max_depth(depth)
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| e.depth() == 0 || e.file_name() != ".git")
        .overrides(overrides.build().map_err(|e| ToolError::invalid(e.to_string()))?);
    let mut entries = Vec::new();
    for entry in builder.build() {
        if cx.cancel.is_cancelled() {
            return Err(crate::ToolError::new(crate::ToolErrorKind::Cancelled, "Cancelled"));
        }
        entries.push(entry.map_err(|e| ToolError::failed(e.to_string()))?);
    }
    Ok(entries)
}
