//! The agent block's host of the shared media viewer (`crate::media_viewer`, `docs/MEDIA.md` §5.3, §6.4): the
//! session's generated images and videos in transcript order, their media from the pane's cache (fetched on
//! first ask), opened from a tile over the whole pane, composer included. Closed with nothing to focus back,
//! the composer takes focus.

use std::rc::Rc;

use gpui::{App, AppContext as _, Context, WeakEntity, Window};
use otto_proto::MediaKind;

use super::{
    AgentPane,
    media::{Entry, Media, Tiles, cache_file},
    transcript::Row,
};
use crate::media_viewer::{Item, Kind, MediaViewer, Source, ViewerEvent};

/// One image or video of the session, with what made it.
pub(super) struct Shown {
    pub media: Media,
    pub prompt: String,
    pub credit: Option<String>,
}

/// A generated file as the viewer's item: its line's metadata, the call's prompt and credit, and the folder
/// next to its cache file for copies named as made.
pub(super) fn item(shown: &Shown, session: Option<&str>) -> Item {
    let m = &shown.media;
    Item {
        id: m.id.clone(),
        key: m.id.clone(),
        name: m.name.clone(),
        title: None,
        kind: if m.kind() == MediaKind::Video { Kind::Video } else { Kind::Image },
        mime: m.mime.clone(),
        size: Some(m.size.clone()),
        dims: m.dims,
        ms: m.ms,
        credit: shown.credit.clone(),
        prompt: Some(shown.prompt.clone()),
        scratch: session.map(|s| cache_file(&crate::cache::root(), s, &m.id, &m.name).with_extension("")),
    }
}

/// The viewer's view of the pane.
struct Host(WeakEntity<AgentPane>);

impl Source for Host {
    fn items(&self, cx: &App) -> Vec<Item> {
        self.0.upgrade().map(|pane| pane.read(cx).viewer_items()).unwrap_or_default()
    }

    fn media(&self, item: &Item, cx: &mut App) -> Entry {
        let m = Media {
            id: item.id.clone(),
            name: item.name.clone(),
            mime: item.mime.clone(),
            size: item.size.clone().unwrap_or_default(),
            dims: item.dims,
            ms: item.ms,
        };
        self.0.update(cx, |pane, cx| pane.media(&m, cx)).unwrap_or(Entry::Failed)
    }
}

impl AgentPane {
    /// Every generated image and video of the session, in transcript order.
    pub(super) fn generated(&self) -> Vec<Shown> {
        let mut out = Vec::new();
        for row in &self.rows {
            if let Row::Media(s) = row
                && let Tiles::Ready(media) = &s.tiles
            {
                let shown = media.iter().filter(|m| m.kind() != MediaKind::Audio);
                out.extend(shown.map(|m| Shown { media: m.clone(), prompt: s.prompt.clone(), credit: s.credit.clone() }));
            }
        }
        out
    }

    /// What the viewer steps through.
    fn viewer_items(&self) -> Vec<Item> {
        let session = self.session_id.as_deref();
        self.generated().iter().map(|s| item(s, session)).collect()
    }

    /// Open the viewer on attachment `id`; it takes key focus.
    pub(super) fn open_viewer(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let source: Rc<dyn Source> = Rc::new(Host(cx.entity().downgrade()));
        let viewer = cx.new(|cx| MediaViewer::new(source, id, window, cx));
        let sub = cx.subscribe_in(&viewer, window, |this, viewer, e: &ViewerEvent, window, cx| match e {
            ViewerEvent::Closed { refocus: true } => this.focus_composer(window, cx),
            ViewerEvent::Gone if this.viewer.as_ref().is_some_and(|(v, _)| v == viewer) => {
                this.viewer = None;
                cx.notify();
            }
            _ => {}
        });
        self.viewer = Some((viewer, sub));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_file_is_an_item_with_its_call() {
        let media = Media {
            id: "att_1".into(),
            name: "fox.mp4".into(),
            mime: "video/mp4".into(),
            size: "4.2 MB".into(),
            dims: Some((1280, 720)),
            ms: Some(8000),
        };
        let shown = Shown { media, prompt: "A fox running".into(), credit: Some("xAI grok-imagine".into()) };
        let it = item(&shown, Some("ses_1"));
        assert_eq!((it.id.as_str(), it.key.as_str(), it.kind), ("att_1", "att_1", Kind::Video));
        assert_eq!((it.dims, it.ms, it.size.as_deref()), (Some((1280, 720)), Some(8000), Some("4.2 MB")));
        assert_eq!((it.credit.as_deref(), it.prompt.as_deref()), (Some("xAI grok-imagine"), Some("A fox running")));
        assert!(it.scratch.is_some_and(|d| d.ends_with("media/ses_1/att_1")), "next to its cache file");
        assert_eq!(item(&shown, None).scratch, None, "no session: no copies");
        let png = Shown { media: Media { mime: "image/png".into(), ..shown.media.clone() }, prompt: String::new(), credit: None };
        assert_eq!(item(&png, None).kind, Kind::Image);
    }
}
