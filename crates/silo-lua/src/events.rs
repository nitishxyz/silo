//! Events as Lua hears them: every server event by its name, plus the
//! events worked out from `sessions.changed`, `session.changed` and
//! `block.changed` (`docs/LUA.md` §4.5) by diffing against what was seen.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use silo_proto::{
    control::Event,
    model::{BlockInfo, Lifecycle, SessionSummary, SessionView},
};

/// Where an event happened: `ctx` of a handler.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventCtx {
    pub session_id: Option<String>,
    pub window_id: Option<String>,
    pub block_id: Option<String>,
}

/// One event as Lua hears it.
#[derive(Clone, Debug, PartialEq)]
pub struct Heard {
    pub name: String,
    pub ev: Value,
    pub ctx: EventCtx,
}

#[derive(Clone, Debug, Default)]
struct SessionSnap {
    name: String,
    windows: Vec<String>,
    blocks: HashMap<String, BlockSnap>,
}

#[derive(Clone, Debug)]
struct BlockSnap {
    kind: Value,
    window: Option<String>,
    cwd: Option<String>,
    title: String,
    exited: bool,
}

impl BlockSnap {
    fn of(block: &BlockInfo, window: Option<String>) -> Self {
        Self {
            kind: serde_json::to_value(block.kind).unwrap_or(Value::Null),
            window,
            cwd: block.cwd.clone(),
            title: block.title.clone(),
            exited: block.lifecycle == Lifecycle::Exited,
        }
    }
}

/// What has been seen of the sessions, to tell what changed.
#[derive(Debug, Default)]
pub struct Tracker {
    sessions: HashMap<String, SessionSnap>,
    /// Closed sessions: their late events are not a new session.
    closed: HashSet<String>,
}

impl Tracker {
    /// Know of the sessions in `list`, without events.
    pub fn seed_list(&mut self, list: &[SessionSummary]) {
        for s in list {
            self.sessions.entry(s.id.clone()).or_default().name.clone_from(&s.name);
        }
    }

    /// Know `view` as it is, without events.
    pub fn seed_view(&mut self, view: &SessionView) {
        let mut out = Vec::new();
        self.apply_view(view, &mut out);
    }

    /// The session, window and block `event` is about, where it says or
    /// what was seen tells.
    fn ctx_of(&self, data: &Value) -> EventCtx {
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
        let mut ctx = EventCtx {
            session_id: text(data.get("session_id"))
                .or_else(|| text(data.pointer("/view/session/id")))
                .or_else(|| text(data.pointer("/block/session_id"))),
            window_id: text(data.get("window_id")),
            block_id: text(data.get("block_id")).or_else(|| text(data.pointer("/block/id"))).or_else(|| text(data.pointer("/status/block_id"))),
        };
        if let Some(block) = &ctx.block_id {
            let found = match &ctx.session_id {
                Some(s) => self.sessions.get(s).and_then(|snap| snap.blocks.get(block)).map(|b| (s.clone(), b)),
                None => self.sessions.iter().find_map(|(s, snap)| snap.blocks.get(block).map(|b| (s.clone(), b))),
            };
            if let Some((session, snap)) = found {
                ctx.session_id.get_or_insert(session);
                if ctx.window_id.is_none() {
                    ctx.window_id.clone_from(&snap.window);
                }
            }
        }
        ctx
    }

