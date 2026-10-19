//! Per-profile request contexts (`<root>/profiles/<name>`), optionally proxied through a tunnel.

#![allow(clippy::transmute_ptr_to_ptr)]

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cef::rc::Rc as _;
use cef::*;

use crate::shared::lock;

pub const DEFAULT: &str = "default";

/// A tunnel's local HTTP proxy at `127.0.0.1:<port>` and its Basic proxy-auth credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct Tunnel {
    pub port: u16,
    pub user: String,
    pub pass: String,
}

impl fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Tunnel {{ port: {}, user: {:?}, pass: <redacted> }}", self.port, self.user)
    }
}

/// The profile's current tunnel plus an answer budget, read by its browsers' auth handler on
/// CEF's IO thread.
#[derive(Debug, Default)]
pub(crate) struct TunnelAuth {
    tunnel: Option<Tunnel>,
    /// The context finished initializing: prefs can be set. Before that `ProxyOnInit` applies
    /// whatever `tunnel` holds when it runs.
    initialized: bool,
    window: Option<Instant>,
    answers: u32,
}

pub(crate) type SharedTunnel = Arc<Mutex<TunnelAuth>>;

/// Chromium re-asks immediately after every rejected 407, so stale credentials would retry
/// forever. Answer at most this many challenges per second; the rest are cancelled.
const ANSWERS_PER_SECOND: u32 = 4;

/// Creates `dir` (and parents) and forces mode 0700: cookies are stored unencrypted
/// (`--use-mock-keychain`), so profile data must be owner-only.
pub(crate) fn private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).with_context(|| format!("chmod 0700 {}", dir.display()))
}

/// A browser profile: its own cookies, storage and proxy settings.
#[derive(Clone)]
pub struct Profile {
    pub name: String,
    pub(crate) context: RequestContext,
    pub(crate) tunnel: SharedTunnel,
}

/// Directory name for a profile (`"<server>/<name>"` → `"<server>_<name>"`).
pub(crate) fn dir_name(name: &str) -> Result<String> {
    let dir: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    if dir.is_empty() || dir.chars().all(|c| c == '.') {
        bail!("invalid profile name {name:?}");
    }
    Ok(dir)
}

/// Sets Chromium's `proxy` preference: everything (loopback included) through the tunnel proxy at
/// `127.0.0.1:<port>`, or direct when `None`. The proxy itself sends public hosts DIRECT: Chromium
/// overrides PAC results for loopback, and a bypass list can't express "private hosts only".
fn apply_proxy(context: &RequestContext, port: Option<u16>) -> Result<()> {
    let mut dict = dictionary_value_create().context("dictionary_value_create")?;
    let server = port.map(|port| format!("127.0.0.1:{port}"));
    let entries = match &server {
        // `<-loopback>` removes Chromium's implicit localhost/127/8/[::1] bypass.
        Some(server) => vec![("mode", "fixed_servers"), ("server", server), ("bypass_list", "<-loopback>")],
        None => vec![("mode", "direct")],
    };
    for (key, value) in entries {
        dict.set_string(Some(&CefString::from(key)), Some(&CefString::from(value)));
    }
    let mut value = value_create().context("value_create")?;
    value.set_dictionary(Some(&mut dict));
    let name = CefString::from("proxy");
    // `CefString::default()` marshals as a null pointer and libcef rejects a null `error` out-param.
    let mut error = CefString::from("-");
    if context.set_preference(Some(&name), Some(&mut value), Some(&mut error)) == 0 {
        let (has, can) = (context.has_preference(Some(&name)), context.can_set_preference(Some(&name)));
        bail!("set proxy preference failed (has={has} can_set={can}): {error}");
    }
    Ok(())
}

wrap_request_context_handler! {
    struct ProxyOnInit {
        tunnel: SharedTunnel,
    }

    impl RequestContextHandler {
        fn on_request_context_initialized(&self, context: Option<&mut RequestContext>) {
            let Some(context) = context else { return };
            let port = {
                let mut auth = lock(&self.tunnel);
                auth.initialized = true;
                auth.port()
            };
            if let Err(err) = apply_proxy(context, port) {
                tracing::error!("silo-web: {err:#}");
            }
        }
    }
}

impl Profile {
    /// The global context (`profiles/default`, `Settings::cache_path`). Never tunnelled.
    pub(crate) fn global() -> Result<Profile> {
        let context = request_context_get_global_context().context("no global request context")?;
        let tunnel = SharedTunnel::default();
        lock(&tunnel).initialized = true;
        Ok(Profile { name: DEFAULT.to_owned(), context, tunnel })
    }

