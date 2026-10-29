//! The page's context menu, by what was clicked, grouped like Chrome's: a
//! misspelled word, a link, an image or a video, a text field, selected
//! text, else the page itself; Inspect closes every menu.

use silo_web::{ContextMenu, MediaAction, MediaKind, MenuCommand};

use crate::context_menu::Item;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// A new tab behind this one, like Chrome's.
    OpenTab(String),
    /// A browser block beside this pane.
    OpenSplit(String),
    Copy(String),
    CopyImage(String),
    /// Download the URL to where the user picks.
    SaveAs(String),
    SavePage,
    /// A search for the text on Settings › Browser's engine, in front.
    Search(String),
    /// The system's definition of the text, at the menu's point.
    LookUp(String),
    Media(MediaAction),
    Back,
    Forward,
    Reload,
    Print,
    ViewSource,
    /// DevTools on the element at the menu's point.
    Inspect,
    /// Run by the engine on the element the menu opened on.
    Engine(MenuCommand),
}

/// Longest selection quoted in "Search for …" and "Look Up …".
const QUOTE: usize = 24;

fn quote(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(QUOTE) {
        Some((at, _)) => format!("“{}…”", text[..at].trim_end()),
        None => format!("“{text}”"),
    }
}

/// Whether `url` can be opened, copied or saved as an address (not a
/// `blob:` stream or nothing).
fn addressable(url: &str) -> bool {
    !url.is_empty() && !url.starts_with("blob:")
}

/// `view-source:` of `page`, when a block may load it.
pub(crate) fn source_of(page: &str) -> Option<String> {
    silo_proto::model::browser_url(&format!("view-source:{page}"))
}

