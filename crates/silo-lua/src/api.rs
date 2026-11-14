//! How Lua reaches Silo: a [`Backend`] that calls control methods, and an
//! [`Api`] that adds where ids default from.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
};

use serde_json::{Map, Value};
use silo_proto::{
    control::RpcError,
    model::{BlockId, ClientId, SessionId, WindowId},
};

use crate::Ctx;

/// A boxed future a [`Backend`] answers with.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Calls control methods by name with raw JSON params: a connection to a
/// server ([`silo_client::Control`]), or a fake in tests.
pub trait Backend: Send + Sync {
    fn call(&self, method: String, params: Value) -> BoxFuture<Result<Value, RpcError>>;
}

impl Backend for silo_client::Control {
    fn call(&self, method: String, params: Value) -> BoxFuture<Result<Value, RpcError>> {
        let control = self.clone();
        Box::pin(async move { control.call_raw(&method, params).await.map_err(rpc_error) })
    }
}

/// A [`silo_client::Control`] call's error as the server's [`RpcError`]:
/// a refused inner call keeps its code (`forbidden`, `not_found`,
/// `no_app`, …); a closed connection is `closed`.
#[must_use]
pub fn rpc_error(error: anyhow::Error) -> RpcError {
    let error = match error.downcast::<RpcError>() {
        Ok(rpc) => return rpc,
        Err(other) => other,
    };
    if let Some(e) = error.downcast_ref::<silo_client::NotSupported>() {
        return RpcError::new(RpcError::METHOD_NOT_FOUND, e.message.clone());
    }
    if let Some(e) = error.downcast_ref::<silo_client::Unauthorized>() {
        return RpcError::new(RpcError::UNAUTHORIZED, e.message.clone());
    }
    if error.is::<silo_client::Closed>() {
        return RpcError::new(CLOSED, "the connection to the server closed");
    }
    RpcError::internal(format!("{error:#}"))
}

/// Code of a call on a closed connection.
pub const CLOSED: &str = "closed";
/// Code of a call with no backend: at config load, or a check.
pub const NO_CALLER: &str = "no_caller";

/// What Lua calls Silo through, and where `session_id`, `window_id`,
/// `block_id` and `client_id` default from when the running invocation's
/// [`Ctx`] has none (a script's `-s`/`-b`). Cheap to clone.
#[derive(Clone, Default)]
pub struct Api {
    backend: Option<Arc<dyn Backend>>,
    pub session_id: Option<SessionId>,
    pub window_id: Option<WindowId>,
    pub block_id: Option<BlockId>,
    pub client_id: Option<ClientId>,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("connected", &self.backend.is_some())
            .field("session_id", &self.session_id)
            .field("window_id", &self.window_id)
            .field("block_id", &self.block_id)
            .field("client_id", &self.client_id)
            .finish()
    }
}

impl Api {
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend: Some(backend), ..Self::default() }
    }

    /// Calls over `control`.
    #[must_use]
    pub fn control(control: silo_client::Control) -> Self {
        Self::new(Arc::new(control))
    }

    /// No backend: every call fails with [`NO_CALLER`].
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_session(mut self, session_id: Option<SessionId>) -> Self {
        self.session_id = session_id;
        self
    }

    #[must_use]
    pub fn with_block(mut self, block_id: Option<BlockId>) -> Self {
        self.block_id = block_id;
        self
    }

    #[must_use]
    pub fn with_client(mut self, client_id: Option<ClientId>) -> Self {
        self.client_id = client_id;
        self
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.backend.is_some()
    }

    /// Call `method` with exactly `params`.
    ///
    /// # Errors
    /// The server's error; [`NO_CALLER`] without a backend.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match &self.backend {
            Some(backend) => backend.call(method.to_owned(), params).await,
            None => Err(RpcError::new(NO_CALLER, format!("{method}: nothing to call Silo with here (calls do not run at config load)"))),
        }
    }

    /// `params` with the ids `method` takes and leaves out filled from
    /// `ctx`, else from this API's defaults; `client_id` too for `app.*`.
    /// Unknown methods and non-object params are left as they are.
    #[must_use]
    pub fn fill(&self, method: &str, params: Value, ctx: &Ctx) -> Value {
        let Value::Object(mut map) = params else { return params };
        let app = method.starts_with("app.");
        let takes = takes(method);
        let mut put = |key: &str, takes: bool, value: Option<Value>| {
            if takes
                && !map.contains_key(key)
                && let Some(value) = value
            {
                map.insert(key.to_owned(), value);
            }
        };
        let string = |a: &Option<String>, b: &Option<String>| a.clone().or_else(|| b.clone()).map(Value::String);
        if let Some(t) = takes {
            put("session_id", t.session, string(&ctx.session_id, &self.session_id));
            put("window_id", t.window, string(&ctx.window_id, &self.window_id));
            put("block_id", t.block, string(&ctx.block_id, &self.block_id));
        }
        put("client_id", app, ctx.client_id.or(self.client_id).map(Value::from));
        Value::Object(map)
    }
}

/// Which ids a method's params have.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Takes {
    session: bool,
    window: bool,
    block: bool,
}

/// From the method's param schema ([`silo_proto::api::describe`], built
/// once per method); `None` for a method this build does not know.
pub(crate) fn takes(method: &str) -> Option<Takes> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Takes>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    if let Some(found) = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(method) {
        return *found;
    }
    let found = silo_proto::api::describe(method).map(|d| {
        let props = d.params_schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_else(Map::new);
        Takes { session: props.contains_key("session_id"), window: props.contains_key("window_id"), block: props.contains_key("block_id") }
    });
    cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(method.to_owned(), found);
    found
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn ids_fill_only_where_the_method_takes_them() {
        let api = Api::none().with_session(Some("s0".into())).with_client(Some(9));
        let ctx = Ctx { block_id: Some("b1".into()), window_id: Some("w1".into()), ..Ctx::default() };
        assert_eq!(api.fill("block.zoom", json!({"zoomed": true}), &ctx), json!({"zoomed": true, "block_id": "b1"}));
        assert_eq!(api.fill("session.view", json!({}), &ctx), json!({"session_id": "s0"}));
        assert_eq!(api.fill("window.rename", json!({"name": "x"}), &ctx), json!({"name": "x", "window_id": "w1"}));
        assert_eq!(api.fill("block.zoom", json!({"block_id": "b9"}), &ctx), json!({"block_id": "b9"}), "given ids stay");
        assert_eq!(api.fill("nope.nothing", json!({}), &ctx), json!({}), "unknown: as given");
        assert_eq!(api.fill("app.palette", json!({}), &ctx), json!({"client_id": 9}));
    }
}
