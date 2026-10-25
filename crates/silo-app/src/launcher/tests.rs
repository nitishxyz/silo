use silo_proto::model::{Lifecycle, TermSize};

use super::*;

fn block(id: &str, kind: BlockKind) -> BlockInfo {
    BlockInfo {
        id: id.into(),
        session_id: "s".into(),
        kind,
        title: String::new(),
        cwd: None,
        exited: None,
        lifecycle: Lifecycle::Running,
        size: TermSize::new(80, 24),
        owner: None,
        owner_name: None,
        owner_block: None,
        pid: None,
        browser: None,
        agent: None,
        files: None,
        changes: None,
    }
}

fn page(url: &str, title: &str) -> RecentPage {
    RecentPage { host: "local".into(), url: url.into(), title: title.into() }
}

fn split(side: Side, auto: Side) -> Target {
    Target::Split { block: "b".into(), side, auto }
}

/// The kinds of `text`'s rows, in order (no recents).
fn kinds(text: &str) -> Vec<Kind> {
    rows(text, &[]).iter().map(Row::kind).collect()
}

#[test]
fn new_tab_opens_the_focused_kind() {
    let blocks = [block("t", BlockKind::Terminal), block("b", BlockKind::Browser), block("a", BlockKind::Agent), block("u", BlockKind::Unknown)];
    assert_eq!(same_kind(Some("t"), &blocks), Kind::Terminal);
    assert_eq!(same_kind(Some("b"), &blocks), Kind::Browser);
    assert_eq!(same_kind(Some("a"), &blocks), Kind::Agent);
    assert_eq!(same_kind(None, &blocks), Kind::Terminal, "nothing focused");
    assert_eq!(same_kind(Some("t"), &[]), Kind::Terminal, "no window");
    assert_eq!(same_kind(Some("u"), &blocks), Kind::Terminal, "a kind no tab starts with");
}

#[test]
fn every_shown_kind_is_a_row_and_enter_on_the_preselected_one_is_the_instant_command() {
    let all = rows("", &[]);
    assert_eq!(all, SHOWN.iter().map(|k| Row::Kind(*k, Vec::new())).collect::<Vec<_>>());
    for kind in SHOWN {
        assert_eq!(all[preselect("", &all, *kind)].kind(), *kind);
        assert_ne!(kind.tab_command(), Command::NewTab);
        assert_eq!(SHOWN.iter().filter(|k| k.block_kind() == kind.block_kind()).count(), 1);
    }
}

#[test]
fn input_is_classified() {
    assert_eq!(classify(""), Intent::Filter(String::new()));
    assert_eq!(classify("  bro "), Intent::Filter("bro".into()));
    assert!(matches!(classify("example.com/a"), Intent::Open(url) if url.contains("example.com/a")));
    assert!(matches!(classify("https://x.dev"), Intent::Open(url) if url == "https://x.dev" || url == "https://x.dev/"));
    assert!(matches!(classify("localhost:3000"), Intent::Open(url) if url.contains("localhost:3000")));
    assert_eq!(classify(">  cargo test "), Intent::Run("cargo test".into()));
    assert_eq!(classify(">"), Intent::Run(String::new()));
    assert_eq!(classify("> ls example.com"), Intent::Run("ls example.com".into()), "a command, not an address");
    assert_eq!(classify("@build"), Intent::Agent("build".into()));
    assert_eq!(classify("@"), Intent::Agent(String::new()));
    assert_eq!(classify("browser"), Intent::Filter("browser".into()), "a word is not an address");
    assert_eq!(classify("two words"), Intent::Filter("two words".into()));
    assert_eq!(classify(" /etc/hosts "), Intent::Path("/etc/hosts".into()));
    assert_eq!(classify("~/src"), Intent::Path("~/src".into()));
    assert_eq!(classify("~"), Intent::Path("~".into()));
    assert_eq!(classify("~user"), Intent::Filter("~user".into()), "only this user's home");
}

#[test]
fn files_is_a_kind_and_a_path_opens_it() {
    assert_eq!(kinds("fil"), vec![Kind::Files]);
    assert_eq!(rows("~/src/app", &[]), vec![Row::Path("~/src/app".into())]);
    assert_eq!(Row::Path("/x".into()).kind(), Kind::Files);
    assert_eq!(
        (Kind::Files.block_kind(), Kind::Files.tab_command(), Kind::Files.split_command()),
        (BlockKind::Files, Command::NewFilesTab, Command::NewFiles)
    );
    assert_eq!(Kind::Files.split_command().feature(), Some(silo_proto::compat::feature::FILES), "hosts without Files blocks say so");
    assert_eq!(digit_pick("4", "", rows("", &[]).len()), Some(3), "fourth in the list");
    assert_eq!(rows("", &[])[3], Row::Kind(Kind::Files, Vec::new()));
}

