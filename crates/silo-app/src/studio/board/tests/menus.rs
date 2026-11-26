//! Menus as data: the context menus, the selection actions, the toolbar, and what choosing does.

use silo_proto::studio::{Canvas, EdgeRole, GenKind, GenStatus, Op, StudioMedia};

use super::super::menu::{canvas_menu, node_menu, selection_actions, selection_menu, toolbar, wire_menu};
use super::super::{Action, Effect, MenuEntry, MenuGroup, MenuTarget, Point, Tool};
use super::fixtures::*;

fn fixture() -> Canvas {
    let two = vec![
        take("canvases/ep01/assets/t1.png", StudioMedia::Image, 1920, 1080),
        take("canvases/ep01/assets/t2.png", StudioMedia::Image, 1920, 1080),
    ];
    let mut failed = card("failed", GenKind::Video, 2000.0, 1000.0);
    if let silo_proto::studio::NodeBody::Gen { status, error, .. } = &mut failed.body {
        *status = GenStatus::Failed;
        *error = Some("quota".into());
    }
    canvas(
        vec![
            card("draft", GenKind::Image, 0.0, 0.0),
            with_takes(card("done", GenKind::Image, 600.0, 0.0), two),
            with_status(card("running", GenKind::Video, 1200.0, 0.0), GenStatus::Running),
            failed,
            note("n", 0.0, 600.0, "# Beat\nMara hears the bell"),
            image("i", 400.0, 600.0),
            video("vid", 800.0, 600.0),
            asset("aud", StudioMedia::Audio, 1200.0, 600.0, 320.0, 72.0),
            entity("mara", 0.0, 1000.0, "characters/mara"),
            entity("teo", 400.0, 1000.0, "characters/teo"),
            frame("f", 0.0, 1400.0, 600.0, 400.0),
            card("fed", GenKind::Video, 1800.0, 0.0),
        ],
        vec![edge("w", "draft", "fed", EdgeRole::FirstFrame)],
    )
}

fn labels(entries: &[MenuEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.label.as_str()).collect()
}

fn all(groups: &[MenuGroup]) -> Vec<&MenuEntry> {
    groups.iter().flat_map(|g| &g.entries).collect()
}

fn find<'a>(groups: &'a [MenuGroup], label: &str) -> Option<&'a MenuEntry> {
    groups.iter().flat_map(|g| &g.entries).find(|e| e.label == label)
}

fn node(c: &Canvas, id: &str) -> Vec<MenuGroup> {
    node_menu(c, &sections(), id)
}

#[test]
fn the_empty_canvas_menu() {
    let g = canvas_menu();
    assert_eq!(g[0].title.as_deref(), Some("Generate here"));
    assert_eq!(labels(&g[0].entries), ["Image", "Video", "Voice", "Sound", "Music"]);
    assert_eq!(g[0].entries[2].action, Action::Generate(GenKind::Speech));
    assert_eq!(labels(&g[1].entries), ["Note", "Frame", "Add from World…"]);
    assert_eq!(g[1].entries[0].shortcut, Some("N"));
    assert_eq!(labels(&g[2].entries), ["Tidy up"]);
    assert_eq!(g[2].entries[0].action, Action::Tidy);
    assert!(all(&g).iter().all(|e| e.enabled && !e.danger));
}

#[test]
fn a_draft_cards_menu() {
    let c = fixture();
    let g = node(&c, "draft");
    assert_eq!(labels(&g[0].entries), ["Run", "Settings"]);
    let run = &g[0].entries[0];
    assert!(!run.enabled);
    assert_eq!(run.reason.as_deref(), Some("Write a prompt or wire an input first"));
    // Make from this: there is nothing to make from yet.
    assert_eq!(g[1].title.as_deref(), Some("Make from this"));
    assert_eq!(labels(&g[1].entries), ["Image", "Video"]);
    for e in &g[1].entries {
        assert!(!e.enabled);
        assert_eq!(e.reason.as_deref(), Some("Run Image draft first"));
    }
    assert_eq!(labels(&g[2].entries), ["Ask agent about this", "Copy", "Duplicate"]);
    assert_eq!(labels(&g[3].entries), ["Delete"]);
    assert!(g[3].entries[0].danger);
    // A card wired from it can run (its input is the problem, shown in settings).
    assert!(find(&node(&c, "fed"), "Run").unwrap().enabled);
}

#[test]
fn cards_menus_follow_their_status() {
    let c = fixture();
    let g = node(&c, "done");
    assert_eq!(labels(&g[0].entries), ["Run again", "Takes", "Settings"]);
    assert_eq!(labels(&g[2].entries), ["Ask agent about this", "Set as entity sheet…", "Reveal in Files", "Copy", "Save…", "Duplicate"]);
    assert_eq!(g[0].entries[0].action, Action::Run);
    let g = node(&c, "running");
    assert_eq!(g[0].entries[0].label, "Stop");
    assert_eq!(g[0].entries[0].action, Action::Stop);
    let g = node(&c, "failed");
    assert_eq!(g[0].entries[0].label, "Retry");
    assert_eq!(g[0].entries[0].action, Action::Run);
}

