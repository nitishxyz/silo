use super::*;

fn chord(text: &str) -> Chord {
    Chord::parse(text).unwrap()
}

fn overrides(json: &str) -> Overrides {
    parse(json).unwrap()
}

#[test]
fn chords_round_trip_in_canonical_order() {
    for (text, canonical, hint) in [
        ("cmd-shift-n", "shift-cmd-n", "⇧⌘N"),
        ("cmd-}", "cmd-}", "⇧⌘]"),
        ("cmd-shift-]", "cmd-}", "⇧⌘]"),
        ("cmd-alt-left", "alt-cmd-left", "⌥⌘←"),
        ("ctrl-cmd-enter", "ctrl-cmd-enter", "⌃⌘↩"),
        ("cmd-N", "shift-cmd-n", "⇧⌘N"),
        ("cmd--", "cmd--", "⌘-"),
        ("cmd-shift--", "cmd-_", "⇧⌘-"),
        ("ctrl-space", "ctrl-space", "⌃Space"),
        ("f5", "f5", "F5"),
        ("Command-Option-t", "alt-cmd-t", "⌥⌘T"),
    ] {
        let parsed = chord(text);
        assert_eq!(parsed.to_string(), canonical, "{text}");
        assert_eq!(parsed.hint(), hint, "{text}");
        assert_eq!(chord(canonical), parsed, "{canonical} parses back");
    }
    for bad in ["", "cmd-", "cmd-foo", "hyper-k", "cmd-f99"] {
        assert!(Chord::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_key_press_is_the_chord_its_binding_parses_to() {
    let press = |text: &str| Chord::from_keystroke(&gpui::Keystroke::parse(text).unwrap());
    assert_eq!(press("cmd-shift-n"), Some(chord("cmd-shift-n")));
    // macOS reports ⇧⌘] as `cmd-}`.
    assert_eq!(press("cmd-}"), Some(chord("cmd-shift-]")));
    assert_eq!(press("fn-a"), None, "Fn chords are voice typing's");
}

#[test]
fn every_default_parses_and_ids_round_trip() {
    for command in Command::ALL {
        assert_eq!(defaults(*command).len(), command.default_keys().len(), "{command:?}");
        assert_eq!(Command::from_id(command.id()), Some(*command));
    }
    assert!(merge(&Overrides::new()).problems.is_empty(), "the defaults don't conflict");
    assert_eq!(defaults(Command::NewPane), [chord("cmd-enter")], "⌘⏎ is New Pane");
    assert_eq!(defaults(Command::NewWindow), [chord("cmd-n")], "⌘N is New Window");
}

#[test]
fn overrides_rebind_add_and_unbind() {
    let keymap = merge(&overrides(r#"{ "SplitRight": "cmd-e", "NewTab": ["cmd-t", "ctrl-alt-t"], "HideSilo": [], "Quit": null }"#));
    assert!(keymap.problems.is_empty(), "{:?}", keymap.problems);
    assert_eq!(keymap.bindings[&Command::SplitRight], [chord("cmd-e")]);
    assert_eq!(keymap.bindings[&Command::NewTab], [chord("cmd-t"), chord("ctrl-alt-t")]);
    assert!(keymap.bindings[&Command::HideSilo].is_empty());
    assert!(keymap.bindings[&Command::Quit].is_empty());
    assert_eq!(keymap.bindings[&Command::SplitDown], defaults(Command::SplitDown), "others keep their defaults");
}

#[test]
fn unknown_ids_bad_chords_and_duplicates_are_reported() {
    let keymap = merge(&overrides(r#"{ "Nope": "cmd-e", "SplitRight": ["cmd-e", "cmd-e", "cmd-"], "SplitDown": "cmd-t" }"#));
    assert_eq!(keymap.bindings[&Command::SplitRight], [chord("cmd-e")], "a repeated chord counts once");
    assert_eq!(keymap.problems, ["Unknown command “Nope”", "Split Right: “cmd-” isn't a shortcut", "⌘T is used by New Tab and Split Down"]);
    assert!(parse(r#"{ "NewTab": 3 }"#).is_err());
    assert!(parse(r#"{ "NewTab": ["cmd-t", 3] }"#).is_err());
    assert!(parse("not json").is_err());
}

#[test]
fn conflicts_are_found() {
    let keymap = merge(&overrides(r#"{ "SplitDown": ["cmd-d"] }"#));
    assert_eq!(conflicts(&keymap.bindings), [(chord("cmd-d"), vec![Command::SplitRight, Command::SplitDown])]);
    assert!(merge(&Overrides::new()).problems.is_empty());
}

#[test]
fn setting_the_defaults_drops_the_entry() {
    let mut o = Overrides::new();
    set(&mut o, Command::SplitRight, &[chord("cmd-e")]);
    assert_eq!(o.get("SplitRight"), Some(&vec!["cmd-e".to_owned()]));
    set(&mut o, Command::SplitRight, &defaults(Command::SplitRight));
    assert!(o.is_empty());
    set(&mut o, Command::Quit, &[]);
    assert_eq!(merge(&o).bindings[&Command::Quit], []);
}

#[test]
fn reserved_and_terminal_chords_are_warned_about() {
    assert_eq!(chord("cmd-space").warning(), Some("macOS uses this shortcut"));
    assert_eq!(chord("cmd-shift-3").warning(), Some("macOS uses this shortcut"));
    assert_eq!(chord("ctrl-c").warning(), Some("Terminal programs use this key"));
    assert_eq!(chord("cmd-left").warning(), Some("Terminals use it for line editing"));
    assert_eq!(chord("cmd-e").warning(), None);
    for command in Command::ALL {
        for chord in defaults(*command) {
            assert_eq!(chord.warning(), None, "{command:?} {chord}");
        }
    }
}

#[test]
fn files_save_and_load() {
    let home = std::env::temp_dir().join(format!("silo-keymap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(load(&home), (Overrides::new(), None), "no file: no overrides");
    let mut o = Overrides::new();
    set(&mut o, Command::SplitRight, &[chord("cmd-e"), chord("ctrl-cmd-e")]);
    save(&home, &o).unwrap();
    assert_eq!(load(&home), (o, None));
    std::fs::write(path(&home), "{").unwrap();
    assert!(load(&home).1.is_some(), "an invalid file is reported");
    std::fs::remove_dir_all(&home).unwrap();
}
