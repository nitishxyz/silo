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
    /// The image under the pointer (its source).
    pub image: Option<String>,
    /// The selected text (empty: none).
    pub selection: String,
    /// A text field or contenteditable.
    pub editable: bool,
    pub edit: EditState,
    /// The misspelled word under the pointer and the spellchecker's
    /// replacements (at most [`MenuCommand::SUGGESTIONS`]).
    pub misspelled: Option<String>,
    pub suggestions: Vec<String>,
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
    pub(crate) fn from_params(params: &ContextMenuParams) -> Self {
        let types = sys::cef_context_menu_type_flags_t::from(params.type_flags()).0;
        let has = |flag: sys::cef_context_menu_type_flags_t| types & flag.0 != 0;
        let edit = sys::cef_context_menu_edit_state_flags_t::from(params.edit_state_flags()).0;
        let can = |flag: sys::cef_context_menu_edit_state_flags_t| edit & flag.0 != 0;
        use sys::{cef_context_menu_edit_state_flags_t as E, cef_context_menu_type_flags_t as T};
        let link = Some(text(&params.link_url())).filter(|url| has(T::CM_TYPEFLAG_LINK) && !url.is_empty());
        let image = params.media_type() == ContextMenuMediaType::IMAGE && params.has_image_contents() != 0;
        let image = Some(text(&params.source_url())).filter(|url| image && !url.is_empty());
        let mut list = CefStringList::new();
        params.dictionary_suggestions(Some(&mut list));
        let misspelled = Some(text(&params.misspelled_word())).filter(|word| !word.is_empty());
        Self {
            x: params.xcoord(),
            y: params.ycoord(),
            link,
            image,
            selection: if has(T::CM_TYPEFLAG_SELECTION) { text(&params.selection_text()) } else { String::new() },
            editable: params.is_editable() != 0,
            edit: EditState {
                undo: can(E::CM_EDITFLAG_CAN_UNDO),
                redo: can(E::CM_EDITFLAG_CAN_REDO),
                cut: can(E::CM_EDITFLAG_CAN_CUT),
                copy: can(E::CM_EDITFLAG_CAN_COPY),
                paste: can(E::CM_EDITFLAG_CAN_PASTE),
                select_all: can(E::CM_EDITFLAG_CAN_SELECT_ALL),
            },
            suggestions: if misspelled.is_some() { list.into_iter().take(MenuCommand::SUGGESTIONS).collect() } else { Vec::new() },
            misspelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MenuCommand;

    #[test]
    fn commands_are_cef_menu_ids() {
        assert_eq!(MenuCommand::Undo.id(), 110);
        assert_eq!(MenuCommand::Copy.id(), 113);
        assert_eq!(MenuCommand::SelectAll.id(), 117);
        assert_eq!(MenuCommand::Suggestion(0).id(), 200);
        assert_eq!(MenuCommand::Suggestion(4).id(), 204);
        assert_eq!(MenuCommand::Suggestion(9).id(), 204, "only five suggestion ids exist");
        assert_eq!(MenuCommand::AddToDictionary.id(), 206);
    }
}