#[test]
fn notes_assets_entities_and_frames_menus() {
    let c = fixture();
    let g = node(&c, "n");
    assert_eq!(labels(&g[0].entries), ["Edit"]);
    assert_eq!(labels(&g[1].entries), ["Image", "Video", "Voice", "Sound", "Music"]);
    assert!(g[1].entries.iter().all(|e| matches!(e.action, Action::Make { port: EdgeRole::Text, .. }) && e.enabled));

    let g = node(&c, "i");
    assert_eq!(labels(&g[0].entries), ["View"]);
    assert_eq!(g[1].entries[1].action, Action::Make { makes: GenKind::Video, port: EdgeRole::FirstFrame });
    assert!(find(&g, "Set as entity sheet…").is_some() && find(&g, "Save…").is_some());
    assert_eq!(labels(&g.last().unwrap().entries), ["Delete", "Delete File…"]);
    assert!(g.last().unwrap().entries.iter().all(|e| e.danger));

    let g = node(&c, "vid");
    assert_eq!(labels(&g[1].entries), ["Video"]);
    assert!(find(&g, "Set as entity sheet…").is_none());
    let g = node(&c, "aud");
    assert_eq!(labels(&g[1].entries), ["Video"]);
    assert_eq!(g[1].entries[0].action, Action::Make { makes: GenKind::Video, port: EdgeRole::Audio });

    let g = node(&c, "mara");
    assert_eq!(labels(&g[0].entries), ["Open in World"]);
    assert!(g[1].entries.iter().all(|e| e.enabled));
    let g = node(&c, "teo");
    assert_eq!(g[1].entries[0].reason.as_deref(), Some("Teo has no sheet yet"));
    assert!(find(&g, "Reveal in Files").is_none());

    let g = node(&c, "f");
    assert_eq!(labels(&g[0].entries), ["Rename"]);
    assert!(g.iter().all(|g| g.title.as_deref() != Some("Make from this")));
    assert!(find(&g, "Copy").is_none());
    assert!(find(&g, "Delete File…").is_none());
}

#[test]
fn selection_and_wire_menus() {
    let g = selection_menu(3);
    assert_eq!(g[0].title.as_deref(), Some("3 selected"));
    assert_eq!(labels(&g[0].entries), ["Ask agent about these", "Duplicate"]);
    assert_eq!(labels(&g[1].entries), ["Delete"]);
    let g = wire_menu();
    assert_eq!(g[0].entries[0].action, Action::Unwire);
    assert!(g[0].entries[0].danger);
}

#[test]
fn selection_actions_per_node() {
    let c = fixture();
    let s = sections();
    let acts = |id: &str| selection_actions(&c, &s, id);
    assert_eq!(labels(&acts("n")), ["Visualize"]);
    assert_eq!(acts("n")[0].action, Action::Make { makes: GenKind::Image, port: EdgeRole::Text });
    assert_eq!(labels(&acts("i")), ["Animate", "Edit"]);
    assert_eq!(acts("i")[0].action, Action::Make { makes: GenKind::Video, port: EdgeRole::FirstFrame });
    assert_eq!(acts("i")[1].action, Action::Make { makes: GenKind::Image, port: EdgeRole::Reference });
    assert_eq!(labels(&acts("vid")), ["Extend"]);
    assert_eq!(acts("vid")[0].action, Action::Make { makes: GenKind::Video, port: EdgeRole::Video });
    assert!(acts("aud").is_empty());
    assert!(acts("f").is_empty());
    assert_eq!(labels(&acts("mara")), ["Animate", "Edit"]);

    let done = acts("done");
    assert_eq!(labels(&done), ["Animate", "Edit", "Run again", "Takes", "Settings"]);
    assert!(done.iter().all(|e| e.enabled));

    // A draft: nothing to make from yet, nothing to run, one take at most.
    let draft = acts("draft");
    assert_eq!(labels(&draft), ["Animate", "Edit", "Run", "Takes", "Settings"]);
    assert_eq!(draft[0].reason.as_deref(), Some("Run Image draft first"));
    assert!(!draft[1].enabled && !draft[2].enabled);
    assert_eq!(draft[3].reason.as_deref(), Some("Only one take"));
    assert!(draft[4].enabled);
    assert_eq!(labels(&acts("running"))[1], "Stop");
}

#[test]
fn the_selection_bar_floats_over_a_single_selected_node() {
    let c = fixture();
    let mut b = board(&c);
    assert!(b.selection_bar().is_none());
    b.select(&["i".to_owned()]);
    let bar = b.selection_bar().unwrap();
    assert_eq!(bar.node, "i");
    assert_eq!(bar.anchor, Point::new(560.0, 600.0));
    assert_eq!(labels(&bar.entries), ["Animate", "Edit"]);
    b.select(&["i".to_owned(), "n".to_owned()]);
    assert!(b.selection_bar().is_none());
    b.select(&["f".to_owned()]);
    assert!(b.selection_bar().is_none());
    // Not while dragging.
    b.select(&["i".to_owned()]);
    drag_hold(&mut b, pt(500.0, 650.0), pt(700.0, 700.0), NONE);
    assert!(b.selection_bar().is_none());
}