#[test]
fn rows_follow_the_intent() {
    assert_eq!(kinds("agen"), vec![Kind::Agent]);
    assert_eq!(kinds("chan"), vec![Kind::Changes]);
    assert_eq!(kinds("term"), vec![Kind::Terminal]);
    assert_eq!(kinds("  BRO "), vec![Kind::Browser], "trimmed, any case");
    #[allow(clippy::single_range_in_vec_init, reason = "one highlighted run")]
    let highlighted = vec![0..2];
    assert_eq!(rows("br", &[]), vec![Row::Kind(Kind::Browser, highlighted)], "matched range highlighted");
    assert!(rows("zzz", &[]).is_empty());
    let open = rows("example.com", &[]);
    assert!(matches!(open.as_slice(), [Row::Open { url, title }] if url.contains("example.com") && title.is_empty()), "{open:?}");
    assert_eq!(rows(">make", &[]), vec![Row::Run("make".into())]);
    assert_eq!(rows(">make", &[])[0].kind(), Kind::Terminal);
    assert_eq!(kinds(">"), vec![Kind::Terminal]);
    assert_eq!(kinds("@plan"), vec![Kind::Agent]);
    assert!(intent_note("@plan").is_some_and(|n| n.contains("plan")));
    assert_eq!(intent_note("@"), None);
    assert_eq!(intent_note("term"), None);
}

#[test]
fn recent_pages_follow_the_kinds() {
    let recents = [page("https://a.dev/", "Alpha"), page("https://b.dev/", "Beta"), page("https://c.dev/", ""), page("https://d.dev/", "Delta")];
    let all = rows("", &recents);
    assert_eq!(all.len(), SHOWN.len() + RECENTS, "the newest few");
    assert_eq!(all[SHOWN.len()], Row::Open { url: "https://a.dev/".into(), title: "Alpha".into() });
    assert!(all[SHOWN.len()..].iter().all(|r| r.kind() == Kind::Browser));
    let beta = rows("beta", &recents);
    assert_eq!(beta, vec![Row::Open { url: "https://b.dev/".into(), title: "Beta".into() }], "matched by title");
    let by_url = rows("d.dev", &recents);
    assert!(matches!(by_url.as_slice(), [Row::Open { title, .. }] if title.is_empty()), "an address opens as typed: {by_url:?}");
    assert_eq!(rows("delta", &recents).len(), 1);
    assert!(rows(">ls", &recents).iter().all(|r| !matches!(r, Row::Open { .. })));
}

#[test]
fn selection_wraps_digits_pick_and_typing_selects_the_best() {
    assert_eq!(step(0, 3, -1), 2);
    assert_eq!(step(2, 3, 1), 0);
    assert_eq!(step(1, 3, 1), 2);
    assert_eq!(step(0, 0, 1), 0, "no rows");
    assert_eq!(digit_pick("1", "", 3), Some(0));
    assert_eq!(digit_pick("3", "", 3), Some(2));
    assert_eq!(digit_pick("4", "", 3), None, "past the rows");
    assert_eq!(digit_pick("0", "", 3), None);
    assert_eq!(digit_pick("a", "", 3), None);
    assert_eq!(digit_pick("1", "192.", 3), None, "digits are text once typing");
    let both = rows("r", &[]);
    assert_eq!(both.len(), 2);
    assert_eq!(preselect("r", &both, Kind::Browser), 0, "typing selects the best match");
    let all = rows("", &[]);
    assert_eq!(preselect("", &all, Kind::Agent), 2);
    assert_eq!(preselect("", &[], Kind::Agent), 0);
}

#[test]
fn rows_show_true_shortcuts() {
    assert_eq!(row_hint(Kind::Browser, &Target::Tab, Kind::Terminal).as_deref(), Some("⌥⌘T"));
    assert_eq!(row_hint(Kind::Terminal, &Target::Tab, Kind::Terminal).as_deref(), Some("⌘T"), "⌘T opens a terminal here");
    assert_eq!(row_hint(Kind::Terminal, &Target::Tab, Kind::Browser), None, "⌘T opens a browser here");
    assert_eq!(row_hint(Kind::Terminal, &split(Side::Right, Side::Down), Kind::Terminal).as_deref(), Some("⌘D"));
    assert_eq!(row_hint(Kind::Terminal, &split(Side::Down, Side::Right), Kind::Terminal).as_deref(), Some("⇧⌘D"));
    assert_eq!(row_hint(Kind::Agent, &split(Side::Right, Side::Right), Kind::Terminal).as_deref(), Some("⇧⌘O"), "⌘↩'s side");
    assert_eq!(row_hint(Kind::Agent, &split(Side::Down, Side::Right), Kind::Terminal), None, "⇧⌘O picks its own side");
    assert_eq!(row_hint(Kind::Browser, &split(Side::Down, Side::Down), Kind::Terminal), None);
}