/// The rows for `menu`; `back` / `forward`: the block's history steps that way.
pub(crate) fn items(menu: &ContextMenu, engine: &str, back: bool, forward: bool) -> Vec<Item<Action>> {
    let mut items = Vec::new();
    let row = |label: &str, keys: &'static str, action: Action| Item::action(label.to_owned(), keys, action);
    if menu.misspelled.is_some() {
        for (n, word) in menu.suggestions.iter().enumerate() {
            items.push(Item::action(word.clone(), "", Action::Engine(MenuCommand::Suggestion(n))));
        }
        if menu.suggestions.is_empty() {
            items.push(row("No Guesses Found", "", Action::Engine(MenuCommand::Suggestion(0))).enabled(false));
        }
        items.push(row("Add to Dictionary", "", Action::Engine(MenuCommand::AddToDictionary)));
        items.push(Item::Separator);
    }
    if let Some(link) = &menu.link {
        items.push(row("Open Link in New Tab", "", Action::OpenTab(link.clone())));
        items.push(row("Open Link in Split", "", Action::OpenSplit(link.clone())));
        items.push(Item::Separator);
        items.push(row("Save Link As…", "", Action::SaveAs(link.clone())));
        items.push(row("Copy Link Address", "", Action::Copy(link.clone())));
        if !menu.link_text.is_empty() {
            items.push(row("Copy Link Text", "", Action::Copy(menu.link_text.clone())));
        }
        items.push(Item::Separator);
    }
    if let Some(media) = &menu.media {
        let (src, state) = (&media.src, media.state);
        match media.kind {
            MediaKind::Image => {
                items.push(row("Open Image in New Tab", "", Action::OpenTab(src.clone())));
                items.push(row("Save Image As…", "", Action::SaveAs(src.clone())));
                items.push(row("Copy Image", "", Action::CopyImage(src.clone())).enabled(state.has_image));
                items.push(row("Copy Image Address", "", Action::Copy(src.clone())));
            }
            MediaKind::Video | MediaKind::Audio => {
                let noun = if media.kind == MediaKind::Video { "Video" } else { "Audio" };
                let playable = !state.error;
                items.push(row(if state.paused { "Play" } else { "Pause" }, "", Action::Media(MediaAction::PlayPause)).enabled(playable));
                let audible = playable && (media.kind == MediaKind::Audio || state.has_audio);
                items.push(row(if state.muted { "Unmute" } else { "Mute" }, "", Action::Media(MediaAction::Mute)).enabled(audible));
                items.push(row("Loop", "", Action::Media(MediaAction::Loop)).checked(state.looping).enabled(playable));
                if state.can_toggle_controls {
                    items.push(row("Show Controls", "", Action::Media(MediaAction::Controls)).checked(state.controls));
                }
                if addressable(src) {
                    items.push(Item::Separator);
                    items.push(row(&format!("Open {noun} in New Tab"), "", Action::OpenTab(src.clone())));
                    items.push(row(&format!("Save {noun} As…"), "", Action::SaveAs(src.clone())).enabled(state.can_save));
                    items.push(row(&format!("Copy {noun} Address"), "", Action::Copy(src.clone())));
                }
            }
            MediaKind::Canvas => {}
        }
        items.push(Item::Separator);
    }
    let selected = !menu.selection.trim().is_empty();
    let edit = menu.edit;
    let command = |label: &str, keys: &'static str, command: MenuCommand, on: bool| row(label, keys, Action::Engine(command)).enabled(on);
    if menu.editable {
        items.push(command("Undo", "⌘Z", MenuCommand::Undo, edit.undo));
        items.push(command("Redo", "⇧⌘Z", MenuCommand::Redo, edit.redo));
        items.push(Item::Separator);
        items.push(command("Cut", "⌘X", MenuCommand::Cut, edit.cut && selected));
        items.push(command("Copy", "⌘C", MenuCommand::Copy, edit.copy && selected));
        items.push(command("Paste", "⌘V", MenuCommand::Paste, edit.paste));
        items.push(command("Paste and Match Style", "", MenuCommand::PasteMatchStyle, edit.paste));
        items.push(command("Delete", "", MenuCommand::Delete, edit.delete && selected));
        items.push(command("Select All", "⌘A", MenuCommand::SelectAll, edit.select_all));
    } else if selected {
        items.push(row("Copy", "⌘C", Action::Engine(MenuCommand::Copy)));
    }
    if selected {
        let text = menu.selection.trim().to_owned();
        items.push(Item::Separator);
        items.push(row(&format!("Search {engine} for {}", quote(&text)), "", Action::Search(text.clone())));
        items.push(row(&format!("Look Up {}", quote(&text)), "", Action::LookUp(text)));
    }
    let source = source_of(&menu.page_url).is_some();
    if menu.link.is_none() && menu.media.as_ref().is_none_or(|m| m.kind == MediaKind::Canvas) && !menu.editable && !selected {
        items.push(row("Back", "⌘←", Action::Back).enabled(back));
        items.push(row("Forward", "⌘→", Action::Forward).enabled(forward));
        items.push(row("Reload", "⌘R", Action::Reload));
        items.push(Item::Separator);
        items.push(row("Save Page As…", "⌘S", Action::SavePage).enabled(source));
        items.push(row("Print…", "", Action::Print));
        items.push(Item::Separator);
        items.push(row("View Page Source", "⌥⌘U", Action::ViewSource).enabled(source));
    } else if selected && !menu.editable {
        items.push(Item::Separator);
        items.push(row("Print…", "", Action::Print));
    }
    items.push(Item::Separator);
    items.push(row("Inspect", "⌥⌘I", Action::Inspect));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_web::{Media, MediaState};

    /// Each row's label, whether it is enabled and its check (`---`: a separator).
    fn rows(menu: &ContextMenu, back: bool) -> Vec<(String, bool, Option<bool>)> {
        let menu = crate::context_menu::Menu::new(gpui::point(gpui::px(0.0), gpui::px(0.0)), items(menu, "Google", back, false));
        menu.items()
            .iter()
            .map(|item| match item {
                Item::Action { label, enabled, check, .. } => (label.to_string(), *enabled, *check),
                Item::Separator => ("---".into(), true, None),
            })
            .collect()
    }

    fn labels(menu: &ContextMenu) -> Vec<String> {
        rows(menu, true).into_iter().map(|(label, ..)| label).collect()
    }

    fn actions(menu: &ContextMenu) -> Vec<Action> {
        items(menu, "Google", false, false)
            .into_iter()
            .filter_map(|i| match i {
                Item::Action { action, .. } => Some(action),
                Item::Separator => None,
            })
            .collect()
    }

    fn page() -> ContextMenu {
        ContextMenu { page_url: "https://a.test/p".into(), ..ContextMenu::default() }
    }

    #[test]
    fn the_page_gets_navigation_saving_and_tools() {
        assert_eq!(labels(&page()), ["Back", "Forward", "Reload", "---", "Save Page As…", "Print…", "---", "View Page Source", "---", "Inspect"]);
        let rows = rows(&page(), false);
        assert!(!rows[0].1 && !rows[1].1, "no history either way: Back and Forward are off");
        assert!(rows[4].1 && rows[7].1);
        let blank = rows_of(ContextMenu { page_url: "about:blank".into(), ..ContextMenu::default() });
        assert!(!blank["Save Page As…"] && !blank["View Page Source"], "no source to show or save");
        assert!(actions(&page()).contains(&Action::ViewSource));
        assert_eq!(source_of("https://a.test/p").as_deref(), Some("view-source:https://a.test/p"));
        assert_eq!(source_of("data:text/html,x"), None);
    }

    fn rows_of(menu: ContextMenu) -> std::collections::HashMap<String, bool> {
        rows(&menu, true).into_iter().map(|(label, on, _)| (label, on)).collect()
    }

    #[test]
    fn links_open_save_and_copy() {
        let link = ContextMenu { link: Some("https://a.test/x".into()), link_text: "Docs".into(), ..page() };
        assert_eq!(
            labels(&link),
            ["Open Link in New Tab", "Open Link in Split", "---", "Save Link As…", "Copy Link Address", "Copy Link Text", "---", "Inspect"]
        );
        let url = "https://a.test/x".to_owned();
        assert_eq!(
            actions(&link)[..5],
            [
                Action::OpenTab(url.clone()),
                Action::OpenSplit(url.clone()),
                Action::SaveAs(url.clone()),
                Action::Copy(url),
                Action::Copy("Docs".into())
            ]
        );
        let bare = ContextMenu { link_text: String::new(), ..link };
        assert!(!labels(&bare).contains(&"Copy Link Text".to_owned()), "no text: no Copy Link Text");
    }

    #[test]
    fn images_open_save_and_copy() {
        let src = "https://a.test/i.png".to_owned();
        let state = MediaState { has_image: true, ..MediaState::default() };
        let image = ContextMenu { media: Some(Media { kind: MediaKind::Image, src: src.clone(), state }), ..page() };
        assert_eq!(labels(&image), ["Open Image in New Tab", "Save Image As…", "Copy Image", "Copy Image Address", "---", "Inspect"]);
        assert_eq!(
            actions(&image)[..4],
            [Action::OpenTab(src.clone()), Action::SaveAs(src.clone()), Action::CopyImage(src.clone()), Action::Copy(src.clone())]
        );
        let broken = ContextMenu { media: Some(Media { kind: MediaKind::Image, src, state: MediaState::default() }), ..page() };
        assert!(!rows_of(broken)["Copy Image"], "no pixels: nothing to copy");
        let linked = ContextMenu { link: Some("https://a.test/x".into()), ..image };
        assert_eq!(labels(&linked)[..4], ["Open Link in New Tab", "Open Link in Split", "---", "Save Link As…"], "the link first, as in Chrome");
        assert!(labels(&linked).contains(&"Copy Image".to_owned()));
    }

    #[test]
    fn videos_toggle_and_save() {
        let state = MediaState {
            paused: true,
            has_audio: true,
            looping: true,
            controls: false,
            can_toggle_controls: true,
            can_save: true,
            ..MediaState::default()
        };
        let video = ContextMenu { media: Some(Media { kind: MediaKind::Video, src: "https://a.test/v.mp4".into(), state }), ..page() };
        assert_eq!(
            rows(&video, true),
            [
                ("Play".into(), true, None),
                ("Mute".into(), true, None),
                ("Loop".into(), true, Some(true)),
                ("Show Controls".into(), true, Some(false)),
                ("---".into(), true, None),
                ("Open Video in New Tab".into(), true, None),
                ("Save Video As…".into(), true, None),
                ("Copy Video Address".into(), true, None),
                ("---".into(), true, None),
                ("Inspect".into(), true, None),
            ]
        );
        assert_eq!(actions(&video)[..4], [MediaAction::PlayPause, MediaAction::Mute, MediaAction::Loop, MediaAction::Controls].map(Action::Media));
        let playing = MediaState { paused: false, muted: true, has_audio: false, ..state };
        let silent = ContextMenu { media: Some(Media { kind: MediaKind::Video, src: "blob:https://a.test/1".into(), state: playing }), ..page() };
        let rows = rows_of(silent);
        assert!(rows["Pause"] && !rows["Unmute"], "no audio track: Mute is off");
        assert!(!rows.contains_key("Save Video As…") && !rows.contains_key("Copy Video Address"), "a stream has no address");
        let audio =
            ContextMenu { media: Some(Media { kind: MediaKind::Audio, src: "https://a.test/a.mp3".into(), state: MediaState::default() }), ..page() };
        let labels = labels(&audio);
        assert!(labels.contains(&"Mute".to_owned()) && labels.contains(&"Save Audio As…".to_owned()));
        assert!(!labels.contains(&"Show Controls".to_owned()), "controls only where the element can toggle them");
    }

    #[test]
    fn selected_text_copies_searches_and_looks_up() {
        let text = ContextMenu { selection: "  silo   browser  tabs ".into(), ..page() };
        assert_eq!(
            labels(&text),
            ["Copy", "---", "Search Google for “silo browser tabs”", "Look Up “silo browser tabs”", "---", "Print…", "---", "Inspect"]
        );
        assert!(actions(&text).contains(&Action::LookUp("silo   browser  tabs".into())));
        let long = ContextMenu { selection: "a very long selection of words to search".into(), ..page() };
        assert_eq!(labels(&long)[2], "Search Google for “a very long selection of…”");
    }

    #[test]
    fn fields_edit_by_what_they_allow() {
        let edit = silo_web::EditState { paste: true, select_all: true, copy: true, cut: true, delete: true, ..Default::default() };
        let field = ContextMenu { editable: true, edit, ..page() };
        let row = |label: &str, on: bool| (label.to_owned(), on, None);
        assert_eq!(
            rows(&field, true),
            [
                row("Undo", false),
                row("Redo", false),
                row("---", true),
                row("Cut", false),
                row("Copy", false),
                row("Paste", true),
                row("Paste and Match Style", true),
                row("Delete", false),
                row("Select All", true),
                row("---", true),
                row("Inspect", true),
            ],
            "nothing selected: no Cut, Copy or Delete"
        );
        let typo = ContextMenu {
            selection: "wrold".into(),
            misspelled: Some("wrold".into()),
            suggestions: vec!["world".into(), "wold".into()],
            ..field.clone()
        };
        let labels = labels(&typo);
        assert_eq!(labels[..4], ["world", "wold", "Add to Dictionary", "---"]);
        assert!(labels.contains(&"Search Google for “wrold”".to_owned()));
        assert!(!labels.contains(&"Print…".to_owned()), "a field's selection isn't printed");
        assert!(rows_of(typo)["Delete"]);
        let none = ContextMenu { misspelled: Some("qqq".into()), ..field };
        assert_eq!(rows(&none, true)[0], ("No Guesses Found".into(), false, None));
    }
}
