//! A page's context menu: what was clicked (CEF's `ContextMenuParams`) and
//! the engine commands the app can answer with.

use cef::*;

/// What the page's context menu opened on, at `(x, y)` in view points.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ContextMenu {
    pub x: i32,
    pub y: i32,
    /// The link under the pointer.
    pub link: Option<String>,
    /// The link's text, whitespace collapsed (empty: none).
    pub link_text: String,
    /// The image, video, audio or canvas under the pointer.
    pub media: Option<Media>,
    /// The selected text (empty: none).
    pub selection: String,
    /// A text field or contenteditable.
    pub editable: bool,
    pub edit: EditState,
    /// The misspelled word under the pointer and the spellchecker's
    /// replacements (at most [`MenuCommand::SUGGESTIONS`]).
    pub misspelled: Option<String>,
    pub suggestions: Vec<String>,
    /// The top-level page's URL.
    pub page_url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Canvas,
}

/// A media element the menu opened on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Media {
    pub kind: MediaKind,
    /// Its source URL (empty for a canvas or a stream).
    pub src: String,
    pub state: MediaState,
}

/// The element's state as the menu opened, from CEF's media flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct MediaState {
    pub paused: bool,
    pub muted: bool,
    pub looping: bool,
    pub controls: bool,
    pub has_audio: bool,
    pub can_save: bool,
    pub can_toggle_controls: bool,
    pub can_loop: bool,
    pub error: bool,
    /// An image with its pixels loaded (Copy Image works).
    pub has_image: bool,
}

/// What a media row does to the element the menu opened on
/// ([`crate::Browser::media`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaAction {
    PlayPause,
    Mute,
    Loop,
    Controls,
}

impl MediaAction {
    /// The statement run on `m`, the clicked media element.
    pub(crate) fn script(self) -> &'static str {
        match self {
            Self::PlayPause => "if (m.paused) m.play().catch(() => {}); else m.pause();",
            Self::Mute => "m.muted = !m.muted;",
            Self::Loop => "m.loop = !m.loop;",
            Self::Controls => "m.controls = !m.controls;",
        }
    }
}

/// Which edit commands the clicked element can run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct EditState {
    pub undo: bool,
    pub redo: bool,
    pub cut: bool,
    pub copy: bool,
    pub paste: bool,
    pub delete: bool,
    pub select_all: bool,
}

/// Engine commands of a context menu, run with its frame and facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    PasteMatchStyle,
    Delete,
    SelectAll,
    /// Replace the misspelled word with suggestion `n`.
    Suggestion(usize),
    AddToDictionary,
}

impl MenuCommand {
    /// CEF's spelling suggestion ids (`MENU_ID_SPELLCHECK_SUGGESTION_0..=4`).
    pub const SUGGESTIONS: usize = 5;

    pub(crate) fn id(self) -> i32 {
        use sys::cef_menu_id_t as Id;
        let id = match self {
            Self::Undo => Id::MENU_ID_UNDO,
            Self::Redo => Id::MENU_ID_REDO,
            Self::Cut => Id::MENU_ID_CUT,
            Self::Copy => Id::MENU_ID_COPY,
            Self::Paste => Id::MENU_ID_PASTE,
            Self::PasteMatchStyle => Id::MENU_ID_PASTE_MATCH_STYLE,
            Self::Delete => Id::MENU_ID_DELETE,
            Self::SelectAll => Id::MENU_ID_SELECT_ALL,
            Self::AddToDictionary => Id::MENU_ID_ADD_TO_DICTIONARY,
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            Self::Suggestion(n) => return Id::MENU_ID_SPELLCHECK_SUGGESTION_0 as i32 + n.min(Self::SUGGESTIONS - 1) as i32,
        };
        id as i32
    }
}

fn text(value: &CefStringUserfree) -> String {
    CefString::from(value).to_string()
}

impl ContextMenu {
    /// `link_text`: what the page's menu script reported for this click.
    pub(crate) fn from_params(params: &ContextMenuParams, link_text: String) -> Self {
        let types = sys::cef_context_menu_type_flags_t::from(params.type_flags()).0;
        let has = |flag: sys::cef_context_menu_type_flags_t| types & flag.0 != 0;
        let edit = sys::cef_context_menu_edit_state_flags_t::from(params.edit_state_flags()).0;
        let can = |flag: sys::cef_context_menu_edit_state_flags_t| edit & flag.0 != 0;
        use sys::{cef_context_menu_edit_state_flags_t as E, cef_context_menu_type_flags_t as T};
        let link = Some(text(&params.link_url())).filter(|url| has(T::CM_TYPEFLAG_LINK) && !url.is_empty());
        let mut list = CefStringList::new();
        params.dictionary_suggestions(Some(&mut list));
        let misspelled = Some(text(&params.misspelled_word())).filter(|word| !word.is_empty());
        Self {
            x: params.xcoord(),
            y: params.ycoord(),
            link_text: if link.is_some() { link_text } else { String::new() },
            link,
            media: media(params),
            selection: if has(T::CM_TYPEFLAG_SELECTION) { text(&params.selection_text()) } else { String::new() },
            editable: params.is_editable() != 0,
            edit: EditState {
                undo: can(E::CM_EDITFLAG_CAN_UNDO),
                redo: can(E::CM_EDITFLAG_CAN_REDO),
                cut: can(E::CM_EDITFLAG_CAN_CUT),
                copy: can(E::CM_EDITFLAG_CAN_COPY),
                paste: can(E::CM_EDITFLAG_CAN_PASTE),
                delete: can(E::CM_EDITFLAG_CAN_DELETE),
                select_all: can(E::CM_EDITFLAG_CAN_SELECT_ALL),
            },
            suggestions: if misspelled.is_some() { list.into_iter().take(MenuCommand::SUGGESTIONS).collect() } else { Vec::new() },
            misspelled,
            page_url: text(&params.page_url()),
        }
    }
}

