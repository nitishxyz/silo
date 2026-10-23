//! Agent actions on a pane's CEF page, using in-process CDP.

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use gpui::{BackgroundExecutor, Context, Task};
use serde::Deserialize;
use serde_json::{Value, json};
use silo_web::{
    Browser, DevToolsEvent,
    snapshot::{AxNode, Options, Refs, Snapshot},
};
use tokio::sync::Mutex;

/// How long after input a main-frame navigation may take to start.
const NAVIGATION_START: Duration = Duration::from_millis(300);
/// How long after input a started navigation may take to commit and parse.
const NAVIGATION_SETTLE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Default, Deserialize)]
struct Action {
    #[serde(default)]
    include_snapshot: bool,
}

#[derive(Deserialize)]
struct Field {
    target: String,
    value: String,
}

#[derive(Default, Deserialize)]
struct Filter {
    since: Option<u64>,
    limit: Option<u32>,
    level: Option<String>,
    #[serde(default)]
    failed_only: bool,
}

#[derive(Deserialize)]
#[serde(tag = "method", content = "params")]
enum Request {
    #[serde(rename = "browser.snapshot")]
    Snapshot {
        within: Option<String>,
        depth: Option<u32>,
        #[serde(default)]
        boxes: bool,
    },
    #[serde(rename = "browser.find")]
    Find { text: Option<String>, regex: Option<String> },
    #[serde(rename = "browser.click")]
    Click {
        target: String,
        #[serde(default)]
        double: bool,
        button: Option<Button>,
        #[serde(flatten)]
        action: Action,
    },
    #[serde(rename = "browser.hover")]
    Hover {
        target: String,
        #[serde(flatten)]
        action: Action,
    },
    #[serde(rename = "browser.fill")]
    Fill {
        target: String,
        value: String,
        #[serde(flatten)]
        action: Action,
    },
    #[serde(rename = "browser.fill_form")]
    FillForm {
        fields: Vec<Field>,
        #[serde(flatten)]
        action: Action,
    },
    #[serde(rename = "browser.type")]
    Type {
        text: String,
        submit: Option<String>,
        #[serde(flatten)]
        action: Action,
    },
    #[serde(rename = "browser.press")]
    Press {
        key: String,
        #[serde(flatten)]
        action: Action,
    },
    #[serde(rename = "browser.scroll")]
    Scroll {
        target: Option<String>,
        dx: Option<f64>,
        dy: Option<f64>,
        #[serde(flatten)]
        action: Action,
    },
    #[serde(rename = "browser.wait_for")]
    WaitFor {
        text: Option<Vec<String>>,
        #[serde(default)]
        gone: bool,
        timeout_ms: Option<u64>,
    },
    #[serde(rename = "browser.dialog")]
    Dialog {
        accept: bool,
        prompt_text: Option<String>,
        #[serde(flatten)]
        action: Action,
    },
    #[serde(rename = "browser.eval")]
    Eval {
        function: String,
        #[serde(default)]
        args: Vec<String>,
    },
    #[serde(rename = "browser.console")]
    Console(Filter),
    #[serde(rename = "browser.network")]
    Network(Filter),
    #[serde(rename = "browser.screenshot")]
    Screenshot {
        target: Option<String>,
        #[serde(default)]
        full_page: bool,
        format: Option<Format>,
        quality: Option<u32>,
    },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Format {
    Png,
    Jpeg,
}

#[derive(Default)]
struct State {
    snapshot: Snapshot,
    /// This document's refs; a node keeps its ref until navigation.
    refs: Refs,
    generation: u64,
    dialog: Option<Value>,
    main_frame: Option<String>,
    console: VecDeque<Value>,
    network: VecDeque<(String, Value)>,
    console_id: u64,
    network_id: u64,
}

fn at() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

impl State {
    fn console(&mut self, level: &str, text: String, url: Option<&Value>, line: Option<&Value>) {
        // Silo's own page scripts report to the engine through the console.
        if text.starts_with("\u{1}silo-") {
            return;
        }
        self.console_id += 1;
        let mut message = json!({"id": self.console_id, "level": level, "text": text, "at": at()});
        if let Some(url) = url {
            message["url"] = url.clone();
        }
        if let Some(line) = line {
            message["line"] = line.clone();
        }
        self.console.push_back(message);
        if self.console.len() > 500 {
            self.console.pop_front();
        }
    }