#[test]
fn the_toolbar() {
    let t = toolbar(Tool::Note);
    assert_eq!(labels(&t), ["Select", "Note", "Frame", "Prompt", "Import"]);
    assert_eq!(t.iter().map(|e| e.shortcut).collect::<Vec<_>>(), [Some("V"), Some("N"), Some("F"), Some("G"), None]);
    assert_eq!(t.iter().filter(|e| e.active).map(|e| e.label.as_str()).collect::<Vec<_>>(), ["Note"]);
    let mut b = board(&fixture());
    assert!(b.toolbar()[0].active);
    b.choose(&Action::Tool(Tool::Frame));
    assert_eq!(b.tool(), Tool::Frame);
    assert!(b.toolbar()[2].active);
    let fx = b.choose(&Action::Prompt);
    assert!(matches!(fx[..], [Effect::OpenPrompt { view, .. }] if view == Point::new(VIEW.w / 2.0, VIEW.h / 2.0)));
}

// ---------------------------------------------------------------- the board's menus and choosing

#[test]
fn the_board_opens_the_menu_for_what_is_under_the_pointer() {
    let c = fixture();
    let mut b = board(&c);
    right_click(&mut b, pt(3000.0, 3000.0));
    assert_eq!(b.menu().unwrap().target, MenuTarget::Canvas { frame: None });
    assert_eq!(b.menu().unwrap().canvas_at, pt(3000.0, 3000.0));
    right_click(&mut b, pt(300.0, 1700.0));
    assert_eq!(b.menu().unwrap().target, MenuTarget::Canvas { frame: Some("f".into()) });
    right_click(&mut b, pt(100.0, 650.0));
    assert_eq!(b.menu().unwrap().target, MenuTarget::Node("n".into()));
    // On one of several selected: the selection's menu.
    b.select(&["n".to_owned(), "i".to_owned()]);
    right_click(&mut b, pt(100.0, 650.0));
    let m = b.menu().unwrap();
    assert_eq!(m.target, MenuTarget::Selection(vec!["n".into(), "i".into()]));
    assert_eq!(m.groups[0].title.as_deref(), Some("2 selected"));
    // A press elsewhere closes it.
    let fx = click(&mut b, pt(3000.0, 3000.0), NONE);
    assert!(fx.contains(&Effect::CloseMenu));
    assert!(b.menu().is_none());
}

#[test]
fn choosing_node_actions() {
    let c = fixture();
    let mut b = board(&c);
    let on = |b: &mut super::super::Board, p: Point, a: Action| {
        right_click(b, p);
        b.choose(&a)
    };
    let done = pt(700.0, 100.0);
    assert_eq!(on(&mut b, done, Action::Settings), [Effect::CloseMenu, Effect::Settings { node: "done".into() }]);
    assert_eq!(on(&mut b, done, Action::Takes), [Effect::CloseMenu, Effect::Takes { node: "done".into() }]);
    let sheet = on(&mut b, done, Action::SetAsEntitySheet);
    assert_eq!(sheet, [Effect::CloseMenu, Effect::SetAsEntitySheet { path: "canvases/ep01/assets/t2.png".into() }]);
    assert_eq!(on(&mut b, done, Action::AskAgent), [Effect::CloseMenu, Effect::AskAgent { nodes: vec!["done".into()] }]);
    let img = pt(500.0, 650.0);
    let path = "canvases/ep01/assets/i.png".to_owned();
    assert_eq!(on(&mut b, img, Action::RevealInFiles), [Effect::CloseMenu, Effect::RevealInFiles { path: path.clone() }]);
    assert_eq!(on(&mut b, img, Action::Save), [Effect::CloseMenu, Effect::Save { path: path.clone() }]);
    assert_eq!(on(&mut b, img, Action::DeleteFile), [Effect::CloseMenu, Effect::DeleteFile { node: "i".into(), path }]);
    assert_eq!(on(&mut b, pt(100.0, 1100.0), Action::OpenInWorld), [Effect::CloseMenu, Effect::OpenWorld { entity: "characters/mara".into() }]);
    let fx = on(&mut b, img, Action::Delete);
    assert_eq!(sent(&fx).1, [Op::RemoveNode { id: "i".into() }]);
    let fx = on(&mut b, pt(100.0, 650.0), Action::Duplicate);
    assert_eq!(sent(&fx).1.len(), 1);
}

#[test]
fn unwiring_from_the_wire_menu() {
    let c = fixture();
    let mut b = board(&c);
    let mid = b.scene().curve(&c, 0).unwrap().mid();
    let fx = right_click(&mut b, mid);
    assert!(fx.contains(&Effect::Menu));
    assert_eq!(b.menu().unwrap().target, MenuTarget::Wire("w".into()));
    assert_eq!(b.selected_edge(), Some("w"));
    let (_, ops) = sent(&b.choose(&Action::Unwire));
    assert_eq!(ops, [Op::RemoveEdge { id: "w".into() }]);
    assert!(b.canvas().edges.is_empty());
}
