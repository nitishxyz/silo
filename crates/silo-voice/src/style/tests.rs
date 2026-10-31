use super::*;

fn text(input: &str) -> String {
    format(input, &[], &Style { multiline: true, lists: true, vocabulary: vec!["otto".into()] })
}

fn line(input: &str) -> String {
    format(input, &[], &Style::default())
}

fn rule(heard: &str, write: &str) -> Replacement {
    Replacement { heard: heard.into(), write: write.into() }
}

#[test]
fn replacements_are_whole_words_keeping_case() {
    let rules = [rule("auto", "otto"), rule("next test", "nextest"), rule("get hub", "GitHub")];
    assert_eq!(replace("ask auto about automatic autos", &rules), "ask otto about automatic autos");
    assert_eq!(replace("Auto, run next  test. AUTO!", &rules), "Otto, run nextest. OTTO!");
    assert_eq!(replace("open get hub; next, test", &rules), "open GitHub; next, test", "punctuation splits a phrase");
    assert_eq!(replace("auto-complete auto's", &rules), "auto-complete otto's");
    assert_eq!(replace("ünïcode auto", &[rule("ÜNÏCODE", "x")]), "x auto");
    assert_eq!(replace("keep this", &[rule("  ", "x"), rule("a.b", "x")]), "keep this", "no word, or not words");
}

#[test]
fn spoken_breaks_and_lists() {
    assert_eq!(text("Hello. New line. World."), "Hello.\nWorld.");
    assert_eq!(text("first part new paragraph second part"), "first part\n\nsecond part");
    assert_eq!(text("groceries bullet milk next bullet eggs"), "groceries\n- milk\n- eggs");
    assert_eq!(text("steps numbered list open it next bullet close it"), "steps\n1. open it\n2. close it");
    assert_eq!(text("todo bullet"), "todo", "no empty item at the end");
    assert_eq!(line("hello new line world bullet again"), "hello world again", "one line in a terminal");
}

#[test]
fn command_words_said_plainly_stay() {
    for plain in [
        "add a new line to the file",
        "the new line character",
        "insert new line here",
        "use a comma separated list",
        "for a short period",
        "the trial period ends today",
        "the question mark is missing",
        "use bullet points",
        "the Oxford comma",
        "make a numbered list",
        "I scratch that itch",
        "you can scratch that idea",
        "comma",
    ] {
        assert_eq!(text(plain), plain);
    }
}

#[test]
fn spoken_punctuation() {
    assert_eq!(text("hello comma how are you question mark"), "hello, how are you?");
    assert_eq!(text("Hello comma, how are you question mark?"), "Hello, how are you?");
    assert_eq!(text("that's it period"), "that's it.");
    assert_eq!(text("done period Next one"), "done. Next one");
    assert_eq!(text("wow exclamation mark nice"), "wow! Nice");
}

#[test]
fn scratch_that_drops_the_last_sentence() {
    assert_eq!(text("Send it to Bob. Scratch that. Send it to Alice."), "Send it to Alice.");
    assert_eq!(text("Hi there. Call Bob tomorrow. Scratch that."), "Hi there.");
    assert_eq!(text("list bullet milk bullet eggs, scratch that bullet bread"), "list\n- milk\n- bread");
    assert_eq!(text("ok. Scratch that."), "");
    assert_eq!(text("numbered list milk next bullet eggs, scratch that. Next bullet bread"), "1. milk\n2. bread");
}

#[test]
fn ordinals_make_numbered_lists() {
    assert_eq!(
        text("Here's the plan. First, open the file. Second, run the tests, and third, commit the change. Then we're done."),
        "Here's the plan.\n1. Open the file\n2. Run the tests\n3. Commit the change\n\nThen we're done."
    );
    assert_eq!(text("first open the file second run the tests third commit"), "1. Open the file\n2. Run the tests\n3. Commit");
    assert_eq!(
        text("First of all, otto reviews, second, wait, and finally, merge"),
        "1. otto reviews\n2. Wait\n3. Merge",
        "vocabulary keeps its case"
    );
    assert_eq!(text("I need firstly milk secondly eggs thirdly bread"), "I need:\n1. Milk\n2. Eggs\n3. Bread");
}

#[test]
fn numbers_make_numbered_lists_only_at_clause_starts() {
    assert_eq!(text("One, open the file. Two, run the tests. Three, commit the change."), "1. Open the file\n2. Run the tests\n3. Commit the change");
    assert_eq!(text("1. Open the file 2. Run the tests 3. Commit it"), "1. Open the file\n2. Run the tests\n3. Commit it");
    assert_eq!(text("Step one open the file step two run tests step three commit it"), "1. Open the file\n2. Run tests\n3. Commit it");
}

#[test]
fn bulleted_enumerations() {
    assert_eq!(text("I need three things: milk, eggs, and bread."), "I need three things:\n- Milk\n- Eggs\n- Bread");
    assert_eq!(text("A few things, the docs, the tests or the build. Thanks."), "A few things:\n- The docs\n- The tests\n- The build\n\nThanks.");
}

#[test]
fn no_lists_where_there_are_none() {
    for plain in [
        "one of two things",
        "one of two things and three more",
        "I have one dog, two cats and three fish",
        "One, two, three, four",
        "the first time and the second time and the third",
        "My first car was red, my second was blue and my third was green.",
        "I came in first, then second, then third.",
        "first come first served",
        "At first I thought so. Second, it failed.",
        "wait a second, first things first, third time lucky",
        "Note: the build, the tests and the docs are done.",
        "I need two things: milk and eggs.",
        "Things: a very long first item that goes on, b, and c",
    ] {
        assert_eq!(text(plain), plain);
    }
    assert_eq!(format("first a second b third c", &[], &Style { multiline: true, lists: false, vocabulary: vec![] }), "first a second b third c");
    assert_eq!(line("first a second b third c"), "first a second b third c", "no lists in a terminal");
}

#[test]
fn the_whole_chain() {
    let rules = [rule("auto", "otto")];
    let style = Style { multiline: true, lists: true, vocabulary: vec!["otto".into()] };
    assert_eq!(format("first ask auto second wait third merge", &rules, &style), "1. Ask otto\n2. Wait\n3. Merge");
    assert_eq!(format("Auto new line done", &rules, &Style::default()), "Otto done");
}