    fn event(&mut self, event: DevToolsEvent) {
        let p = event.params;
        match event.method.as_str() {
            "Page.frameNavigated" if p["frame"].get("parentId").is_none() => {
                self.snapshot = Snapshot::default();
                self.refs = Refs::default();
                self.dialog = None;
                self.main_frame = p["frame"]["id"].as_str().map(str::to_owned);
            }
            "Page.javascriptDialogOpening" => self.dialog = Some(json!({"kind": p["type"], "message": p["message"]})),
            "Page.javascriptDialogClosed" => self.dialog = None,
            "Runtime.consoleAPICalled" => {
                let text = p["args"].as_array().map(|args| args.iter().map(remote_text).collect::<Vec<_>>().join(" ")).unwrap_or_default();
                let frame = &p["stackTrace"]["callFrames"][0];
                self.console(p["type"].as_str().unwrap_or("log"), text, frame.get("url"), frame.get("lineNumber"));
            }
            "Runtime.exceptionThrown" => {
                let details = &p["exceptionDetails"];
                let text = details.get("exception").map(remote_text).unwrap_or_else(|| details["text"].as_str().unwrap_or_default().to_owned());
                self.console("error", text, details.get("url"), details.get("lineNumber"));
            }
            "Log.entryAdded" => {
                let entry = &p["entry"];
                self.console(
                    entry["level"].as_str().unwrap_or("log"),
                    entry["text"].as_str().unwrap_or_default().to_owned(),
                    entry.get("url"),
                    entry.get("lineNumber"),
                );
            }
            "Network.requestWillBeSent" => {
                let id = p["requestId"].as_str().unwrap_or_default().to_owned();
                if let Some(redirect) = p.get("redirectResponse")
                    && let Some((_, previous)) = self.network.iter_mut().rev().find(|(key, _)| key == &id)
                {
                    previous["status"] = redirect["status"].clone();
                }
                self.network_id += 1;
                self.network.push_back((
                    id,
                    json!({"id": self.network_id, "method": p["request"]["method"], "url": p["request"]["url"], "type": p["type"], "at": at()}),
                ));
                if self.network.len() > 500 {
                    self.network.pop_front();
                }
            }
            "Network.responseReceived" | "Network.loadingFailed" => {
                if let Some((_, record)) = self.network.iter_mut().rev().find(|(key, _)| Some(key.as_str()) == p["requestId"].as_str()) {
                    if event.method == "Network.loadingFailed" {
                        record["failed"] = p["errorText"].clone();
                    } else {
                        record["status"] = p["response"]["status"].clone();
                        record["type"] = p["type"].clone();
                    }
                }
            }
            _ => {}
        }
    }
}

fn remote_text(value: &Value) -> String {
    if let Some(value) = value.get("value") {
        value.as_str().map_or_else(|| value.to_string(), str::to_owned)
    } else {
        value["description"].as_str().or_else(|| value["unserializableValue"].as_str()).unwrap_or_default().to_owned()
    }
}

/// A main-frame navigation an action may have started, followed through its events.
#[derive(Default)]
struct Navigation {
    started: bool,
    committed: bool,
    url: Option<String>,
}

impl Navigation {
    /// Feeds an event; true once the result can be reported.
    fn feed(&mut self, event: &DevToolsEvent, main: &str) -> bool {
        let p = &event.params;
        let main_frame = p["frameId"].as_str() == Some(main);
        let start = match event.method.as_str() {
            "Page.javascriptDialogOpening" => return true,
            "Page.navigatedWithinDocument" if main_frame => return true,
            "Page.frameNavigated" if p["frame"].get("parentId").is_none() => {
                self.started = true;
                self.committed = true;
                return false;
            }
            "Page.domContentEventFired" => return self.committed,
            "Page.frameStoppedLoading" if main_frame => return self.started,
            "Page.frameRequestedNavigation" => main_frame && p["disposition"].as_str().is_none_or(|d| d == "currentTab"),
            "Page.frameStartedNavigating" => main_frame && !matches!(p["navigationType"].as_str(), Some("sameDocument" | "historySameDocument")),
            "Page.frameScheduledNavigation" | "Page.frameStartedLoading" => main_frame,
            _ => false,
        };
        if start {
            self.started = true;
            if self.url.is_none() {
                self.url = p["url"].as_str().map(str::to_owned);
            }
        }
        false
    }

