//! The page's context menu, by what was clicked: a misspelled word, a
//! link, an image, a text field, selected text, or the page itself.

use silo_web::{ContextMenu, MenuCommand};

use crate::context_menu::Item;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// A new tab behind this one, like Chrome's.
    OpenTab(String),
    Copy(String),
    CopyImage(String),
    /// A search for the text on Settings › Browser's engine, in front.
    Search(String),
    Back,
    Forward,
    Reload,
    /// Run by the engine on the element the menu opened on.
    Engine(MenuCommand),
}

/// Longest selection quoted in "Search for …".
const QUOTE: usize = 24;

fn quote(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(QUOTE) {
        Some((at, _)) => format!("“{}…”", text[..at].trim_end()),
        None => format!("“{text}”"),
    }
}

/// The rows for `menu`; `back` / `forward`: the block's history steps that way.
pub(crate) fn items(menu: &ContextMenu, engine: &str, back: bool, forward: bool) -> Vec<Item<Action>> {
    let mut items = Vec::new();
    if menu.misspelled.is_some() {
        for (n, word) in menu.suggestions.iter().enumerate() {
            items.push(Item::action(word.clone(), "", Action::Engine(MenuCommand::Suggestion(n))));
        }
        if menu.suggestions.is_empty() {
            items.push(Item::action("No Guesses Found", "", Action::Engine(MenuCommand::Suggestion(0))).enabled(false));
        }
        items.push(Item::action("Add to Dictionary", "", Action::Engine(MenuCommand::AddToDictionary)));
        items.push(Item::Separator);
    }
    if let Some(link) = &menu.link {
        items.push(Item::action("Open Link in New Tab", "", Action::OpenTab(link.clone())));
        items.push(Item::action("Copy Link", "", Action::Copy(link.clone())));
        items.push(Item::Separator);
    }
    if let Some(image) = &menu.image {
        items.push(Item::action("Open Image in New Tab", "", Action::OpenTab(image.clone())));
        items.push(Item::action("Copy Image", "", Action::CopyImage(image.clone())));
        items.push(Item::action("Copy Image Address", "", Action::Copy(image.clone())));
        items.push(Item::Separator);
    }
    let selected = !menu.selection.trim().is_empty();
    let edit = menu.edit;
    if menu.editable {
        items.push(Item::action("Undo", "⌘Z", Action::Engine(MenuCommand::Undo)).enabled(edit.undo));
        items.push(Item::action("Redo", "⇧⌘Z", Action::Engine(MenuCommand::Redo)).enabled(edit.redo));
        items.push(Item::Separator);
        items.push(Item::action("Cut", "⌘X", Action::Engine(MenuCommand::Cut)).enabled(edit.cut && selected));
        items.push(Item::action("Copy", "⌘C", Action::Engine(MenuCommand::Copy)).enabled(edit.copy && selected));
        items.push(Item::action("Paste", "⌘V", Action::Engine(MenuCommand::Paste)).enabled(edit.paste));
        items.push(Item::action("Select All", "⌘A", Action::Engine(MenuCommand::SelectAll)).enabled(edit.select_all));
    } else if selected {
        items.push(Item::action("Copy", "⌘C", Action::Engine(MenuCommand::Copy)));
    }
    if selected {
        items.push(Item::Separator);
        items.push(Item::action(format!("Search {engine} for {}", quote(&menu.selection)), "", Action::Search(menu.selection.trim().to_owned())));
    }
    if menu.link.is_none() && menu.image.is_none() && !menu.editable && !selected {
        items.push(Item::action("Back", "⌘[", Action::Back).enabled(back));
        items.push(Item::action("Forward", "⌘]", Action::Forward).enabled(forward));
        items.push(Item::action("Reload", "⌘R", Action::Reload));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each row's label and whether it is enabled (`---`: a separator).
    fn rows(menu: &ContextMenu, back: bool) -> Vec<(String, bool)> {
        let menu = crate::context_menu::Menu::new(gpui::point(gpui::px(0.0), gpui::px(0.0)), items(menu, "Google", back, false));
        menu.items()
            .iter()
            .map(|item| match item {
                Item::Action { label, enabled, .. } => (label.to_string(), *enabled),
                Item::Separator => ("---".into(), true),
            })
            .collect()
    }

    fn labels(menu: &ContextMenu) -> Vec<String> {
        rows(menu, true).into_iter().map(|(label, _)| label).collect()
    }

    #[test]
    fn the_page_gets_navigation() {
        assert_eq!(rows(&ContextMenu::default(), true), [("Back".into(), true), ("Forward".into(), false), ("Reload".into(), true)]);
        assert!(!rows(&ContextMenu::default(), false)[0].1, "no history behind: Back is off");
    }

    #[test]
    fn links_and_images_open_in_new_tabs_and_copy() {
        let link = ContextMenu { link: Some("https://a.test/x".into()), ..ContextMenu::default() };
        assert_eq!(labels(&link), ["Open Link in New Tab", "Copy Link"]);
        let image = ContextMenu { image: Some("https://a.test/i.png".into()), ..link.clone() };
        assert_eq!(labels(&image), ["Open Link in New Tab", "Copy Link", "---", "Open Image in New Tab", "Copy Image", "Copy Image Address"]);
        let actions: Vec<Action> = items(&image, "Google", false, false)
            .into_iter()
            .filter_map(|i| match i {
                Item::Action { action, .. } => Some(action),
                Item::Separator => None,
            })
            .collect();
        assert_eq!(actions[0], Action::OpenTab("https://a.test/x".into()));
        assert_eq!(actions[2], Action::OpenTab("https://a.test/i.png".into()));
        assert_eq!(actions[3], Action::CopyImage("https://a.test/i.png".into()));
    }

    #[test]
    fn selected_text_copies_and_searches() {
        let text = ContextMenu { selection: "  silo   browser  tabs ".into(), ..ContextMenu::default() };
        assert_eq!(labels(&text), ["Copy", "---", "Search Google for “silo browser tabs”"]);
        let long = ContextMenu { selection: "a very long selection of words to search".into(), ..ContextMenu::default() };
        assert_eq!(labels(&long)[2], "Search Google for “a very long selection of…”");
    }

    #[test]
    fn fields_edit_by_what_they_allow() {
        let edit = silo_web::EditState { paste: true, select_all: true, copy: true, cut: true, ..Default::default() };
        let field = ContextMenu { editable: true, edit, ..ContextMenu::default() };
        assert_eq!(
            rows(&field, true),
            [
                ("Undo".into(), false),
                ("Redo".into(), false),
                ("---".into(), true),
                ("Cut".into(), false),
                ("Copy".into(), false),
                ("Paste".into(), true),
                ("Select All".into(), true),
            ],
            "nothing selected: no Cut or Copy"
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
        let none = ContextMenu { misspelled: Some("qqq".into()), ..field };
        assert_eq!(rows(&none, true)[0], ("No Guesses Found".into(), false));
    }
}
