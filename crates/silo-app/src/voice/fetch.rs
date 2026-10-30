//! Voice-model downloads, one per engine for the whole app: started from
//! Settings › Voice or by the first dictation, followed by both.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{LazyLock, Mutex},
};

use silo_voice::EngineKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    Running { done: u64, total: Option<u64> },
    Failed(String),
}

static FETCHES: LazyLock<Mutex<HashMap<EngineKind, Fetch>>> = LazyLock::new(Mutex::default);

fn put(kind: EngineKind, fetch: Option<Fetch>) {
    if let Ok(mut all) = FETCHES.lock() {
        match fetch {
            Some(f) => all.insert(kind, f),
            None => all.remove(&kind),
        };
    }
}

/// `kind`'s download now, if one runs or failed.
#[must_use]
pub fn fetch(kind: EngineKind) -> Option<Fetch> {
    FETCHES.lock().ok()?.get(&kind).cloned()
}

/// Any download running.
#[must_use]
pub fn busy() -> bool {
    FETCHES.lock().is_ok_and(|all| all.values().any(|f| matches!(f, Fetch::Running { .. })))
}

/// Download `kind`'s model unless it is here or already coming.
pub fn start(home: PathBuf, kind: EngineKind, rt: &tokio::runtime::Handle) {
    let model = kind.model();
    if model.installed(&home) || matches!(fetch(kind), Some(Fetch::Running { .. })) {
        return;
    }
    put(kind, Some(Fetch::Running { done: 0, total: Some(model.bytes) }));
    rt.spawn(async move {
        let result = model.download(&home, move |done, total| put(kind, Some(Fetch::Running { done, total: total.or(Some(model.bytes)) }))).await;
        put(kind, result.err().map(Fetch::Failed));
    });
}

/// "42%" of a running download.
#[must_use]
pub fn percent(done: u64, total: Option<u64>) -> Option<u64> {
    total.filter(|t| *t > 0).map(|t| (done.min(t) * 100) / t)
}
