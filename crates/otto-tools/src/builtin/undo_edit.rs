//! `undo_edit`: the calling session's file edits (`edit`, `multiedit`, `write`,
//! `apply_patch`), kept in memory newest last and taken back one at a time.
use super::*;
use crate::ToolOutput;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, PoisonError},
};

/// Edits kept per session; the oldest go first.
const KEPT: usize = 64;
/// A file before and after one edit (`None`: absent).
struct Change {
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
}
static CHANGES: LazyLock<Mutex<HashMap<String, Vec<Change>>>> = LazyLock::new(Default::default);

fn session(cx: &ToolCx) -> String {
    cx.sessions.first().cloned().unwrap_or_default()
}

/// `path` went from `before` to `after` by an edit of `cx`'s session.
pub(crate) fn record(cx: &ToolCx, path: &Path, before: Option<&str>, after: Option<&str>) {
    let mut all = CHANGES.lock().unwrap_or_else(PoisonError::into_inner);
    let changes = all.entry(session(cx)).or_default();
    changes.push(Change { path: path.into(), before: before.map(Into::into), after: after.map(Into::into) });
    if changes.len() > KEPT {
        changes.remove(0);
    }
}

fn undo_spec() -> crate::ToolSpec {
    let mut spec = spec(
        "undo_edit",
        "Undo this session's last edit to a file (by edit, multiedit, write or apply_patch); call again to undo the one before. Refused when the file changed after that edit.",
        json!({"path":{"type":"string"}}),
        &["path"],
        Effects::WRITE,
    );
    spec.loading = crate::Loading::Deferred;
    spec
}
tool!(UndoEdit, undo_spec(), run);

async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let path = path(&input, &cx)?;
    let mut all = CHANGES.lock().unwrap_or_else(PoisonError::into_inner);
    let changes = all.entry(session(&cx)).or_default();
    let at = changes
        .iter()
        .rposition(|c| c.path == path)
        .ok_or_else(|| ToolError::invalid(format!("Nothing to undo: this session has no edit to {} left to take back", path.display())))?;
    let current = match regular_text(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io(e)),
    };
    if current != changes[at].after {
        return Err(ToolError::invalid(format!("{} changed after this session's last edit to it; read it and edit it instead", path.display())));
    }
    match &changes[at].before {
        Some(before) => atomic_write(&path, before).map_err(io)?,
        None => std::fs::remove_file(&path).map_err(io)?,
    }
    let change = changes.remove(at);
    let (old, new) = (current.unwrap_or_default(), change.before.unwrap_or_default());
    Ok(ToolOutput::text(format!("Undid the last edit to {}", path.display())).with_metadata(diff(&path, &old, &new)))
}