    /// The requested URL of a navigation that has not committed.
    fn pending(&self) -> Option<String> {
        (self.started && !self.committed).then(|| self.url.clone()).flatten()
    }
}

pub(crate) struct Executor {
    page: Rc<Browser>,
    state: Rc<RefCell<State>>,
    enabled: Mutex<bool>,
    requests: Mutex<()>,
    background: BackgroundExecutor,
    _events: Task<()>,
}

impl Executor {
    pub(crate) fn new(page: Rc<Browser>, cx: &mut Context<crate::browser_pane::BrowserPane>) -> Rc<Self> {
        let state = Rc::new(RefCell::new(State::default()));
        let weak = Rc::downgrade(&state);
        let mut events = page.subscribe_cdp();
        let task = cx.spawn(async move |_, _| {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let Some(state) = weak.upgrade() else { break };
                        state.borrow_mut().event(event);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(state) = weak.upgrade() {
                            let mut state = state.borrow_mut();
                            state.snapshot = Snapshot::default();
                            state.refs = Refs::default();
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Rc::new(Self {
            page,
            state,
            enabled: Mutex::new(false),
            requests: Mutex::new(()),
            background: cx.background_executor().clone(),
            _events: task,
        })
    }

    pub(crate) async fn run(&self, method: &str, mut params: Value) -> Result<Value> {
        // The server's typed params send absent options as `null`.
        if let Some(map) = params.as_object_mut() {
            map.retain(|_, v| !v.is_null());
        }
        let request: Request = serde_json::from_value(json!({"method": method, "params": params}))?;
        // A dialog must be answerable even while a script or input command is blocked on it.
        let bypass = matches!(request, Request::Dialog { .. } | Request::Console(_) | Request::Network(_));
        let _guard = if bypass { None } else { Some(self.requests.lock().await) };
        // A page blocked in a dialog answers no CDP until the dialog closes.
        if !matches!(request, Request::Dialog { .. }) {
            let mut enabled = self.enabled.lock().await;
            if !*enabled {
                for domain in ["Page", "Runtime", "DOM", "Accessibility", "Log", "Network"] {
                    self.cdp(&format!("{domain}.enable"), json!({})).await?;
                }
                let tree = self.cdp("Page.getFrameTree", json!({})).await?;
                self.state.borrow_mut().main_frame = tree["frameTree"]["frame"]["id"].as_str().map(str::to_owned);
                *enabled = true;
            }
        }
        let action = match &request {
            Request::Click { action, .. }
            | Request::Hover { action, .. }
            | Request::Fill { action, .. }
            | Request::FillForm { action, .. }
            | Request::Type { action, .. }
            | Request::Press { action, .. }
            | Request::Scroll { action, .. }
            | Request::Dialog { action, .. } => Some(*action),
            _ => None,
        };
        if !bypass && self.state.borrow().dialog.is_some() {
            return match action {
                Some(action) => self.action(action, None).await,
                None => Err(PendingDialog.into()),
            };
        }
        let result = self.execute(request).await;
        if result.as_ref().is_err_and(|error| error.is::<PendingDialog>()) {
            return match action {
                Some(action) => self.action(action, None).await,
                None => result,
            };
        }
        // A pending navigation holds renderer commands until it commits, and takes the objects with the old document.
        if !bypass && !result.as_ref().is_ok_and(|result| result.get("navigating_to").is_some()) {
            let _ = self.cdp("Runtime.releaseObjectGroup", json!({"objectGroup":"silo-agent"})).await;
        }
        result
    }

    async fn cdp(&self, method: &str, params: Value) -> Result<Value> {
        let mut events = self.page.subscribe_cdp();
        let interruptible = method.starts_with("Input.") || method == "Runtime.callFunctionOn";
        let reply = self.page.cdp(method, params);
        let timer = self.background.timer(Duration::from_secs(30));
        tokio::pin!(reply, timer);
        loop {
            tokio::select! {
                result = &mut reply => return result,
                () = &mut timer => bail!("{method} timed out"),
                event = events.recv(), if interruptible => {
                    if let Ok(event) = event && event.method == "Page.javascriptDialogOpening" {
                        self.state.borrow_mut().event(event);
                        return Err(PendingDialog.into());
                    }
                }
            }
        }
    }

    fn reference(&self, reference: &str) -> Result<u64> {
        let state = self.state.borrow();
        if state.generation == self.page.document_generation()
            && let Some(backend) = state.refs.get(reference)
        {
            return Ok(backend);
        }
        bail!("ref {reference} is not on the current page; take a new snapshot")
    }

    async fn resolve(&self, target: &str) -> Result<String> {
        let result = if is_ref(target) {
            self.cdp("DOM.resolveNode", json!({"backendNodeId": self.reference(target)?, "objectGroup": "silo-agent"})).await?["object"].clone()
        } else {
            self.cdp(
                "Runtime.evaluate",
                json!({"expression": format!("document.querySelector({})", serde_json::to_string(target)?), "objectGroup": "silo-agent"}),
            )
            .await?["result"]
                .clone()
        };
        result["objectId"].as_str().map(str::to_owned).with_context(|| format!("target {target:?} does not resolve to an element"))
    }

    async fn function(&self, object: &str, function: &str, args: Vec<Value>) -> Result<Value> {
        let result = self.cdp("Runtime.callFunctionOn", json!({"objectId": object, "functionDeclaration": function, "arguments": args, "returnByValue": true, "awaitPromise": true, "userGesture": true})).await?;
        if let Some(exception) = result.get("exceptionDetails") {
            bail!("JavaScript: {}", remote_text(&exception["exception"]));
        }
        Ok(result["result"].get("value").cloned().unwrap_or(Value::Null))
    }

    async fn metadata(&self) -> Result<Value> {
        let history = self.cdp("Page.getNavigationHistory", json!({})).await?;
        let index = history["currentIndex"].as_u64().context("missing current navigation")? as usize;
        let entry = &history["entries"][index];
        Ok(json!({"url":entry["url"].as_str().unwrap_or_default(), "title":entry["title"].as_str().unwrap_or_default()}))
    }

    async fn snapshot(&self, within: Option<&str>, depth: Option<u32>, boxes: bool) -> Result<String> {
        let within = within.map(|reference| self.reference(reference)).transpose()?;
        let generation = self.page.document_generation();
        let result = self.cdp("Accessibility.getFullAXTree", json!({})).await?;
        let nodes: Vec<AxNode> = serde_json::from_value(result["nodes"].clone())?;
        let mut bounds = HashMap::new();
        if boxes {
            let preview = Snapshot::build(&nodes, &mut self.state.borrow_mut().refs, &Options { within, depth, ..Options::default() })?;
            for backend in preview.refs.into_values() {
                if let Ok(model) = self.cdp("DOM.getBoxModel", json!({"backendNodeId": backend})).await
                    && let Ok(rect) = box_rect(&model)
                {
                    bounds.insert(backend, rect);
                }
            }
        }
        if generation != self.page.document_generation() {
            bail!("page navigated during snapshot; take a new snapshot");
        }
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        if state.generation != generation {
            state.refs = Refs::default();
        }
        state.snapshot = Snapshot::build(&nodes, &mut state.refs, &Options { within, depth, boxes: bounds })?;
        state.generation = generation;
        Ok(state.snapshot.text())
    }

    /// Follows a main-frame navigation the input started; returns its URL if it has not committed by the deadline.
    async fn settle(&self, mut events: tokio::sync::broadcast::Receiver<DevToolsEvent>) -> Option<String> {
        let main = self.state.borrow().main_frame.clone()?;
        let begun = Instant::now();
        let mut navigation = Navigation::default();
        loop {
            let deadline = begun + if navigation.started { NAVIGATION_SETTLE } else { NAVIGATION_START };
            let timer = self.background.timer(deadline.saturating_duration_since(Instant::now()));
            let event = tokio::select! {
                event = events.recv() => event,
                () = timer => return navigation.pending(),
            };
            let Ok(event) = event else { return navigation.pending() };
            let done = navigation.feed(&event, &main);
            if event.method == "Page.javascriptDialogOpening" {
                self.state.borrow_mut().event(event);
            }
            if done {
                return None;
            }
        }
    }

    async fn action(&self, action: Action, navigating_to: Option<String>) -> Result<Value> {
        let mut result = self.metadata().await?;
        if let Some(url) = navigating_to {
            result["navigating_to"] = url.into();
        }
        let dialog = self.state.borrow().dialog.clone();
        if let Some(dialog) = dialog {
            result["dialog"] = dialog;
        }
        if action.include_snapshot && result.get("dialog").is_none() && result.get("navigating_to").is_none() {
            let generation = self.page.document_generation();
            let snapshot = match self.snapshot(None, None, false).await {
                Err(_) if self.page.document_generation() != generation => self.snapshot(None, None, false).await?,
                snapshot => snapshot?,
            };
            result["snapshot"] = snapshot.into();
        }
        Ok(result)
    }

    async fn point(&self, target: &str) -> Result<(String, f64, f64)> {
        let object = self.resolve(target).await?;
        self.cdp("DOM.scrollIntoViewIfNeeded", json!({"objectId": object})).await?;
        let model = self.cdp("DOM.getBoxModel", json!({"objectId": object})).await?;
        let [x, y, w, h] = box_rect(&model)?;
        Ok((object, x + w / 2., y + h / 2.))
    }

    async fn fill(&self, target: &str, value: &str) -> Result<()> {
        let object = self.resolve(target).await?;
        self.function(&object, FILL, vec![json!({"value": value})]).await?;
        Ok(())
    }

    async fn press(&self, key: &str) -> Result<()> {
        let (key, code, modifiers, vk) = key_event(key)?;
        for kind in ["keyDown", "keyUp"] {
            let mut params = json!({"type": kind, "key": key, "code": code, "modifiers": modifiers, "windowsVirtualKeyCode": vk});
            if kind == "keyDown" && modifiers & 6 == 0 {
                if key == "Enter" {
                    params["text"] = "\r".into();
                } else if key.chars().count() == 1 {
                    params["text"] = key.clone().into();
                }
            }
            self.cdp("Input.dispatchKeyEvent", params).await?;
        }
        Ok(())
    }

    async fn execute(&self, request: Request) -> Result<Value> {
        // Subscribed before input so a navigation it starts is not missed.
        let events =
            matches!(request, Request::Click { .. } | Request::Press { .. } | Request::Type { submit: Some(_), .. } | Request::Dialog { .. })
                .then(|| self.page.subscribe_cdp());
        let action = match request {
            Request::Snapshot { within, depth, boxes } => {
                let snapshot = self.snapshot(within.as_deref(), depth, boxes).await?;
                let mut result = self.metadata().await?;
                result["snapshot"] = snapshot.into();
                return Ok(result);
            }
            Request::Find { text, regex } => {
                if text.is_some() == regex.is_some() {
                    bail!("provide exactly one of text or regex");
                }
                self.snapshot(None, None, false).await?;
                return Ok(json!({"matches": self.state.borrow().snapshot.find(text.as_deref(), regex.as_deref())?}));
            }
            Request::Click { target, double, button, action } => {
                let (_, x, y) = self.point(&target).await?;
                let button = button.unwrap_or(Button::Left).name();
                self.cdp("Input.dispatchMouseEvent", json!({"type":"mouseMoved", "x":x, "y":y})).await?;
                for count in 1..=if double { 2 } else { 1 } {
                    for kind in ["mousePressed", "mouseReleased"] {
                        self.cdp("Input.dispatchMouseEvent", json!({"type":kind, "x":x, "y":y, "button":button, "clickCount":count})).await?;
                    }
                }
                action
            }
            Request::Hover { target, action } => {
                let (_, x, y) = self.point(&target).await?;
                self.cdp("Input.dispatchMouseEvent", json!({"type":"mouseMoved", "x":x, "y":y})).await?;
                action
            }
            Request::Fill { target, value, action } => {
                self.fill(&target, &value).await?;
                action
            }
            Request::FillForm { fields, action } => {
                for field in fields {
                    self.fill(&field.target, &field.value).await?;
                }
                action
            }
            Request::Type { text, submit, action } => {
                self.cdp("Input.insertText", json!({"text":text})).await?;
                if let Some(key) = submit {
                    self.press(&key).await?;
                }
                action
            }
            Request::Press { key, action } => {
                self.press(&key).await?;
                action
            }
            Request::Scroll { target, dx, dy, action } => {
                let (dx, dy) = (dx.unwrap_or(0.), dy.unwrap_or(0.));
                // Scripted scrolling: synthetic wheel events are asynchronous and
                // missed outside the pointer's scroller.
                const BY: &str = "function(dx, dy) { let n = this; \
                    while (n && n !== document.scrollingElement && !(n.scrollHeight > n.clientHeight && /auto|scroll/.test(getComputedStyle(n).overflowY)) \
                    && !(n.scrollWidth > n.clientWidth && /auto|scroll/.test(getComputedStyle(n).overflowX))) n = n.parentElement; \
                    (n || document.scrollingElement || document.documentElement).scrollBy({left: dx, top: dy, behavior: 'instant'}); }";
                let object = match &target {
                    Some(target) => {
                        self.point(target).await?;
                        self.resolve(target).await?
                    }
                    None => self
                        .cdp(
                            "Runtime.evaluate",
                            json!({"expression": "document.scrollingElement || document.documentElement", "objectGroup": "silo-agent"}),
                        )
                        .await?["result"]["objectId"]
                        .as_str()
                        .context("page has no document element")?
                        .to_owned(),
                };
                if dx != 0. || dy != 0. {
                    self.function(&object, BY, vec![json!({"value": dx}), json!({"value": dy})]).await?;
                }
                action
            }
            Request::WaitFor { text, gone, timeout_ms } => {
                if text.as_ref().is_some_and(Vec::is_empty) || (gone && text.is_none()) {
                    bail!("text must be a non-empty list when supplied or when gone is true");
                }
                let deadline = Instant::now() + Duration::from_millis(timeout_ms.unwrap_or(10_000).min(115_000));
                let expression = if text.is_some() { "document.body ? document.body.innerText : ''" } else { "document.readyState === 'complete'" };
                let found = loop {
                    let result = self.cdp("Runtime.evaluate", json!({"expression":expression, "returnByValue":true})).await?;
                    let value = &result["result"]["value"];
                    let present = match &text {
                        Some(text) => text.iter().any(|text| value.as_str().unwrap_or_default().contains(text)),
                        None => value.as_bool().unwrap_or(false),
                    };
                    if present != gone {
                        break true;
                    }
                    if Instant::now() >= deadline {
                        break false;
                    }
                    self.background.timer(Duration::from_millis(100)).await;
                };
                let mut result = self.metadata().await?;
                result["found"] = found.into();
                return Ok(result);
            }
            Request::Dialog { accept, prompt_text, action } => {
                if !self.page.answer_dialog(accept, prompt_text.as_deref()) {
                    bail!("no JavaScript dialog is showing");
                }
                self.state.borrow_mut().dialog = None;
                action
            }
            Request::Eval { function, args } => {
                let mut resolved = Vec::new();
                for reference in args {
                    if !is_ref(&reference) {
                        bail!("eval args must be snapshot refs");
                    }
                    resolved.push(json!({"objectId":self.resolve(&reference).await?}));
                }
                let global = self.cdp("Runtime.evaluate", json!({"expression":"globalThis", "objectGroup":"silo-agent"})).await?;
                let object = global["result"]["objectId"].as_str().context("missing global object")?;
                return Ok(json!({"value":self.function(object, &function, resolved).await?}));
            }
            Request::Console(filter) => return Ok(json!({"messages": filtered(self.state.borrow().console.iter(), &filter)})),
            Request::Network(filter) => {
                return Ok(json!({"requests": filtered(self.state.borrow().network.iter().map(|(_, record)| record), &filter)}));
            }
            Request::Screenshot { target, full_page, format, quality } => {
                return self.screenshot(target.as_deref(), full_page, format.unwrap_or(Format::Png), quality).await;
            }
        };
        let navigating_to = match events {
            Some(events) => self.settle(events).await,
            None => None,
        };
        self.action(action, navigating_to).await
    }

    async fn screenshot(&self, target: Option<&str>, full_page: bool, mut format: Format, quality: Option<u32>) -> Result<Value> {
        if quality.is_some_and(|quality| quality > 100) {
            bail!("quality must be between 0 and 100");
        }
        let target_box = if let Some(target) = target {
            let (object, _, _) = self.point(target).await?;
            Some(box_rect(&self.cdp("DOM.getBoxModel", json!({"objectId":object})).await?)?)
        } else {
            None
        };
        let metrics = self.cdp("Page.getLayoutMetrics", json!({})).await?;
        let viewport = &metrics["cssVisualViewport"];
        let [x, y, width, height] = if let Some([x, y, w, h]) = target_box {
            [x + viewport["pageX"].as_f64().unwrap_or(0.), y + viewport["pageY"].as_f64().unwrap_or(0.), w, h]
        } else if full_page {
            let size = &metrics["cssContentSize"];
            [
                size["x"].as_f64().unwrap_or(0.),
                size["y"].as_f64().unwrap_or(0.),
                size["width"].as_f64().context("page width")?,
                size["height"].as_f64().context("page height")?,
            ]
        } else {
            let size = &metrics["cssVisualViewport"];
            [
                size["pageX"].as_f64().unwrap_or(0.),
                size["pageY"].as_f64().unwrap_or(0.),
                size["clientWidth"].as_f64().context("viewport width")?,
                size["clientHeight"].as_f64().context("viewport height")?,
            ]
        };
        if width <= 0. || height <= 0. {
            bail!("empty screenshot area");
        }
        let mut scale = (16_000_000. / (width * height)).sqrt().min(16_384. / width.max(height)).min(1.);
        // A page off screen paints a few times a second: ask for a frame now.
        self.page.invalidate();
        for attempt in 0..10 {
            let name = match format {
                Format::Png => "png",
                Format::Jpeg => "jpeg",
            };
            let mut params = json!({"format":name,"captureBeyondViewport":true,"clip":{"x":x,"y":y,"width":width,"height":height,"scale":scale}});
            if matches!(format, Format::Jpeg) {
                params["quality"] = quality.unwrap_or(80).into();
            }
            let result = self.cdp("Page.captureScreenshot", params).await?;
            let data = result["data"].as_str().context("missing screenshot data")?;
            // Cap the wire representation, not only the decoded image.
            if data.len() <= 4 * 1024 * 1024 {
                let bytes = data_encoding::BASE64.decode(data.as_bytes())?;
                let (width, height) = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?.into_dimensions()?;
                return Ok(json!({"mime":format!("image/{name}"),"data_base64":data,"width":width,"height":height}));
            }
            if attempt == 0 && matches!(format, Format::Png) {
                format = Format::Jpeg;
            } else {
                scale *= 0.65;
            }
        }
        bail!("screenshot exceeds 4 MiB")
    }
}

#[derive(Debug)]
struct PendingDialog;

impl std::fmt::Display for PendingDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JavaScript dialog is pending; answer it with browser.dialog")
    }
}

impl std::error::Error for PendingDialog {}

fn is_ref(target: &str) -> bool {
    target.strip_prefix('e').is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))
}

