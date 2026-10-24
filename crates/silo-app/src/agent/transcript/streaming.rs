//! Tool calls shown while the model still writes them (`tool.input_start`, `tool.input_delta`), and a compaction
//! as its summary streams (`compaction.started`, `compaction.delta`).

use otto_client::SessionMirror;
use otto_proto::Event;
use serde_json::json;

use super::*;

struct Session {
    m: SessionMirror,
    seq: i64,
    /// Observed after every event, as the pane does: first-seen order.
    times: Times,
}

impl Session {
    /// A running turn with an assistant item streaming (`a`), as a pane follows it; `streaming`: calls the view
    /// held still being written (a session loaded mid-call).
    fn new(streaming: Value) -> Self {
        let view = serde_json::from_value(json!({"info":{"id":"s","project_id":"p","agent":"a","provider":"p","model":"m","title":"t","cwd":"/work/app","status":"busy","created_at":0,"updated_at":0,"last_seq":1,"usage":{"usage":{"input":0,"output":0},"cost_usd":0.0}},"items":[serde_json::to_value(Item::user_text("u", "go")).unwrap()],"pending_approvals":[],"pending_questions":[],"queue":[],"running_tool_calls":[],"streaming_tool_calls":streaming,"seq":1})).unwrap();
        let mut s = Self { m: SessionMirror::new(view), seq: 1, times: Times::live() };
        s.event(Event::TurnStarted { turn: 1 });
        s.event(Event::ItemStarted { item_id: "a".into(), role: Role::Assistant });
        s
    }

    fn event(&mut self, event: Event) {
        self.seq += 1;
        self.m.apply(self.seq, &event);
        let running = self.m.turn.is_some_and(|t| t.1 == TurnStatus::Running);
        self.times.observe(&self.m.items, &self.m.tools, running, HashMap::new(), u64::try_from(self.seq).unwrap_or(0));
    }

    fn start(&mut self, call: &str, name: &str) {
        self.event(Event::ToolInputStart { call_id: call.into(), name: name.into() });
    }

    fn delta(&mut self, call: &str, delta: &str) {
        self.event(Event::ToolInputDelta { call_id: call.into(), delta: delta.into() });
    }

    /// The model finished the call (its item done), otto runs it, then its result.
    fn call(&mut self, call: &str, name: &str, input: &Value) {
        let part = Part::ToolCall { call_id: call.into(), name: name.into(), input: input.clone(), kind: otto_llm::CallKind::Function, opaque: None };
        self.event(Event::ItemDone { item: Item::new("a", Role::Assistant, vec![part]) });
        self.event(Event::ToolCall { call_id: call.into(), name: name.into(), input: input.clone() });
    }

    fn result(&mut self, call: &str, text: &str) {
        let output = json!({"content": [{"type": "text", "text": text}], "is_error": false});
        self.event(Event::ToolResult { call_id: call.into(), is_error: false, output, item_id: "r".into() });
    }

    fn rows(&self) -> Vec<Row> {
        let times = &self.times;
        let m = &self.m;
        build(&Input {
            items: &m.items,
            tools: &m.tools,
            turn: m.turn.map(|t| t.1),
            error: None,
            retrying: None,
            compactions: &m.compactions,
            cwd: Some("/work/app"),
            home: None,
            times,
            now: 0,
        })
    }

    /// The live group's key and its line for `call`, if shown.
    fn line(&self, call: &str) -> Option<(String, Line)> {
        self.rows().into_iter().find_map(|r| match r {
            Row::Group(g) => g.lines.iter().find(|l| l.key == call).cloned().map(|l| (g.key.clone(), l)),
            _ => None,
        })
    }

