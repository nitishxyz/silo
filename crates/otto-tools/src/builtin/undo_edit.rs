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
/// A file before and after one edit (`None`: absent), `mark` its place among all sessions' edits.
struct Change {
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
    mark: u64,
}
static CHANGES: LazyLock<Mutex<HashMap<String, Vec<Change>>>> = LazyLock::new(Default::default);
/// The last edit's mark (any session's); edits after a [`mark`] have greater ones.
static MARK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Where the edit records are now: [`edited_since`] this mark lists the edits made after it.
#[must_use]
pub fn mark() -> u64 {
    MARK.load(std::sync::atomic::Ordering::SeqCst)
}

/// A file `session`'s edits changed since a [`mark`], with the lines added and removed between its content
/// before the first of them and after the last. Edits taken back by `undo_edit` don't count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edited {
    pub path: PathBuf,
    pub added: u64,
    pub removed: u64,
}

/// The files `session` (its id) changed with the edit tools after `mark`, in the order first edited; a file
/// back as it was is left out. Only the last 64 edits per session are kept, so a long turn may miss some.
#[must_use]
pub fn edited_since(session: &str, mark: u64) -> Vec<Edited> {
    let all = CHANGES.lock().unwrap_or_else(PoisonError::into_inner);
    let mut files: Vec<(&Path, Option<&str>, Option<&str>)> = Vec::new();
    for change in all.get(session).into_iter().flatten().filter(|c| c.mark > mark) {
        match files.iter_mut().find(|f| f.0 == change.path) {
            Some(file) => file.2 = change.after.as_deref(),
            None => files.push((&change.path, change.before.as_deref(), change.after.as_deref())),
        }
    }
    files
        .into_iter()
        .filter(|(_, before, after)| before != after)
        .map(|(path, before, after)| {
            let (mut added, mut removed) = (0, 0);
            for change in similar::TextDiff::from_lines(before.unwrap_or_default(), after.unwrap_or_default()).iter_all_changes() {
                match change.tag() {
                    similar::ChangeTag::Insert => added += 1,
                    similar::ChangeTag::Delete => removed += 1,
                    similar::ChangeTag::Equal => {}
                }
            }
            Edited { path: path.to_path_buf(), added, removed }
        })
        .collect()
}

fn session(cx: &ToolCx) -> String {
    cx.sessions.first().cloned().unwrap_or_default()
}

/// `path` went from `before` to `after` by an edit of `cx`'s session.
pub(crate) fn record(cx: &ToolCx, path: &Path, before: Option<&str>, after: Option<&str>) {
    let mut all = CHANGES.lock().unwrap_or_else(PoisonError::into_inner);
    let changes = all.entry(session(cx)).or_default();
    let mark = MARK.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    changes.push(Change { path: path.into(), before: before.map(Into::into), after: after.map(Into::into), mark });
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