fn filtered<'a>(records: impl Iterator<Item = &'a Value>, filter: &Filter) -> Vec<Value> {
    records
        .filter(|v| v["id"].as_u64().unwrap_or(0) > filter.since.unwrap_or(0))
        .filter(|v| filter.level.as_ref().is_none_or(|level| v["level"].as_str() == Some(level)))
        .filter(|v| !filter.failed_only || v.get("failed").is_some())
        .take(filter.limit.unwrap_or(500).min(500) as usize)
        .cloned()
        .collect()
}

fn box_rect(model: &Value) -> Result<[f64; 4]> {
    let quad = model["model"]["border"].as_array().context("target has no box")?;
    if quad.len() != 8 {
        bail!("invalid box quad");
    }
    let numbers: Vec<f64> = quad.iter().map(|v| v.as_f64().context("invalid box coordinate")).collect::<Result<_>>()?;
    let xs = [numbers[0], numbers[2], numbers[4], numbers[6]];
    let ys = [numbers[1], numbers[3], numbers[5], numbers[7]];
    let x = xs.into_iter().fold(f64::INFINITY, f64::min);
    let y = ys.into_iter().fold(f64::INFINITY, f64::min);
    Ok([x, y, xs.into_iter().fold(f64::NEG_INFINITY, f64::max) - x, ys.into_iter().fold(f64::NEG_INFINITY, f64::max) - y])
}

