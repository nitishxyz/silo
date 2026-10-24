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
        pid: None,
        browser: None,
        agent: None,
    }
}

fn page(url: &str, title: &str) -> RecentPage {
    RecentPage { host: "local".into(), url: url.into(), title: title.into() }
}

fn split(via: Command) -> Target {
    Target::Split { block: "b".into(), axis: Axis::Horizontal, via }
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
}

#[test]
fn rows_follow_the_intent() {
    assert_eq!(kinds("age"), vec![Kind::Agent]);
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
    assert_eq!(row_hint(Kind::Terminal, &split(Command::SplitRight), Kind::Terminal).as_deref(), Some("⌘D"));
    assert_eq!(row_hint(Kind::Terminal, &split(Command::SplitDown), Kind::Terminal).as_deref(), Some("⇧⌘D"));
    assert_eq!(row_hint(Kind::Terminal, &split(Command::NewPane), Kind::Terminal).as_deref(), Some("⌘↩"));
    assert_eq!(row_hint(Kind::Agent, &split(Command::NewPane), Kind::Terminal).as_deref(), Some("⇧⌘O"));
    assert_eq!(row_hint(Kind::Agent, &split(Command::SplitRight), Kind::Terminal), None, "⇧⌘O picks its own side");
    assert_eq!(row_hint(Kind::Browser, &split(Command::SplitDown), Kind::Terminal), None);
}

#[test]
fn choosers_stand_for_their_instant_splits() {
    assert_eq!(split_via(Command::SplitRightChooser), Some(Command::SplitRight));
    assert_eq!(split_via(Command::SplitDownChooser), Some(Command::SplitDown));
    assert_eq!(split_via(Command::NewPaneChooser), Some(Command::NewPane));
    assert_eq!(split_via(Command::NewTabChooser), None, "a tab");
    assert_eq!(split_via(Command::SplitRight), None);
}