    /// `event` as Lua hears it: itself (by its name) first, then what it
    /// changed. The server's `session.closed` is folded into the derived
    /// one, which also has the name.
    pub fn handle(&mut self, event: &Event) -> Vec<Heard> {
        let raw = serde_json::to_value(event).unwrap_or(Value::Null);
        let name = raw.get("event").and_then(Value::as_str).unwrap_or_default().to_owned();
        let data = raw.get("data").cloned().unwrap_or_else(|| json!({}));
        let mut out = Vec::new();
        if let Event::SessionClosed { session_id } = event {
            self.close(session_id, &mut out);
            return out;
        }
        // Before applying it, so a closed block's event still finds its window.
        let ctx = self.ctx_of(&data);
        out.push(Heard { name, ev: data, ctx });
        match event {
            Event::SessionsChanged { sessions } => {
                for s in sessions {
                    if self.closed.contains(&s.id) {
                        continue;
                    }
                    match self.sessions.get_mut(&s.id) {
                        Some(snap) => snap.name.clone_from(&s.name),
                        None => {
                            self.sessions.insert(s.id.clone(), SessionSnap { name: s.name.clone(), ..SessionSnap::default() });
                            out.push(session_event("session.created", &s.id, &s.name));
                        }
                    }
                }
                let gone: Vec<String> = self.sessions.keys().filter(|id| !sessions.iter().any(|s| &s.id == *id)).cloned().collect();
                for id in gone {
                    self.close(&id, &mut out);
                }
            }
            Event::SessionChanged { view } => self.apply_view(view, &mut out),
            Event::BlockChanged { block } => {
                if let Some(snap) = self.sessions.get_mut(&block.session_id) {
                    match snap.blocks.get_mut(&block.id) {
                        Some(old) => {
                            let window = old.window.clone();
                            diff_block(&block.session_id, old, block, window, &mut out);
                        }
                        None => {
                            snap.blocks.insert(block.id.clone(), BlockSnap::of(block, None));
                            out.push(block_event(
                                "block.created",
                                &block.session_id,
                                &block.id,
                                None,
                                json!({"block_id": block.id, "kind": block.kind}),
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn close(&mut self, session_id: &str, out: &mut Vec<Heard>) {
        if !self.closed.insert(session_id.to_owned()) {
            return;
        }
        let name = self.sessions.remove(session_id).map(|s| s.name).unwrap_or_default();
        out.push(session_event("session.closed", session_id, &name));
    }

    fn apply_view(&mut self, view: &SessionView, out: &mut Vec<Heard>) {
        let session = &view.session;
        if self.closed.contains(&session.id) {
            return;
        }
        if !self.sessions.contains_key(&session.id) {
            out.push(session_event("session.created", &session.id, &session.name));
        }
        let snap = self.sessions.entry(session.id.clone()).or_default();
        snap.name.clone_from(&session.name);
        let windows: Vec<String> = session.windows.iter().map(|w| w.id.clone()).collect();
        let window_of: HashMap<String, String> =
            session.windows.iter().flat_map(|w| w.blocks().into_iter().map(move |b| (b, w.id.clone()))).collect();
        let sid = &session.id;
        for w in windows.iter().filter(|w| !snap.windows.contains(w)) {
            out.push(Heard {
                name: "window.created".into(),
                ev: json!({"window_id": w}),
                ctx: EventCtx { session_id: Some(sid.clone()), window_id: Some(w.clone()), block_id: None },
            });
        }
        for block in &view.blocks {
            let window = window_of.get(&block.id).cloned();
            match snap.blocks.get_mut(&block.id) {
                Some(old) => diff_block(sid, old, block, window, out),
                None => {
                    snap.blocks.insert(block.id.clone(), BlockSnap::of(block, window.clone()));
                    out.push(block_event("block.created", sid, &block.id, window, json!({"block_id": block.id, "kind": block.kind})));
                }
            }
        }
        let gone: Vec<String> = snap.blocks.keys().filter(|id| !view.blocks.iter().any(|b| &b.id == *id)).cloned().collect();
        for id in gone {
            if let Some(old) = snap.blocks.remove(&id) {
                out.push(block_event("block.closed", sid, &id, old.window, json!({"block_id": id, "kind": old.kind})));
            }
        }
        for w in snap.windows.iter().filter(|w| !windows.contains(w)) {
            out.push(Heard {
                name: "window.closed".into(),
                ev: json!({"window_id": w}),
                ctx: EventCtx { session_id: Some(sid.clone()), window_id: Some(w.clone()), block_id: None },
            });
        }
        snap.windows = windows;
    }
}

/// The block events `new` makes of `old`, which becomes `new`. `window`
/// is where it is now, when known.
fn diff_block(session: &str, old: &mut BlockSnap, new: &BlockInfo, window: Option<String>, out: &mut Vec<Heard>) {
    if window.is_some() {
        old.window = window;
    }
    let window = old.window.clone();
    let emit = |out: &mut Vec<Heard>, name: &str, ev: Value| out.push(block_event(name, session, &new.id, window.clone(), ev));
    if new.cwd.is_some() && new.cwd != old.cwd {
        emit(out, "block.cwd_changed", json!({"cwd": new.cwd}));
    }
    if new.title != old.title {
        emit(out, "block.title_changed", json!({"title": new.title}));
    }
    let exited = new.lifecycle == Lifecycle::Exited;
    if exited && !old.exited {
        emit(out, "block.exited", json!({"exit_code": new.exited}));
    }
    *old = BlockSnap { window: old.window.clone(), ..BlockSnap::of(new, None) };
}

fn session_event(name: &str, session_id: &str, session_name: &str) -> Heard {
    Heard {
        name: name.into(),
        ev: json!({"session_id": session_id, "name": session_name}),
        ctx: EventCtx { session_id: Some(session_id.to_owned()), ..EventCtx::default() },
    }
}

fn block_event(name: &str, session: &str, block: &str, window: Option<String>, ev: Value) -> Heard {
    Heard { name: name.into(), ev, ctx: EventCtx { session_id: Some(session.to_owned()), window_id: window, block_id: Some(block.to_owned()) } }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn block(id: &str, session: &str, cwd: &str, title: &str, exited: Option<i32>) -> Value {
        json!({
            "id": id, "session_id": session, "kind": "terminal", "title": title, "cwd": cwd,
            "exited": exited, "lifecycle": if exited.is_some() { "exited" } else { "running" },
            "size": {"columns": 80, "rows": 24}, "owner_name": null,
        })
    }

    /// A session with window `w1` holding `blocks` side by side.
    pub fn view(session: &str, blocks: &[Value]) -> Value {
        let ids: Vec<&str> = blocks.iter().map(|b| b["id"].as_str().unwrap()).collect();
        let mut layout = json!({"type": "block", "id": ids[0]});
        for id in &ids[1..] {
            layout = json!({"type": "split", "axis": "horizontal", "ratio": 0.5, "first": layout, "second": {"type": "block", "id": id}});
        }
        json!({
            "session": {
                "id": session, "name": "work", "active_window": "w1", "revision": 1, "created_at": 0,
                "windows": [{"id": "w1", "layout": layout, "focused": ids[0], "floats": []}],
            },
            "blocks": blocks,
        })
    }

    pub fn event(name: &str, data: Value) -> Event {
        serde_json::from_value(json!({"event": name, "data": data})).unwrap()
    }

    fn names(heard: &[Heard]) -> Vec<&str> {
        heard.iter().map(|h| h.name.as_str()).collect()
    }

    #[test]
    fn block_changes_are_derived_with_their_context() {
        let mut t = Tracker::default();
        let seed: SessionView = serde_json::from_value(view("s1", &[block("b1", "s1", "/a", "zsh", None)])).unwrap();
        t.seed_view(&seed);

        let heard = t.handle(&event("block.changed", json!({"block": block("b1", "s1", "/b", "vim", None)})));
        assert_eq!(names(&heard), ["block.changed", "block.cwd_changed", "block.title_changed"]);
        assert_eq!(heard[1].ev, json!({"cwd": "/b"}));
        let ctx = EventCtx { session_id: Some("s1".into()), window_id: Some("w1".into()), block_id: Some("b1".into()) };
        assert_eq!(heard[1].ctx, ctx);
        assert_eq!(heard[0].ctx, ctx, "a raw event gets the window too");

        let heard = t.handle(&event("block.changed", json!({"block": block("b1", "s1", "/b", "vim", Some(2))})));
        assert_eq!(names(&heard), ["block.changed", "block.exited"]);
        assert_eq!(heard[1].ev, json!({"exit_code": 2}));
        let again = t.handle(&event("block.changed", json!({"block": block("b1", "s1", "/b", "vim", Some(2))})));
        assert_eq!(names(&again), ["block.changed"], "nothing changed");

        let bell = t.handle(&event("block.bell", json!({"block_id": "b1"})));
        assert_eq!(bell[0].ctx, ctx);
    }

    #[test]
    fn sessions_windows_and_blocks_come_and_go() {
        let mut t = Tracker::default();
        let list = |ids: &[&str]| {
            let sessions: Vec<Value> = ids
                .iter()
                .map(|id| json!({"id": id, "name": format!("n{id}"), "windows": 1, "blocks": 1, "attached_clients": 0, "created_at": 0}))
                .collect();
            event("sessions.changed", json!({"sessions": sessions}))
        };
        let heard = t.handle(&list(&["s1"]));
        assert_eq!(names(&heard), ["sessions.changed", "session.created"]);
        assert_eq!(heard[1].ev, json!({"session_id": "s1", "name": "ns1"}));

        let heard = t.handle(&event("session.changed", json!({"view": view("s1", &[block("b1", "s1", "/a", "zsh", None)])})));
        assert_eq!(names(&heard), ["session.changed", "window.created", "block.created"]);
        assert_eq!(heard[2].ev, json!({"block_id": "b1", "kind": "terminal"}));
        assert_eq!(heard[2].ctx.window_id.as_deref(), Some("w1"));

        let two = view("s1", &[block("b1", "s1", "/a", "zsh", None), block("b2", "s1", "/a", "zsh", None)]);
        assert_eq!(names(&t.handle(&event("session.changed", json!({"view": two})))), ["session.changed", "block.created"]);
        let one = view("s1", &[block("b2", "s1", "/a", "zsh", None)]);
        let heard = t.handle(&event("session.changed", json!({"view": one})));
        assert_eq!(names(&heard), ["session.changed", "block.closed"]);
        assert_eq!(heard[1].ctx.block_id.as_deref(), Some("b1"));

        let heard = t.handle(&event("session.closed", json!({"session_id": "s1"})));
        assert_eq!(names(&heard), ["session.closed"]);
        assert_eq!(heard[0].ev, json!({"session_id": "s1", "name": "work"}));
        assert_eq!(names(&t.handle(&list(&[]))), ["sessions.changed"], "closed once");
    }
}