fn key_event(combo: &str) -> Result<(String, String, u32, u32)> {
    let parts: Vec<&str> = combo.split('+').collect();
    let mut modifiers = 0;
    for modifier in &parts[..parts.len().saturating_sub(1)] {
        modifiers |= match *modifier {
            "Alt" => 1,
            "Control" | "Ctrl" => 2,
            "Meta" | "Command" => 4,
            "Shift" => 8,
            _ => bail!("unknown key modifier {modifier}"),
        };
    }
    let key = *parts.last().unwrap_or(&"");
    let (key, code, vk) = match key {
        "Enter" => ("Enter".into(), "Enter".into(), 13),
        "Tab" => ("Tab".into(), "Tab".into(), 9),
        "Escape" | "Esc" => ("Escape".into(), "Escape".into(), 27),
        "Backspace" => ("Backspace".into(), "Backspace".into(), 8),
        "Delete" => ("Delete".into(), "Delete".into(), 46),
        "ArrowLeft" => (key.into(), key.into(), 37),
        "ArrowUp" => (key.into(), key.into(), 38),
        "ArrowRight" => (key.into(), key.into(), 39),
        "ArrowDown" => (key.into(), key.into(), 40),
        "Home" => (key.into(), key.into(), 36),
        "End" => (key.into(), key.into(), 35),
        "PageUp" => (key.into(), key.into(), 33),
        "PageDown" => (key.into(), key.into(), 34),
        "Space" => (" ".into(), "Space".into(), 32),
        key if key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic() => {
            let upper = key.to_ascii_uppercase();
            let value = if modifiers & 8 != 0 { upper.clone() } else { key.to_ascii_lowercase() };
            (value, format!("Key{upper}"), u32::from(upper.as_bytes()[0]))
        }
        key if key.len() == 1 && key.as_bytes()[0].is_ascii_digit() => (key.into(), format!("Digit{key}"), u32::from(key.as_bytes()[0])),
        key if key.starts_with('F') && key[1..].parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)) => {
            (key.into(), key.into(), 111 + key[1..].parse::<u32>()?)
        }
        _ => bail!("unsupported key {key:?}"),
    };
    Ok((key, code, modifiers, vk))
}