#[test]
fn only_new_pane_chooses_a_side_starting_where_new_pane_splits() {
    let wide = chooser_target(Command::NewPaneChooser, Some("b".into()), |_| Axis::Horizontal);
    assert_eq!(wide, Some(split(Side::Right, Side::Right)), "⌘↩ splits a wide pane right");
    let tall = chooser_target(Command::NewPaneChooser, Some("b".into()), |b| {
        assert_eq!(b, "b");
        Axis::Vertical
    });
    assert_eq!(tall, Some(split(Side::Down, Side::Down)), "⌘↩ splits a tall pane down");
    assert_eq!(chooser_target(Command::NewPaneChooser, None, |_| Axis::Horizontal), None, "nothing focused");
    assert_eq!(chooser_target(Command::NewTabChooser, Some("b".into()), |_| unreachable!()), Some(Target::Tab), "a tab has no side");
    for axis in [Axis::Horizontal, Axis::Vertical] {
        assert_eq!(Side::of_axis(axis).axis(), axis);
    }
}

#[test]
fn tab_and_left_right_cycle_the_side() {
    assert_eq!(SIDES, [Side::Right, Side::Down], "splits put the new block second: no left or up");
    assert_eq!(side_key("tab", false), Some(1));
    assert_eq!(side_key("tab", true), Some(-1));
    assert_eq!(side_key("right", false), Some(1));
    assert_eq!(side_key("left", false), Some(-1));
    assert_eq!(side_key("up", false), None, "↑↓ move through the rows");
    assert_eq!(side_key("down", false), None);
    assert_eq!(side_key("enter", false), None);
    assert_eq!(side_key("a", false), None);
    assert_eq!(Side::Right.step(1), Side::Down);
    assert_eq!(Side::Down.step(1), Side::Right, "wraps");
    assert_eq!(Side::Right.step(-1), Side::Down, "⇧tab wraps back");
    assert_eq!(Side::Down.step(-1), Side::Right);
    let mut side = Side::Right;
    for key in ["tab", "tab", "right", "left"] {
        side = side.step(side_key(key, false).unwrap());
    }
    assert_eq!(side, Side::Right);
    assert_eq!(Side::Right.label(), "Right");
    assert_eq!(Side::Down.label(), "Down");
}

#[test]
fn confirming_splits_to_the_chosen_side() {
    use silo_proto::control::Method;
    assert_eq!(Side::Right.command(), Command::SplitRight);
    assert_eq!(Side::Down.command(), Command::SplitDown);
    let rows = [
        (Row::Kind(Kind::Terminal, Vec::new()), BlockKind::Terminal, None),
        (Row::Kind(Kind::Browser, Vec::new()), BlockKind::Browser, None),
        (Row::Kind(Kind::Agent, Vec::new()), BlockKind::Agent, None),
        (Row::Open { url: "https://a.dev/".into(), title: String::new() }, BlockKind::Browser, Some("https://a.dev/".to_owned())),
        (Row::Run("make".into()), BlockKind::Terminal, None),
        (Row::Kind(Kind::Files, Vec::new()), BlockKind::Files, None),
    ];
    for side in SIDES {
        let want_axis = if *side == Side::Right { Axis::Horizontal } else { Axis::Vertical };
        assert_eq!(side.axis(), want_axis);
        for (row, want_kind, want_url) in &rows {
            let Method::BlockSplit { block_id, axis, spawn, kind, url, agent } = split_method("b", *side, row, Some("/w".into())) else {
                panic!("{row:?}: not a split");
            };
            assert_eq!((block_id.as_str(), axis, kind, &url), ("b", want_axis, *want_kind, want_url), "{side:?} {row:?}");
            assert!(agent.is_none());
            assert!(spawn.command.is_empty(), "a >command is typed into the shell, not spawned");
            let cwd = if *want_kind == BlockKind::Browser { None } else { Some("/w".to_owned()) };
            assert_eq!(spawn.cwd, cwd, "{row:?}");
        }
        // A typed path is the new Files block's root, whatever the split block's folder.
        let Method::BlockSplit { kind, spawn, .. } = split_method("b", *side, &Row::Path("~/notes".into()), Some("/w".into())) else { panic!() };
        assert_eq!((kind, spawn.cwd.as_deref()), (BlockKind::Files, Some("~/notes")));
    }
}
