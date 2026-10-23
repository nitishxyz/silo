use super::*;
use crate::{ToolOutput, attachments};
tool!(
    CopyAttachment,
    crate::ToolSpec {
        loading: crate::Loading::Deferred,
        ..spec(
            "copy_attachment",
            "Save a file the user attached into the project, byte for byte (its id is in the [attachment att_…] note of their message). Use it to add attached images or files as project assets. Creates parent folders; refuses to replace an existing file unless overwrite is true.",
            json!({
                "attachment_id":{"type":"string","description":"The id from the attachment note, e.g. att_3f2a9c1b"},
                "path":{"type":"string","description":"Destination file, relative to the working directory (absolute also works)"},
                "overwrite":{"type":"boolean","description":"Replace an existing file (default false)"}
            }),
            &["attachment_id", "path"],
            Effects::WRITE
        )
    },
    run
);
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let id = string(&input, "attachment_id")?.to_owned();
    let dest = path(&input, &cx)?;
    let overwrite = input.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    let root = cx.attachments.clone().ok_or_else(|| ToolError::invalid("This session has no attachments"))?;
    let (attachment, source) =
        attachments::find(&root, &cx.sessions, &id).map_err(|e| ToolError::new(crate::ToolErrorKind::NotFound, e.to_string()))?;
    let target = dest.clone();
    let written = tokio::task::spawn_blocking(move || -> Result<u64, ToolError> {
        use std::io::Write;
        let mut from = regular_open(&source).map_err(io)?;
        if std::fs::symlink_metadata(&target).is_ok() && !overwrite {
            return Err(ToolError::invalid(format!("{} exists; pass overwrite: true to replace it", target.display())));
        }
        let parent = target.parent().unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent).map_err(io)?;
        if overwrite {
            if std::fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir()) {
                return Err(ToolError::invalid(format!("{} is a folder", target.display())));
            }
            let mut file = tempfile::NamedTempFile::new_in(parent).map_err(io)?;
            let n = std::io::copy(&mut from, &mut file).map_err(io)?;
            file.as_file().sync_all().map_err(io)?;
            file.persist(&target).map_err(|e| io(e.error))?;
            Ok(n)
        } else {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&target).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    ToolError::invalid(format!("{} exists; pass overwrite: true to replace it", target.display()))
                } else {
                    io(e)
                }
            })?;
            let n = std::io::copy(&mut from, &mut file).map_err(io)?;
            file.flush().map_err(io)?;
            file.sync_all().map_err(io)?;
            Ok(n)
        }
    })
    .await
    .map_err(|e| ToolError::failed(e.to_string()))??;
    Ok(ToolOutput::text(format!("Copied {} to {} ({})", attachment.name, dest.display(), attachments::size(written)))
        .with_metadata(json!({"path":dest,"bytes":written,"name":attachment.name})))
}