    /// Feed `input`'s JSON in `step`-byte deltas (at char boundaries), checking each row on the way.
    fn stream(&mut self, call: &str, input: &str, step: usize, mut check: impl FnMut(&str, Option<&Line>)) {
        let mut at = 0;
        while at < input.len() {
            let mut end = (at + step).min(input.len());
            while !input.is_char_boundary(end) {
                end += 1;
            }
            self.delta(call, &input[at..end]);
            at = end;
            check(&input[..at], self.line(call).map(|(_, l)| l).as_ref());
        }
    }
}

fn diff(line: &Line) -> &str {
    match &line.detail {
        Some(Detail::Diff(d)) => d,
        other => panic!("no diff: {other:?}"),
    }
}

/// The streaming row is the row the call shows once called, but for the preview mark.
fn settles_into(streaming: &Line, called: &Line) {
    assert!(streaming.preview && !called.preview);
    assert_eq!(Line { preview: false, ..streaming.clone() }, *called);
}

#[test]
fn a_write_shows_as_it_streams_and_settles_into_its_called_row() {
    let mut s = Session::new(json!([]));
    s.start("c", "write");
    let (group, first) = s.line("c").expect("a row from tool.input_start");
    assert_eq!((first.title.as_str(), first.kind, first.status, first.detail.as_ref()), ("Writing", Kind::Edit, Status::Running, None));
    let input = json!({"path": "/work/app/src/a.rs", "content": "fn a() {\n    \"x\"\n}\n"}).to_string();
    let mut last_lines = 0;
    s.stream("c", &input, 5, |sent, line| {
        let line = line.expect("the row stays");
        if sent.contains("\"content\"") {
            assert_eq!(line.title, "Writing src/a.rs", "at {sent}");
        }
        if let Some(Detail::Diff(d)) = &line.detail {
            assert!(line.preview, "a live preview at {sent}");
            assert!(d.lines().all(|l| l.starts_with('+')), "{d}");
            assert!(d.lines().count() >= last_lines, "lines only arrive");
            last_lines = d.lines().count();
        }
    });
    let (_, streamed) = s.line("c").unwrap();
    assert_eq!(diff(&streamed), "+fn a() {\n+    \"x\"\n+}");
    assert_eq!(streamed.desc.as_deref(), Some("+3"));
    s.call("c", "write", &serde_json::from_str(&input).unwrap());
    let (called_group, called) = s.line("c").unwrap();
    assert_eq!(called_group, group, "the same group throughout");
    settles_into(&streamed, &called);
    s.result("c", "Wrote 3 lines");
    let (_, done) = s.line("c").unwrap();
    assert_eq!((done.title.as_str(), done.status, done.preview), ("Wrote src/a.rs", Status::Done, false));
}

