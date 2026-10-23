//! The app's otto daemon connections, one per host (`docs/AGENT_BLOCK.md`
//! §1): this Mac's on its socket, a remote host's through that host's
//! tunnel (service `otto`). Also the calls the pane makes that are not
//! plain `Connection` methods.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use otto_client::{ClientError, Connection};
use otto_proto::{AgentInfo, AuthStatus, ConfigValue, Method, ModelInfo, ProjectInfo};
use silo_client::{transport::EndpointAddr, tunnel::TunnelDialer};

use crate::hosts::Transport;

/// The tunnel service a remote host's otto daemon answers on.
pub const SERVICE: &str = "otto";

/// Which otto daemon a pane talks to.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Otto {
    /// This Mac's.
    Local,
    /// A paired remote host's, by its EndpointId.
    Remote { id: String },
}

impl Otto {
    /// The daemon of a host reached over `transport`; a shared link has none.
    #[must_use]
    pub fn of(transport: &Transport) -> Option<Self> {
        match transport {
            Transport::Local { .. } => Some(Self::Local),
            Transport::Iroh { addr, .. } => Some(Self::Remote { id: addr.id.to_string() }),
            Transport::Shared { .. } => None,
        }
    }
}

/// A remote host's way in: its address, and the one dialer its otto
/// connection and browser tunnel proxy (`web.rs`) share, made on first use.
struct Route {
    home: PathBuf,
    addr: EndpointAddr,
    dialer: Option<Arc<TunnelDialer>>,
}

type Slot = Arc<tokio::sync::Mutex<Option<Connection>>>;

static ROUTES: Mutex<Option<HashMap<String, Route>>> = Mutex::new(None);
/// Cached connections, one lock per daemon (a slow host never holds up another).
static SLOTS: Mutex<Option<HashMap<Otto, Slot>>> = Mutex::new(None);

fn routes<T>(f: impl FnOnce(&mut HashMap<String, Route>) -> T) -> T {
    f(ROUTES.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_insert_with(HashMap::new))
}

fn slots<T>(f: impl FnOnce(&mut HashMap<Otto, Slot>) -> T) -> T {
    f(SLOTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_insert_with(HashMap::new))
}

/// A remote host was added or its address saved: `addr` is used for its
/// next dial. An existing dialer stays (it redials by EndpointId).
pub fn set_route(id: &str, home: PathBuf, addr: EndpointAddr) {
    routes(|routes| match routes.get_mut(id) {
        Some(route) => (route.home, route.addr) = (home, addr),
        None => _ = routes.insert(id.to_owned(), Route { home, addr, dialer: None }),
    });
}

/// A route for `id` unless it has one (a shared link's server, whose
/// browsers tunnel too; a paired host's newer address stays).
pub fn ensure_route(id: &str, home: PathBuf, addr: EndpointAddr) {
    routes(|routes| _ = routes.entry(id.to_owned()).or_insert(Route { home, addr, dialer: None }));
}

/// A remote host was forgotten: its route, dialer and otto connection go.
pub fn forget(id: &str) {
    routes(|routes| routes.remove(id));
    let slot = slots(|slots| slots.remove(&Otto::Remote { id: id.to_owned() }));
    if let Some(connection) = slot.as_ref().and_then(|s| s.try_lock().ok()).and_then(|mut c| c.take()) {
        connection.close();
    }
}

/// The tunnel dialer of remote host `id`, made once.
///
/// # Errors
/// The host has no route (forgotten), or the endpoint could not bind.
pub async fn dialer(id: &str) -> std::io::Result<Arc<TunnelDialer>> {
    let gone = || std::io::Error::new(std::io::ErrorKind::NotFound, "that host was removed");
    let (home, addr) = match routes(|r| r.get(id).map(|r| (r.dialer.clone(), r.home.clone(), r.addr.clone()))).ok_or_else(gone)? {
        (Some(dialer), ..) => return Ok(dialer),
        (None, home, addr) => (home, addr),
    };
    let net = crate::remote::net(&home).await.map_err(|e| std::io::Error::other(format!("{e:#}")))?;
    routes(|r| {
        let route = r.get_mut(id).ok_or_else(gone)?;
        Ok(route.dialer.get_or_insert_with(|| Arc::new(TunnelDialer::new(net, addr))).clone())
    })
}

/// Streams to remote host `id`'s otto daemon, each on its tunnel.
fn dial(id: String) -> otto_client::Dial {
    type Opened = std::pin::Pin<Box<dyn Future<Output = otto_client::Result<Box<dyn otto_client::Stream>>> + Send>>;
    Arc::new(move || -> Opened {
        let id = id.clone();
        Box::pin(async move {
            let stream = dialer(&id).await?.open_service(SERVICE).await?;
            Ok(Box::new(stream) as Box<dyn otto_client::Stream>)
        })
    })
}

