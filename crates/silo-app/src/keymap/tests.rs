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

// ---- init.lua on top ----

fn sequence(text: &str) -> Sequence {
    Sequence::parse(text).unwrap()
}

/// A home server's config, as `config.get` answers it.
fn config(value: serde_json::Value) -> ConfigState {
    serde_json::from_value(value).unwrap()
}

fn command(id: &str) -> serde_json::Value {
    serde_json::json!({ "type": "command", "id": id })
}

#[test]
fn without_a_config_the_json_keymap_stands() {
    let json = merge(&overrides(r#"{ "SplitRight": "cmd-e" }"#));
    assert_eq!(with_config(json.clone(), None), json);
    let keymap = with_config(json, Some(&ConfigState::default()));
    assert_eq!(keymap.shortcuts(Command::SplitRight), [sequence("cmd-e")]);
    assert_eq!(keymap.config_count(), 0);
    assert_eq!(keymap.config_path.as_deref(), Some(""), "the server answered");
}

#[test]
fn config_bindings_win_over_defaults_and_the_json_file() {
    let json = merge(&overrides(r#"{ "SplitRight": "cmd-e" }"#));
    let config = config(serde_json::json!({
        "path": "/u/.config/silo/init.lua",
        "bindings": [
            { "keys": "cmd-e", "target": command("NewTab"), "source": "init.lua:3" },
            { "keys": "cmd-shift-x", "target": command("SplitRight"), "source": "init.lua:4" },
            { "keys": "cmd-t", "target": command("NewTab"), "source": "init.lua:5" },
        ],
    }));
    let keymap = with_config(json, Some(&config));
    assert_eq!(keymap.shortcuts(Command::SplitRight), [sequence("cmd-shift-x")], "cmd-e went to New Tab");
    assert_eq!(keymap.shortcuts(Command::NewTab), [sequence("cmd-e"), sequence("cmd-t")], "init.lua's first: the hint");
    assert_eq!(keymap.bindings[&Command::SplitRight], [chord("cmd-e")], "Settings still edits keybindings.json's");
    assert_eq!(keymap.config_shortcuts(Command::NewTab), [sequence("cmd-e"), sequence("cmd-t")]);
    assert_eq!(keymap.problems, ["init.lua:3: ⌘E replaces Split Right's"], "rebinding New Tab's own ⌘T is no conflict");
    assert_eq!(keymap.config_count(), 3);
    assert_eq!(keymap.config_path.as_deref(), Some("/u/.config/silo/init.lua"));
}

#[test]
fn unbinds_take_a_chord_from_every_command_or_only_the_named_one() {
    let json = merge(&overrides(r#"{ "SplitDown": ["cmd-shift-d", "cmd-t"] }"#));
    let config = config(serde_json::json!({
        "unbinds": [
            { "keys": "cmd-k" },
            { "keys": "cmd-t", "target": "NewTab" },
            { "keys": "cmd-shift-d", "target": "SplitRight" },
        ],
        "bindings": [{ "keys": "cmd-k", "target": { "type": "action", "name": "dev_layout" } }],
    }));
    let keymap = with_config(json, Some(&config));
    assert!(keymap.shortcuts(Command::SwitchSession).is_empty(), "the default ⌘K is gone");
    assert!(keymap.shortcuts(Command::NewTab).is_empty());
    assert_eq!(keymap.shortcuts(Command::SplitDown), [sequence("cmd-shift-d"), sequence("cmd-t")], "only New Tab lost ⌘T");
    assert_eq!(keymap.problems, ["⌘T is used by New Tab and Split Down"], "the file's own conflict is still told");
    assert_eq!(keymap.action_hint("dev_layout").as_deref(), Some("⌘K"), "unbinds go first: the binding stands");
    assert!(!keymap.specs().iter().any(|s| s.does == Does::Command(Command::SwitchSession)));
}

#[test]
fn a_chord_the_config_binds_is_not_a_pane_s() {
    let config = config(serde_json::json!({
        "modes": [{ "name": "resize" }],
        "bindings": [
            { "keys": "cmd-r", "target": { "type": "method", "name": "app.mode_enter" }, "args": { "name": "resize" } },
            { "keys": "ctrl-b c", "target": command("NewTab") },
            { "keys": "cmd-l", "mode": "resize", "target": command("NewTab") },
        ],
    }));
    let keymap = with_config(merge(&Overrides::default()), Some(&config));
    let chord = |text: &str| Chord::parse(text).unwrap();
    assert!(keymap.config_starts(&chord("cmd-r")), "a page's reload gives way");
    assert!(keymap.config_starts(&chord("ctrl-b")), "the start of a sequence");
    assert!(!keymap.config_starts(&chord("cmd-l")), "bound in a mode only");
    assert!(!keymap.config_starts(&chord("cmd-t")), "a default");
}

#[test]
fn sequences_bind_and_hint_with_spaces() {
    let json = merge(&overrides(r#"{ "RenameTab": "ctrl-b" }"#));
    let config = config(serde_json::json!({
        "bindings": [
            { "keys": "ctrl-b c", "target": command("NewTab"), "source": "init.lua:1" },
            { "keys": "ctrl-b %", "target": { "type": "method", "name": "block.split" }, "args": { "axis": "horizontal" } },
        ],
    }));
    let keymap = with_config(json, Some(&config));
    assert_eq!(keymap.shortcuts(Command::NewTab)[0].hint(), "⌃B C");
    assert!(keymap.shortcuts(Command::RenameTab).is_empty(), "a chord that starts a sequence gives way");
    assert_eq!(keymap.problems, ["init.lua:1: ⌃B C replaces Rename Tab's ⌃B"]);
    let specs = keymap.specs();
    assert_eq!(specs[0], Spec { keys: "ctrl-b c".into(), does: Does::Command(Command::NewTab), when: When::Always });
    let split = specs.iter().find(|s| s.keys == "ctrl-b %").unwrap();
    assert_eq!(split.does, Does::Run(1));
    assert_eq!(keymap.config[1].target, Target::Method("block.split".into()));
    assert_eq!(keymap.config[1].args, serde_json::json!({ "axis": "horizontal" }));
}

#[test]
fn modes_bind_block_and_come_last() {
    let config = config(serde_json::json!({
        "modes": [{ "name": "resize", "exclusive": true }, { "name": "peek" }],
        "unbinds": [{ "keys": "right", "mode": "resize" }, { "keys": "left", "mode": "resize" }],
        "bindings": [
            { "keys": "cmd-r", "target": { "type": "method", "name": "app.mode_enter" }, "args": { "name": "resize" } },
            { "keys": "left", "mode": "resize", "target": command("MovePaneEdgeLeft") },
            { "keys": "enter", "mode": "resize", "target": { "type": "method", "name": "app.mode_exit" } },
            { "keys": "x", "mode": "nope", "target": command("NewTab"), "source": "init.lua:9" },
        ],
    }));
    let keymap = with_config(merge(&Overrides::new()), Some(&config));
    assert_eq!(keymap.problems, ["init.lua:9: Unknown mode “nope”"]);
    let blocked: Vec<&Bound> = keymap.config.iter().filter(|b| b.target == Target::Blocked).collect();
    assert_eq!(blocked.len(), 1, "a key the mode binds isn't blocked too");
    assert_eq!((blocked[0].mode.as_deref(), blocked[0].keys.to_string().as_str()), (Some("resize"), "right"));
    assert!(keymap.binds("resize", &sequence("right")) && keymap.binds("resize", &sequence("left")) && !keymap.binds("peek", &sequence("left")));
    assert!(keymap.config_shortcuts(Command::MovePaneEdgeLeft).is_empty(), "a mode's keys aren't the command's shortcut");
    assert_eq!(keymap.shortcuts(Command::MovePaneEdgeLeft), [sequence("ctrl-cmd-left")]);
    assert_eq!(keymap.config_count(), 3, "blocked keys don't count");
    let specs = keymap.specs();
    let first_mode = specs.iter().position(|s| matches!(s.when, When::Mode(_))).unwrap();
    assert!(specs[first_mode..].iter().all(|s| s.when == When::Mode("resize".into())), "mode bindings come last");
    assert!(specs[..first_mode].iter().all(|s| s.when == When::NotExclusive), "an exclusive mode can turn the others off");
    assert_eq!(specs.len() - first_mode, 3, "left, enter and the blocked right");
}

#[test]
fn unknown_targets_and_bad_keys_are_problems() {
    let config = config(serde_json::json!({
        "problems": [{ "level": "warning", "source": "init.lua:2", "message": "silo.action: dev replaces an earlier one" }],
        "bindings": [
            { "keys": "cmd-e", "target": command("Nope"), "source": "init.lua:4" },
            { "keys": "cmd-e", "target": { "type": "macro", "steps": [] }, "source": "init.lua:5" },
            { "keys": "cmd-", "target": command("NewTab"), "source": "keys.lua:1" },
            { "keys": "cmd-y", "target": command("NewTab") },
            { "keys": "cmd-y", "target": command("SplitDown") },
        ],
    }));
    let keymap = with_config(merge(&Overrides::new()), Some(&config));
    assert_eq!(
        keymap.problems,
        [
            "init.lua:2: silo.action: dev replaces an earlier one",
            "init.lua:4: Unknown command “Nope”",
            "init.lua:5: ⌘E: this version of Silo can't run what it's bound to",
            "keys.lua:1: “cmd-” isn't a shortcut",
            "⌘Y is bound twice; the last one wins",
        ]
    );
    assert_eq!(keymap.config.len(), 1);
    assert_eq!(keymap.shortcuts(Command::SplitDown)[0], sequence("cmd-y"));
}