    /// Creates the context in `profiles/<name>`; the proxy is applied once it initializes.
    pub(crate) fn create(profiles: &Path, name: &str, tunnel: Option<Tunnel>) -> Result<Profile> {
        let path = profiles.join(dir_name(name)?);
        private_dir(&path)?;
        let settings =
            RequestContextSettings { cache_path: CefString::from(path.to_string_lossy().as_ref()), persist_session_cookies: 1, ..Default::default() };
        let shared = SharedTunnel::default();
        lock(&shared).set(tunnel);
        let tunnel = shared;
        let mut handler = ProxyOnInit::new(Arc::clone(&tunnel));
        let context = request_context_create_context(Some(&settings), Some(&mut handler)).context("request_context_create_context")?;
        Ok(Profile { name: name.to_owned(), context, tunnel })
    }

    /// Re-points the tunnel proxy and its credentials (e.g. after the local proxy restarts), or
    /// goes direct with `None`. UI thread only. On a context that is still initializing this only
    /// records the tunnel; the init handler applies it (CEF rejects prefs before then).
    pub fn set_tunnel(&self, tunnel: Option<Tunnel>) -> Result<()> {
        if self.name == DEFAULT && tunnel.is_some() {
            bail!("the default profile is local-only; tunnels need a server profile");
        }
        let port = tunnel.as_ref().map(|tunnel| tunnel.port);
        let initialized = {
            let mut auth = lock(&self.tunnel);
            auth.set(tunnel);
            auth.initialized
        };
        if initialized {
            apply_proxy(&self.context, port)?;
        }
        Ok(())
    }
}

impl TunnelAuth {
    fn port(&self) -> Option<u16> {
        self.tunnel.as_ref().map(|tunnel| tunnel.port)
    }

    /// New tunnel (or none); resets the answer budget, keeps `initialized`.
    fn set(&mut self, tunnel: Option<Tunnel>) {
        *self = TunnelAuth { tunnel, initialized: self.initialized, ..TunnelAuth::default() };
    }

    /// Credentials for a proxy challenge from `host:port` at `now`: only the profile's own
    /// tunnel proxy, matched exactly, ever gets them, and only within the answer budget.
    fn answer(&mut self, host: &str, port: i32, now: Instant) -> Option<(String, String)> {
        let tunnel = self.tunnel.as_ref()?;
        if host != "127.0.0.1" || port != i32::from(tunnel.port) {
            return None;
        }
        if self.window.is_none_or(|start| now.duration_since(start) >= Duration::from_secs(1)) {
            (self.window, self.answers) = (Some(now), 0);
        }
        self.answers += 1;
        (self.answers <= ANSWERS_PER_SECOND).then(|| (tunnel.user.clone(), tunnel.pass.clone()))
    }
}

pub(crate) fn proxy_credentials(tunnel: &SharedTunnel, host: &str, port: i32) -> Option<(String, String)> {
    lock(tunnel).answer(host, port, Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_credentials_only_for_own_tunnel() {
        let now = Instant::now();
        let mut auth = TunnelAuth::default();
        assert_eq!(auth.answer("127.0.0.1", 4000, now), None);
        auth.set(Some(Tunnel { port: 4000, user: "u".into(), pass: "p".into() }));
        assert_eq!(auth.answer("127.0.0.1", 4000, now), Some(("u".into(), "p".into())));
        for (host, port) in [("127.0.0.1", 4001), ("localhost", 4000), ("10.0.0.1", 4000), ("", 4000)] {
            assert_eq!(auth.answer(host, port, now), None, "{host}:{port}");
        }
        assert!(!format!("{auth:?}").contains("\"p\""));
    }

    #[test]
    fn proxy_answers_are_rate_limited() {
        let now = Instant::now();
        let mut auth = TunnelAuth::default();
        auth.set(Some(Tunnel { port: 1, user: "u".into(), pass: "p".into() }));
        let answered = (0..10).filter(|_| auth.answer("127.0.0.1", 1, now).is_some()).count();
        assert_eq!(answered, ANSWERS_PER_SECOND as usize);
        assert!(auth.answer("127.0.0.1", 1, now + Duration::from_secs(1)).is_some());
        auth.set(auth.tunnel.clone());
        assert!(auth.answer("127.0.0.1", 1, now).is_some());
    }

    #[test]
    fn profile_dirs_are_flat_and_safe() {
        assert_eq!(dir_name("default").unwrap(), "default");
        assert_eq!(dir_name("laptop/work").unwrap(), "laptop_work");
        assert_eq!(dir_name("../x").unwrap(), ".._x");
        assert!(dir_name("..").is_err());
        assert!(dir_name("").is_err());
    }
}
