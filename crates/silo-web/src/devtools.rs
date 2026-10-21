//! In-process CDP. Requests and registration stay on CEF's UI thread.

#![allow(clippy::transmute_ptr_to_ptr)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result, anyhow, bail};
use cef::rc::Rc as _;
use cef::*;
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

use crate::shared::lock;

#[derive(Clone, Debug)]
pub struct DevToolsEvent {
    pub method: String,
    pub params: Value,
}

pub(crate) struct Channel {
    next: AtomicI32,
    pending: Mutex<HashMap<i32, oneshot::Sender<Result<Value>>>>,
    registration: Mutex<Option<Registration>>,
    dialog: Mutex<Option<JsdialogCallback>>,
    page_enabled: AtomicBool,
    events: broadcast::Sender<DevToolsEvent>,
    pub(crate) generation: AtomicU64,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            next: AtomicI32::new(1),
            pending: Mutex::new(HashMap::new()),
            registration: Mutex::new(None),
            dialog: Mutex::new(None),
            page_enabled: AtomicBool::new(false),
            events: broadcast::channel(2048).0,
            generation: AtomicU64::new(0),
        }
    }
}

impl Channel {
    pub(crate) fn hold_dialog(&self, callback: Option<&mut JsdialogCallback>) -> i32 {
        if !self.page_enabled.load(Ordering::Relaxed) {
            return 0;
        }
        let Some(callback) = callback else { return 0 };
        let previous = lock(&self.dialog).replace(callback.clone());
        drop(previous);
        1
    }

    pub(crate) fn reset_dialog(&self) {
        let callback = lock(&self.dialog).take();
        drop(callback);
    }

    /// Answer the dialog held by [`Self::hold_dialog`]; false if none is.
    pub(crate) fn answer_dialog(&self, accept: bool, prompt: Option<&str>) -> bool {
        let Some(callback) = lock(&self.dialog).take() else { return false };
        callback.cont(i32::from(accept), prompt.map(CefString::from).as_ref());
        true
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<DevToolsEvent> {
        self.events.subscribe()
    }

    pub(crate) fn close(&self) {
        self.reset_dialog();
        self.fail_pending();
        lock(&self.registration).take();
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn fail_pending(&self) {
        self.page_enabled.store(false, Ordering::Relaxed);
        for (_, reply) in std::mem::take(&mut *lock(&self.pending)) {
            let _ = reply.send(Err(anyhow!("browser DevTools agent closed")));
        }
    }

    pub(crate) async fn call(self: &Arc<Self>, host: BrowserHost, method: &str, params: Value) -> Result<Value> {
        if objc2::MainThreadMarker::new().is_none() {
            bail!("CDP must run on the browser UI thread");
        }
        if lock(&self.registration).is_none() {
            let mut observer = Observer::new(Arc::downgrade(self));
            let registration = host.add_dev_tools_message_observer(Some(&mut observer)).context("register DevTools observer")?;
            *lock(&self.registration) = Some(registration);
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        if id <= 0 {
            bail!("DevTools message ids exhausted");
        }
        let (send, receive) = oneshot::channel();
        lock(&self.pending).insert(id, send);
        let _pending = Pending { channel: self, id };
        let message = serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))?;
        if host.send_dev_tools_message(Some(&message)) != 1 {
            bail!("CEF rejected {method}");
        }
        let result = receive.await.context("DevTools reply channel closed")?;
        if result.is_ok() && matches!(method, "Page.enable" | "Page.disable") {
            self.page_enabled.store(method == "Page.enable", Ordering::Relaxed);
        }
        result
    }
}

struct Pending<'a> {
    channel: &'a Channel,
    id: i32,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        lock(&self.channel.pending).remove(&self.id);
    }
}

wrap_dev_tools_message_observer! {
    struct Observer {
        channel: Weak<Channel>,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(&self, _browser: Option<&mut cef::Browser>, message_id: i32, success: i32, result: Option<&[u8]>) {
            let Some(channel) = self.channel.upgrade() else { return };
            let reply = lock(&channel.pending).remove(&message_id);
            if let Some(reply) = reply {
                let value = serde_json::from_slice(result.unwrap_or(b"{}"));
                let result = match value {
                    Ok(value) if success != 0 => Ok(value),
                    Ok(value) => Err(anyhow!("CDP: {value}")),
                    Err(error) => Err(error.into()),
                };
                let _ = reply.send(result);
            }
        }

        fn on_dev_tools_event(&self, _browser: Option<&mut cef::Browser>, method: Option<&CefString>, params: Option<&[u8]>) {
            let Some(channel) = self.channel.upgrade() else { return };
            let Some(method) = method else { return };
            let method = method.to_string();
            let Ok(params) = serde_json::from_slice::<Value>(params.unwrap_or(b"{}")) else { return };
            if method == "Page.frameNavigated" && params["frame"].get("parentId").is_none() {
                channel.generation.fetch_add(1, Ordering::Relaxed);
            }
            let _ = channel.events.send(DevToolsEvent { method, params });
        }

        fn on_dev_tools_agent_detached(&self, _browser: Option<&mut cef::Browser>) {
            if let Some(channel) = self.channel.upgrade() {
                channel.fail_pending();
                channel.generation.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