/// The shared connection to `otto`'s daemon. This Mac's starts `otto-silo`
/// (next to the app executable) when no daemon runs; a remote host's runs
/// through its tunnel and redials through it. A cached connection whose
/// daemon went away is dropped here, so the next call reconnects.
pub async fn connection(otto: &Otto) -> Result<Connection, ClientError> {
    let slot = slots(|slots| slots.entry(otto.clone()).or_default().clone());
    let mut slot = slot.lock().await;
    if let Some(connection) = slot.as_ref().filter(|c| !c.is_closed()) {
        return Ok(connection.clone());
    }
    let connection = match otto {
        Otto::Local => {
            let exe = std::env::current_exe()?;
            let binary = exe.with_file_name(otto_proto::BINARY);
            otto_client::connect_or_start(&otto_proto::paths::home(), &binary, &["server", "--daemon"]).await?
        }
        Otto::Remote { id } => Connection::connect_dial(dial(id.clone())).await?,
    };
    *slot = Some(connection.clone());
    Ok(connection)
}

/// The project of `cwd`: the daemon opens its git toplevel (else `cwd`,
/// else its user's home), like `otto-silo sessions`.
pub async fn project(connection: &Connection, cwd: Option<&str>) -> Result<ProjectInfo, ClientError> {
    connection.call_typed(Method::ProjectOpen { root: String::new(), cwd: cwd.map(str::to_owned) }).await
}

/// Providers, models and agents for the composer's pickers.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub auth: Vec<AuthStatus>,
    pub models: Vec<ModelInfo>,
    pub agents: Vec<AgentInfo>,
    /// `config.get` `provider` / `model`: they override the daemon's default.
    pub config_provider: Option<String>,
    pub config_model: Option<String>,
}

impl Catalog {
    #[must_use]
    pub fn signed_in(&self, provider: &str) -> bool {
        self.auth.iter().any(|a| a.provider == provider && a.signed_in)
    }

    #[must_use]
    pub fn any_signed_in(&self) -> bool {
        self.auth.iter().any(|a| a.signed_in)
    }

    /// Signed in with a subscription: its plan (`Max`), else `sub`; shown as a marker by the model.
    #[must_use]
    pub fn subscription(&self, provider: &str) -> Option<String> {
        self.auth
            .iter()
            .find(|a| a.provider == provider && a.signed_in && a.mode == otto_proto::AuthMode::Subscription)
            .map(|a| a.plan.clone().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| "sub".into()))
    }

    /// Models of signed-in providers.
    pub fn usable(&self) -> impl Iterator<Item = &ModelInfo> {
        self.models.iter().filter(|m| self.signed_in(m.provider.as_str()))
    }

    /// The provider and model a new session with `agent` gets when none is
    /// picked, resolved like the daemon: agent model, then config, then the
    /// signed-in default.
    #[must_use]
    pub fn default_model(&self, agent: &str) -> Option<(String, String)> {
        let mut provider = self.auth.iter().find(|a| a.default_model.is_some()).map(|a| a.provider.clone());
        let mut model = self.auth.iter().find_map(|a| a.default_model.clone());
        if let Some(p) = &self.config_provider {
            provider = Some(p.clone());
        }
        if let Some(m) = &self.config_model {
            model = Some(m.clone());
        }
        if let Some(spec) = self.agents.iter().find(|a| a.name == agent).and_then(|a| a.model.as_deref()) {
            match spec.split_once('/') {
                Some((p, m)) => (provider, model) = (Some(p.to_owned()), Some(m.to_owned())),
                None => model = Some(spec.to_owned()),
            }
        }
        Some((provider?, model?)).filter(|(p, m)| !p.is_empty() && !m.is_empty())
    }
}

/// Set when a provider is connected or disconnected: every agent pane reloads its catalog.
pub struct AuthChanged;
impl gpui::Global for AuthChanged {}

/// Set when an agent or a default is saved on an otto (Otto settings): every
/// agent pane on that otto reloads its pickers.
pub struct OttoChanged(pub Otto);
impl gpui::Global for OttoChanged {}

impl OttoChanged {
    /// Whether a pane on `target` reloads for this change.
    #[must_use]
    pub fn concerns(&self, target: &Otto) -> bool {
        self.0 == *target
    }
}

