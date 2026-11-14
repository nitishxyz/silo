use super::*;

fn modes() -> Vec<KeyMode> {
    serde_json::from_value(serde_json::json!([{ "name": "resize", "exclusive": true }, { "name": "peek" }])).unwrap()
}

fn names(modes: &Modes) -> Vec<&str> {
    modes.stack.iter().map(|e| e.name.as_str()).collect()
}

#[test]
fn modes_stack_and_leave() {
    let known = modes();
    let mut stack = Modes::default();
    assert_eq!(stack.enter("nope", false, &known), Err("No mode named “nope”".into()));
    stack.enter("resize", false, &known).unwrap();
    stack.enter("peek", false, &known).unwrap();
    assert_eq!(names(&stack), ["resize", "peek"]);
    stack.enter("resize", false, &known).unwrap();
    assert_eq!(names(&stack), ["peek", "resize"], "entering again moves it to the top");
    assert!(stack.exit(false));
    assert_eq!(stack.top().map(|t| t.name.as_str()), Some("peek"));
    stack.enter("resize", false, &known).unwrap();
    assert!(stack.exit(true));
    assert!(stack.top().is_none());
    assert!(!stack.exit(false) && !stack.exit(true), "nothing to leave");
}

#[test]
fn a_once_mode_leaves_after_its_next_binding() {
    let known = modes();
    let mut stack = Modes::default();
    stack.enter("peek", true, &known).unwrap();
    assert!(!stack.ran(None), "a binding outside modes leaves it on");
    assert!(stack.ran(Some("peek")));
    assert!(stack.top().is_none());
    stack.enter("peek", false, &known).unwrap();
    assert!(!stack.ran(Some("peek")), "not once: it stays");
    assert!(stack.top().is_some());
}

#[test]
fn only_the_top_mode_is_on_and_exclusive_turns_the_rest_off() {
    let known = modes();
    let mut stack = Modes::default();
    let context = stack.context(&known);
    assert!(context.contains("Workspace") && context.contains("silo_off_resize") && context.contains("silo_off_peek"));
    assert!(!context.contains(keymap::EXCLUSIVE));
    stack.enter("resize", false, &known).unwrap();
    let context = stack.context(&known);
    assert!(!context.contains("silo_off_resize") && context.contains("silo_off_peek"));
    assert!(context.contains(keymap::EXCLUSIVE));
    stack.enter("peek", false, &known).unwrap();
    let context = stack.context(&known);
    assert!(context.contains("silo_off_resize") && !context.contains("silo_off_peek") && !context.contains(keymap::EXCLUSIVE));
}

#[test]
fn escape_leaves_unless_bound_and_exclusive_swallows() {
    let known = modes();
    let mut stack = Modes::default();
    assert_eq!(stack.key(true, false, &known), Key::Pass, "no mode: Escape is the terminal's");
    stack.enter("peek", false, &known).unwrap();
    assert_eq!(stack.key(true, false, &known), Key::Leave);
    assert_eq!(stack.key(true, true, &known), Key::Pass, "the mode binds Escape (its binding ran first)");
    assert_eq!(stack.key(false, false, &known), Key::Pass, "not exclusive: other keys work as usual");
    stack.enter("resize", false, &known).unwrap();
    assert_eq!(stack.key(false, false, &known), Key::Swallow);
    assert_eq!(stack.key(true, false, &known), Key::Leave);
}

#[test]
fn blocked_keys_do_nothing_in_their_mode() {
    let config: silo_proto::config::ConfigState = serde_json::from_value(serde_json::json!({
        "modes": [{ "name": "peek" }],
        "unbinds": [{ "keys": "left", "mode": "peek" }],
    }))
    .unwrap();
    let keymap = keymap::with_config(keymap::merge(&keymap::Overrides::new()), Some(&config));
    let blocked = keymap.specs().into_iter().find(|s| s.keys == "left").unwrap();
    assert_eq!(blocked.when, keymap::When::Mode("peek".into()), "only in its mode");
    let keymap::Does::Run(index) = blocked.does else { panic!() };
    assert_eq!(keymap.config[index].target, keymap::Target::Blocked, "a handled action that does nothing: the key stops there");
}

#[test]
fn modes_the_config_dropped_are_left() {
    let mut stack = Modes::default();
    stack.enter("resize", false, &modes()).unwrap();
    assert!(!stack.forget_unknown(&modes()));
    assert!(stack.forget_unknown(&[]));
    assert!(stack.top().is_none());
}