#[test]
fn an_edit_builds_its_diff_live() {
    let mut s = Session::new(json!([]));
    s.start("c", "edit");
    let input = r#"{"path":"src/a.rs","old_string":"let a = 1;\nlet b = 2;","new_string":"let a = 10;\nlet b = 20;"}"#;
    let cuts = [r#"{"path":"src/a.rs","#, r#""old_string":"let a = 1;\nlet b"#, r#" = 2;","new_string":"let a = 10"#];
    s.delta("c", cuts[0]);
    assert_eq!((s.line("c").unwrap().1.title.as_str(), s.line("c").unwrap().1.detail), ("Editing src/a.rs", None));
    s.delta("c", cuts[1]);
    assert_eq!(diff(&s.line("c").unwrap().1), "-let a = 1;\n-let b");
    s.delta("c", cuts[2]);
    assert_eq!(diff(&s.line("c").unwrap().1), "-let a = 1;\n-let b = 2;\n+let a = 10");
    s.delta("c", &input[cuts.concat().len()..]);
    let (_, streamed) = s.line("c").unwrap();
    assert_eq!(diff(&streamed), "-let a = 1;\n-let b = 2;\n+let a = 10;\n+let b = 20;");
    s.call("c", "edit", &serde_json::from_str(input).unwrap());
    settles_into(&streamed, &s.line("c").unwrap().1);
}

#[test]
fn a_multiedit_and_a_patch_preview_too() {
    let mut s = Session::new(json!([]));
    s.start("m", "multiedit");
    let input = r#"{"path":"a.rs","edits":[{"old_string":"x","new_string":"y"},{"old_string":"p","new_string":"q"}]}"#;
    let cut = input.find("\"q\"").unwrap() + 1;
    s.stream("m", &input[..cut], 7, |_, _| {});
    assert_eq!(diff(&s.line("m").unwrap().1), "-x\n+y\n-p", "an empty new string adds no line yet");
    s.delta("m", &input[cut..]);
    let (_, streamed) = s.line("m").unwrap();
    s.call("m", "multiedit", &serde_json::from_str(input).unwrap());
    settles_into(&streamed, &s.line("m").unwrap().1);
    // A custom tool streams its text, not JSON.
    let mut s = Session::new(json!([]));
    s.start("p", "apply_patch");
    let patch = "*** Begin Patch\n*** Update File: src/b.rs\n@@\n-old\n+new\n*** End Patch";
    s.stream("p", patch, 9, |_, _| {});
    let (_, streamed) = s.line("p").unwrap();
    assert_eq!((streamed.title.as_str(), streamed.desc.as_deref(), streamed.preview), ("Editing src/b.rs", Some("+1 \u{2212}1"), true));
    s.call("p", "apply_patch", &json!(patch));
    settles_into(&streamed, &s.line("p").unwrap().1);
}

#[test]
fn a_path_names_the_row_once_whole() {
    let mut s = Session::new(json!([]));
    s.start("c", "write");
    s.delta("c", r#"{"path":"src/a."#);
    assert_eq!(s.line("c").unwrap().1.title, "Writing");
    s.delta("c", r#"rs","#);
    assert_eq!(s.line("c").unwrap().1.title, "Writing src/a.rs");
}

#[test]
fn every_tool_shows_from_its_start_and_keeps_its_row_through_call_and_result() {
    let mut s = Session::new(json!([]));
    s.start("c", "shell");
    let (group, line) = s.line("c").expect("named, nothing streamed yet");
    assert_eq!((line.title.as_str(), line.kind, line.status, line.preview), ("Running", Kind::Shell, Status::Running, false));
    s.delta("c", r#"{"command":"cargo te"#);
    assert_eq!(s.line("c").unwrap().1.title, "Running cargo te", "the subject fills in as it arrives");
    s.delta("c", r#"st -p app"}"#);
    let (_, streamed) = s.line("c").unwrap();
    assert_eq!((streamed.title.as_str(), streamed.detail.as_ref()), ("Running cargo test -p app", None));
    s.call("c", "shell", &json!({"command": "cargo test -p app"}));
    let (called_group, called) = s.line("c").unwrap();
    assert_eq!((called_group, called.title.as_str()), (group.clone(), "Running cargo test -p app"));
    s.result("c", "ok");
    let (done_group, done) = s.line("c").unwrap();
    assert_eq!((done_group, done.title.as_str(), done.status), (group, "Ran cargo test -p app", Status::Done));
}

#[test]
fn a_call_without_arguments_shows_from_its_start() {
    let mut s = Session::new(json!([]));
    // Ids against start order: the rows follow when calls started, not their ids or a hash.
    s.start("d", "ls");
    assert_eq!(s.line("d").unwrap().1.title, "Listing files");
    s.start("c", "browser_snapshot");
    let titles: Vec<String> = s
        .rows()
        .iter()
        .filter_map(|r| if let Row::Group(g) = r { Some(g) } else { None })
        .flat_map(|g| g.lines.iter().map(|l| l.title.clone()))
        .collect();
    assert_eq!(titles, ["Listing files", "Reading page"], "in the order they started");
    s.call("d", "ls", &json!({}));
    assert_eq!(s.line("d").unwrap().1.title, "Listing files");
}

#[test]
fn without_input_start_edits_show_once_their_keys_say_which_and_the_rest_at_their_call() {
    let mut s = Session::new(json!([]));
    s.delta("c", r#"{"path":"a.rs","#);
    assert!(s.line("c").is_none(), "nothing says which tool yet");
    s.delta("c", r#""content":"x\n"#);
    let (_, line) = s.line("c").expect("path and content: a write");
    assert_eq!((line.title.as_str(), diff(&line)), ("Writing a.rs", "+x"));
    s.delta("c", r#""}"#);
    let (_, streamed) = s.line("c").unwrap();
    s.call("c", "write", &json!({"path": "a.rs", "content": "x\n"}));
    settles_into(&streamed, &s.line("c").unwrap().1);
    let mut s = Session::new(json!([]));
    s.delta("e", r#"{"path":"a.rs","old_string":"a"#);
    assert_eq!(s.line("e").unwrap().1.title, "Editing a.rs");
    s.delta("d", r#"{"command":"ls"}"#);
    assert!(s.line("d").is_none(), "an unnamed shell call waits for its call");
    s.event(Event::ToolCall { call_id: "d".into(), name: "shell".into(), input: json!({"command": "ls"}) });
    assert_eq!(s.line("d").unwrap().1.title, "Running ls");
}

#[test]
fn a_session_loaded_mid_call_shows_the_input_so_far_and_goes_on() {
    let mut s = Session::new(json!([{"call_id": "c", "name": "edit", "input_delta": "{\"path\":\"a.rs\",\"old_string\":\"x\",\"new_string\":\"y"}]));
    let (_, line) = s.line("c").expect("shown from the view");
    assert_eq!((line.title.as_str(), diff(&line), line.preview), ("Editing a.rs", "-x\n+y", true));
    s.delta("c", "z\"}");
    assert_eq!(diff(&s.line("c").unwrap().1), "-x\n+yz");
}

#[test]
fn a_finished_turn_shows_no_streaming_call_as_running() {
    let mut s = Session::new(json!([]));
    s.start("c", "write");
    s.delta("c", r#"{"path":"a.rs","content":"x"#);
    s.event(Event::TurnFinished { turn: 1, status: TurnStatus::Cancelled, usage: otto_llm::Usage::default() });
    let (_, line) = s.line("c").unwrap();
    assert_eq!((line.status, line.preview), (Status::Stopped, false));
}

#[test]
fn an_auto_compaction_streams_in_the_thread_and_folds_once_stored() {
    let mut s = Session::new(json!([]));
    s.call("c1", "read", &json!({"path": "a.rs"}));
    s.result("c1", "fn a() {}");
    s.event(Event::CompactionStarted { id: "k".into(), trigger: "auto".into() });
    for text in ["## Goal\n", "Ship the ", "form."] {
        assert_eq!(s.m.apply_live(s.seq, &Event::CompactionDelta { id: "k".into(), text: text.into() }), otto_client::ApplyResult::Applied);
    }
    let rows = s.rows();
    let Some(Row::Compaction(c)) = rows.last() else { panic!("{rows:?}") };
    assert_eq!((c.title(), c.summary.as_str()), ("Compacting context (auto)".into(), "## Goal\nShip the form."));
    assert!(indicator_is_group(&rows));
    let checkpoint = Item::new("k", Role::System, vec![Part::Compaction { summary: Some("## Goal\nShip the form.".into()), opaque: None }]);
    s.event(Event::ItemDone { item: checkpoint });
    s.event(Event::Compaction { through_seq: 4, summary: Some("## Goal\nShip the form.".into()), id: Some("k".into()) });
    let rows = s.rows();
    let Some(Row::Compaction(c)) = rows.last() else { panic!("{rows:?}") };
    assert_eq!((c.key.as_str(), c.title()), ("k#compaction", "Context compacted · 2 messages summarised".into()));
}