pub async fn catalog(connection: &Connection) -> Result<Catalog, ClientError> {
    let auth = connection.auth_status().await?;
    let models = connection.model_list(None).await.unwrap_or_default();
    let agents = connection.call_typed(Method::AgentsList {}).await.unwrap_or_default();
    let config = connection.call_typed::<ConfigValue>(Method::ConfigGet { key: None }).await.map(|c| c.value).unwrap_or_default();
    let text = |key: &str| config.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_owned);
    Ok(Catalog { auth, models, agents, config_provider: text("provider"), config_model: text("model") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_proto::AuthMode;

    fn status(provider: &str, default_model: Option<&str>) -> AuthStatus {
        AuthStatus {
            provider: provider.into(),
            mode: AuthMode::ApiKey,
            signed_in: default_model.is_some(),
            expires_at: None,
            account: None,
            modes: vec![AuthMode::ApiKey, AuthMode::Subscription],
            default_model: default_model.map(Into::into),
            base_url: None,
            plan: None,
        }
    }

    #[test]
    fn default_model_follows_daemon_precedence() {
        let mut catalog = Catalog { auth: vec![status("anthropic", None), status("openai", Some("gpt-x"))], ..Catalog::default() };
        assert_eq!(catalog.default_model("build"), Some(("openai".into(), "gpt-x".into())));
        catalog.config_model = Some("gpt-y".into());
        assert_eq!(catalog.default_model("build"), Some(("openai".into(), "gpt-y".into())));
        catalog.agents = vec![AgentInfo {
            name: "review".into(),
            description: String::new(),
            model: Some("anthropic/claude-z".into()),
            provider: None,
            scope: otto_proto::AgentScope::User,
        }];
        assert_eq!(catalog.default_model("review"), Some(("anthropic".into(), "claude-z".into())));
        assert_eq!(Catalog::default().default_model("build"), None);
    }

    #[test]
    fn daemons_are_keyed_by_host() {
        let dir = crate::hosts::tests::temp("otto-keys");
        let (a, b) = (crate::hosts::tests::addr(&dir.join("a")), crate::hosts::tests::addr(&dir.join("b")));
        let local = Otto::of(&crate::hosts::tests::local_transport(&dir));
        let remote = |addr: &EndpointAddr| Otto::of(&Transport::Iroh { home: dir.clone(), addr: addr.clone() });
        assert_eq!(local, Some(Otto::Local));
        assert_eq!(remote(&a), Some(Otto::Remote { id: a.id.to_string() }));
        let moved = EndpointAddr::new(a.id);
        assert_eq!(remote(&moved), remote(&a), "a new address is the same daemon");
        let keys: std::collections::HashSet<Option<Otto>> = [local, remote(&a), remote(&moved), remote(&b)].into_iter().collect();
        assert_eq!(keys.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn routes_are_set_and_forgotten() {
        let dir = crate::hosts::tests::temp("otto-routes");
        let addr = crate::hosts::tests::addr(&dir.join("a"));
        let id = addr.id.to_string();
        let routed = || routes(|r| r.get(&id).map(|r| (r.addr.clone(), r.dialer.is_some())));
        assert_eq!(routed(), None);
        set_route(&id, dir.clone(), addr.clone());
        assert_eq!(routed(), Some((addr.clone(), false)), "dialed only on first use");
        let moved = EndpointAddr::new(addr.id);
        set_route(&id, dir.clone(), moved.clone());
        assert_eq!(routed(), Some((moved, false)), "the saved address replaces the old");
        assert_eq!(routes(|r| r.keys().filter(|k| **k == id).count()), 1);
        slots(|s| s.entry(Otto::Remote { id: id.clone() }).or_default().clone());
        forget(&id);
        assert_eq!(routed(), None);
        assert!(!slots(|s| s.contains_key(&Otto::Remote { id: id.clone() })), "its connection went too");
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(rt.block_on(dialer(&id)).err().map(|e| e.kind()), Some(std::io::ErrorKind::NotFound));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn subscription_marker_names_the_plan() {
        let mut sub = status("anthropic", Some("claude-z"));
        sub.mode = AuthMode::Subscription;
        let mut catalog = Catalog { auth: vec![sub, status("openai", Some("gpt-x"))], ..Catalog::default() };
        assert_eq!(catalog.subscription("anthropic").as_deref(), Some("sub"));
        catalog.auth[0].plan = Some("Max".into());
        assert_eq!(catalog.subscription("anthropic").as_deref(), Some("Max"));
        assert_eq!(catalog.subscription("openai"), None);
    }

    #[test]
    fn a_save_reloads_panes_on_the_same_otto_only() {
        let mini = || Otto::Remote { id: "mini".into() };
        assert!(OttoChanged(Otto::Local).concerns(&Otto::Local));
        assert!(OttoChanged(mini()).concerns(&mini()));
        assert!(!OttoChanged(Otto::Local).concerns(&mini()));
        assert!(!OttoChanged(mini()).concerns(&Otto::Remote { id: "boat".into() }));
    }
}
