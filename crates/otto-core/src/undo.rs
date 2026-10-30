//! `undo_edit`'s history in the store (`Store::undo_push`, capped there), so it outlives the daemon.
use crate::Runtime;
use otto_tools::{
    ToolError,
    builtin::undo_edit::{Kept, UndoLog},
};
use std::path::Path;

pub(crate) struct Undo(pub Runtime);

fn failed(error: crate::Error) -> ToolError {
    ToolError::failed(format!("undo history: {error}"))
}

#[async_trait::async_trait]
impl UndoLog for Undo {
    async fn push(&self, session: &str, path: &Path, before: Option<&str>, after: Option<&str>) {
        let (id, path) = (session.to_owned(), path.to_string_lossy().into_owned());
        let (before, after) = (before.map(str::to_owned), after.map(str::to_owned));
        if let Err(error) = self.0.db(move |s| Ok(s.undo_push(&id, &path, before.as_deref(), after.as_deref())?)).await {
            tracing::warn!(%session, %error, "edit not kept for undo");
        }
    }
    async fn last(&self, session: &str, path: &Path) -> Result<Option<Kept>, ToolError> {
        let (id, path) = (session.to_owned(), path.to_string_lossy().into_owned());
        let kept = self.0.db(move |s| Ok(s.undo_last(&id, &path)?)).await.map_err(failed)?;
        Ok(kept.map(|k| Kept { id: k.id, before: k.before, after: k.after }))
    }
    async fn remove(&self, session: &str, id: i64) -> Result<(), ToolError> {
        let session = session.to_owned();
        self.0.db(move |s| Ok(s.undo_remove(&session, id)?)).await.map_err(failed)
    }
}
