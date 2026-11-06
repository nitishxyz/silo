//! The catalog the tools and `@` handles see (docs/MEDIA.md §3.2): each provider's built-in models, then its live
//! catalog's families. A built-in model whose id is one of a family's endpoints is that family (its handle stays).
use crate::endpoints::{Endpoint, families};
use crate::{MediaKind, MediaProvider, ModelEntry};
use otto_proto::MediaModel;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct Model {
    pub provider: Arc<dyn MediaProvider>,
    /// The built-in model's id, else the family's key.
    pub id: String,
    pub name: String,
    pub about: String,
    pub kind: MediaKind,
    pub default: bool,
    pub handle: String,
    pub entry: Option<ModelEntry>,
    /// Live endpoints by mode; none for a built-in model without them (it runs through `generate_<kind>`).
    pub modes: Vec<(String, Endpoint)>,
}

impl Model {
    #[must_use]
    pub fn wire(&self) -> MediaModel {
        MediaModel { id: self.id.clone(), kind: self.kind, name: self.name.clone(), handle: self.handle.clone(), default: self.default }
    }
    /// Whether `name` (any case) names it: its handle, id, or one of its endpoint ids.
    pub fn named(&self, name: &str) -> bool {
        self.handle.eq_ignore_ascii_case(name)
            || self.id.eq_ignore_ascii_case(name)
            || self.modes.iter().any(|(_, e)| e.id.eq_ignore_ascii_case(name))
    }
    pub fn price(&self) -> Option<&str> {
        self.entry.as_ref().and_then(|e| e.price.as_deref()).or_else(|| self.modes.iter().find_map(|(_, e)| e.price.as_deref()))
    }
}

/// Every provider's models: built-in first, then live families none of them is; handles over the built-in ones
/// first so live entries never change them.
pub(crate) fn build(providers: &[Arc<dyn MediaProvider>], live: &dyn Fn(&str) -> Vec<Endpoint>) -> Vec<Model> {
    let mut built: Vec<Model> = Vec::new();
    let mut extra: Vec<Model> = Vec::new();
    for provider in providers {
        let fams = families(&live(provider.id()));
        let mut used = vec![false; fams.len()];
        for entry in provider.models() {
            let found = fams.iter().position(|f| f.kind == entry.kind && f.modes.iter().any(|(_, e)| e.id == entry.id));
            let modes = found.map(|i| {
                used[i] = true;
                fams[i].modes.clone()
            });
            built.push(Model {
                provider: provider.clone(),
                id: entry.id.clone(),
                name: entry.name.clone(),
                about: entry.about.clone(),
                kind: entry.kind,
                default: entry.default,
                handle: String::new(),
                modes: modes.unwrap_or_default(),
                entry: Some(entry),
            });
        }
        for (f, _) in fams.into_iter().zip(used).filter(|(_, used)| !used) {
            extra.push(Model {
                provider: provider.clone(),
                id: f.key,
                name: f.name,
                about: f.about,
                kind: f.kind,
                default: false,
                handle: String::new(),
                entry: None,
                modes: f.modes,
            });
        }
    }
    let names: Vec<(&str, &str)> = built.iter().chain(&extra).map(|m| (m.provider.id(), m.name.as_str())).collect();
    let handles = crate::catalog::handles(&names);
    let mut all: Vec<Model> = built.into_iter().chain(extra).zip(handles).map(|(m, handle)| Model { handle, ..m }).collect();
    // Providers in order, each's built-in models first.
    let order = |m: &Model| providers.iter().position(|p| p.id() == m.provider.id()).unwrap_or(usize::MAX);
    all.sort_by_key(|m| (order(m), m.entry.is_none()));
    all
}