const FILL: &str = r#"function(value) {
  if (this.disabled || this.readOnly) throw new Error('fill target is disabled or read-only');
  this.focus();
  if (this instanceof HTMLInputElement && (this.type === 'checkbox' || this.type === 'radio')) {
    if (value !== 'true' && value !== 'false') throw new Error('checkbox value must be true or false');
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'checked').set.call(this, value === 'true');
  } else {
    const proto = this instanceof HTMLInputElement ? HTMLInputElement.prototype :
      this instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype :
      this instanceof HTMLSelectElement ? HTMLSelectElement.prototype : null;
    if (!proto) throw new Error('fill target must be an input, textarea or select');
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(this, value);
  }
  this.dispatchEvent(new Event('input', {bubbles:true}));
  this.dispatchEvent(new Event('change', {bubbles:true}));
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_ref_syntax() {
        assert_eq!(key_event("Control+A").unwrap(), ("a".into(), "KeyA".into(), 2, 65));
        assert_eq!(key_event("Meta+Shift+R").unwrap(), ("R".into(), "KeyR".into(), 12, 82));
        assert!(key_event("Nope+A").is_err());
        assert!(is_ref("e12"));
        for selector in ["e", "e12x", "#e12", "e\u{ff11}\u{ff12}"] {
            assert!(!is_ref(selector));
        }
    }

    #[test]
    fn request_contract_is_independent_of_protocol_variants() {
        for (method, params) in [
            ("snapshot", json!({"within":"e12","depth":2,"boxes":true})),
            ("find", json!({"regex":"/email/i"})),
            ("click", json!({"target":"e12","double":true,"button":"right","include_snapshot":true})),
            ("hover", json!({"target":"button"})),
            ("fill", json!({"target":"#name","value":"Sam"})),
            ("fill_form", json!({"fields":[{"target":"#email","value":"a@b.c"}]})),
            ("type", json!({"text":"hello","submit":"Enter"})),
            ("press", json!({"key":"Meta+Shift+R"})),
            ("scroll", json!({"dy":100})),
            ("wait_for", json!({"text":["ready"],"gone":false,"timeout_ms":1000})),
            ("dialog", json!({"accept":true,"prompt_text":"answer"})),
            ("eval", json!({"function":"node => node.textContent","args":["e12"]})),
            ("console", json!({"since":1,"level":"error","limit":20})),
            ("network", json!({"failed_only":true})),
            ("screenshot", json!({"format":"jpeg","quality":80,"full_page":true})),
        ] {
            assert!(serde_json::from_value::<Request>(json!({"method":format!("browser.{method}"),"params":params})).is_ok(), "{method}");
        }
        assert!(serde_json::from_value::<Request>(json!({"method":"browser.click","params":{"target":"e12","button":"nope"}})).is_err());
    }

    #[test]
    fn navigation_invalidates_refs_but_keeps_logs() {
        let mut state = State::default();
        state.snapshot.refs.insert("e12".into(), 12);
        state.event(DevToolsEvent { method: "Page.javascriptDialogOpening".into(), params: json!({"type":"prompt","message":"Name?"}) });
        assert_eq!(state.dialog.as_ref().unwrap()["kind"], "prompt");
        state.event(DevToolsEvent { method: "Page.frameNavigated".into(), params: json!({"frame":{"id":"child","parentId":"main"}}) });
        assert_eq!(state.snapshot.refs.len(), 1);
        state.console("log", "kept".into(), None, None);
        state.event(DevToolsEvent { method: "Page.frameNavigated".into(), params: json!({"frame":{"id":"main"}}) });
        assert!(state.snapshot.refs.is_empty());
        assert!(state.dialog.is_none());
        assert_eq!(state.console.len(), 1);
    }

    fn navigation(events: &[(&str, Value)]) -> (Navigation, Option<usize>) {
        let mut navigation = Navigation::default();
        let done =
            events.iter().position(|(method, params)| navigation.feed(&DevToolsEvent { method: (*method).into(), params: params.clone() }, "main"));
        (navigation, done)
    }

    #[test]
    fn navigation_settles_on_main_frame_events() {
        let (iframe, done) = navigation(&[
            ("Page.frameRequestedNavigation", json!({"frameId":"child","url":"https://ad.example","disposition":"currentTab"})),
            ("Page.frameStartedLoading", json!({"frameId":"child"})),
            ("Page.navigatedWithinDocument", json!({"frameId":"child","url":"https://ad.example#x"})),
            ("Page.frameNavigated", json!({"frame":{"id":"child","parentId":"main"}})),
            ("Page.domContentEventFired", json!({})),
        ]);
        assert!(!iframe.started && done.is_none() && iframe.pending().is_none());
        let (_, done) = navigation(&[("Page.navigatedWithinDocument", json!({"frameId":"main","url":"https://a.example#b"}))]);
        assert_eq!(done, Some(0));
        let (requested, done) = navigation(&[
            ("Page.frameRequestedNavigation", json!({"frameId":"main","url":"https://b.example/","disposition":"currentTab"})),
            ("Page.frameStartedLoading", json!({"frameId":"main"})),
            ("Page.domContentEventFired", json!({})),
        ]);
        assert!(done.is_none());
        assert_eq!(requested.pending().as_deref(), Some("https://b.example/"));
        let (committed, done) = navigation(&[
            ("Page.frameRequestedNavigation", json!({"frameId":"main","url":"https://b.example/","disposition":"currentTab"})),
            ("Page.frameNavigated", json!({"frame":{"id":"main","url":"https://b.example/"}})),
            ("Page.domContentEventFired", json!({})),
        ]);
        assert_eq!(done, Some(2));
        assert!(committed.pending().is_none());
        let (new_tab, _) =
            navigation(&[("Page.frameRequestedNavigation", json!({"frameId":"main","url":"https://c.example/","disposition":"newTab"}))]);
        assert!(!new_tab.started);
        let (_, done) = navigation(&[
            ("Page.frameRequestedNavigation", json!({"frameId":"main","url":"https://b.example/"})),
            ("Page.javascriptDialogOpening", json!({"type":"confirm","message":"Leave?"})),
        ]);
        assert_eq!(done, Some(1));
    }

    #[test]
    fn event_buffers_are_bounded_and_filterable() {
        let mut state = State::default();
        for id in 0..600 {
            state.event(DevToolsEvent { method: "Runtime.consoleAPICalled".into(), params: json!({"type":"log", "args":[{"value":id}]}) });
            state.event(DevToolsEvent {
                method: "Network.requestWillBeSent".into(),
                params: json!({"requestId":id.to_string(), "request":{"method":"GET","url":"https://example.com"},"type":"Document"}),
            });
        }
        assert_eq!(state.console.len(), 500);
        assert_eq!(state.network.len(), 500);
        state.event(DevToolsEvent { method: "Network.loadingFailed".into(), params: json!({"requestId":"599","errorText":"failed"}) });
        assert_eq!(filtered(state.network.iter().map(|(_, v)| v), &Filter { failed_only: true, ..Filter::default() }).len(), 1);
        assert_eq!(filtered(state.console.iter(), &Filter { since: Some(598), ..Filter::default() }).len(), 2);
    }
}