fn media(params: &ContextMenuParams) -> Option<Media> {
    let kind = match params.media_type() {
        ContextMenuMediaType::IMAGE => MediaKind::Image,
        ContextMenuMediaType::VIDEO => MediaKind::Video,
        ContextMenuMediaType::AUDIO => MediaKind::Audio,
        ContextMenuMediaType::CANVAS => MediaKind::Canvas,
        _ => return None,
    };
    let flags = sys::cef_context_menu_media_state_flags_t::from(params.media_state_flags()).0;
    use sys::cef_context_menu_media_state_flags_t as M;
    let on = |flag: M| flags & flag.0 != 0;
    let src = text(&params.source_url());
    if kind == MediaKind::Image && src.is_empty() {
        return None;
    }
    let state = MediaState {
        paused: on(M::CM_MEDIAFLAG_PAUSED),
        muted: on(M::CM_MEDIAFLAG_MUTED),
        looping: on(M::CM_MEDIAFLAG_LOOP),
        controls: on(M::CM_MEDIAFLAG_CONTROLS),
        has_audio: on(M::CM_MEDIAFLAG_HAS_AUDIO),
        can_save: on(M::CM_MEDIAFLAG_CAN_SAVE),
        can_toggle_controls: on(M::CM_MEDIAFLAG_CAN_TOGGLE_CONTROLS),
        can_loop: on(M::CM_MEDIAFLAG_CAN_LOOP),
        error: on(M::CM_MEDIAFLAG_IN_ERROR),
        has_image: params.has_image_contents() != 0,
    };
    Some(Media { kind, src, state })
}

/// Run in every frame on load (`__KEY__` replaced by the browser's menu tag
/// as a JS string): on each `contextmenu` it keeps the clicked node for
/// media actions (`window[key]()`) and reports the link's text, which
/// CEF's menu facts lack. The report reaches the display handler before
/// CEF's menu (both go over the frame's host interface, in order).
pub(crate) const MENU_SCRIPT: &str = r"(() => {
const key = __KEY__;
if (window[key]) return;
const log = console.log.bind(console);
let target = null;
Object.defineProperty(window, key, { value: () => target });
addEventListener('contextmenu', e => {
  target = e.composedPath()[0] || e.target;
  const el = target instanceof Element ? target : target && target.parentElement;
  const a = el && el.closest('a[href], area[href]');
  const text = a ? (a.innerText || a.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 1000) : '';
  log(key + text);
}, true);
})()";

/// `action` on the media element of the last context menu, run in its frame.
pub(crate) fn media_script(tag: &str, action: MediaAction) -> String {
    let key = serde_json::to_string(tag).unwrap_or_default();
    format!("(() => {{ const f = window[{key}]; const m = f && f(); if (!(m instanceof HTMLMediaElement)) return; {} }})()", action.script())
}

pub(crate) fn menu_script(tag: &str) -> String {
    MENU_SCRIPT.replace("__KEY__", &serde_json::to_string(tag).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_cef_menu_ids() {
        assert_eq!(MenuCommand::Undo.id(), 110);
        assert_eq!(MenuCommand::Copy.id(), 113);
        assert_eq!(MenuCommand::PasteMatchStyle.id(), 115);
        assert_eq!(MenuCommand::Delete.id(), 116);
        assert_eq!(MenuCommand::SelectAll.id(), 117);
        assert_eq!(MenuCommand::Suggestion(0).id(), 200);
        assert_eq!(MenuCommand::Suggestion(4).id(), 204);
        assert_eq!(MenuCommand::Suggestion(9).id(), 204, "only five suggestion ids exist");
        assert_eq!(MenuCommand::AddToDictionary.id(), 206);
    }

    #[test]
    fn scripts_quote_the_tag() {
        let tag = "\u{1}silo-menu-1:";
        let script = menu_script(tag);
        assert!(script.starts_with("(() => {\nconst key = \"\\u0001silo-menu-1:\";"), "{script}");
        let media = media_script(tag, MediaAction::Loop);
        assert!(media.contains("window[\"\\u0001silo-menu-1:\"]") && media.ends_with("m.loop = !m.loop; })()"), "{media}");
    }
}
