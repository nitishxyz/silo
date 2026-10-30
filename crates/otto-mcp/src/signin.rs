//! OAuth sign-in for HTTP servers: [`Mcp::login`] opens a loopback and returns the URL
//! to open; the browser's redirect (or a pasted callback URL, [`Mcp::login_complete`])
//! finishes it, stores the tokens and reconnects the server.
use crate::{Mcp, client::lock, config, oauth::login::Flow};
use otto_llm::auth::oauth::parse_pasted;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::watch;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

/// A started sign-in: open `url`; the browser comes back to `127.0.0.1:port` + `path`.
#[derive(Clone, Debug)]
pub struct SignIn {
    pub login_id: String,
    pub url: String,
    pub port: u16,
    pub path: String,
}

type Done = Option<Result<(), String>>;

pub(crate) struct Pending {
    name: String,
    server_url: String,
    root: Option<PathBuf>,
    state: String,
    flow: Arc<tokio::sync::Mutex<Option<Flow>>>,
    done: watch::Sender<Done>,
    pub(crate) task: tokio::task::JoinHandle<()>,
    at: Instant,
}

impl Mcp {
    /// Begin signing in to HTTP server `name` as configured for `root`.
    pub async fn login(self: &Arc<Self>, root: Option<&Path>, name: &str) -> Result<SignIn, String> {
        let configured = config::configured(&self.user_config_dir, root).remove(name).ok_or_else(|| format!("no server named {name}"))?;
        let config = configured.entry?;
        let url = config.url.clone().filter(|_| config.is_http()).ok_or("only HTTP servers sign in")?;
        if config.headers.keys().any(|k| k.eq_ignore_ascii_case("authorization")) {
            return Err(format!("{name} sends its own Authorization header"));
        }
        let headers: Vec<(String, String)> = config.headers.into_iter().collect();
        let mut flow = crate::oauth::login::begin(&self.shared.tokens, &url, &headers).await?;
        let callback = flow.take_callback().ok_or("no loopback")?;
        let started = SignIn { login_id: uuid(), url: flow.url.clone(), port: flow.port, path: flow.path.clone() };
        let state = flow.state.clone();
        let flow = Arc::new(tokio::sync::Mutex::new(Some(flow)));
        let (done, _) = watch::channel(None);
        let task = {
            let (mcp, flow, done, state, url) = (Arc::downgrade(self), flow.clone(), done.clone(), state.clone(), url.clone());
            tokio::spawn(async move {
                let result = match callback.wait(&state, LOGIN_TIMEOUT).await {
                    Err(error) => Err(error.to_string()),
                    Ok(code) => match mcp.upgrade() {
                        Some(mcp) => mcp.finish(&flow, &url, &code.code).await,
                        None => return,
                    },
                };
                publish(&done, result);
            })
        };
        let mut logins = self.logins.lock().await;
        logins.retain(|_, l| {
            let stale = l.server_url == url || l.at.elapsed() > LOGIN_TIMEOUT;
            if stale {
                l.task.abort();
            }
            !stale
        });
        logins.insert(
            started.login_id.clone(),
            Pending { name: name.to_owned(), server_url: url, root: root.map(Path::to_path_buf), state, flow, done, task, at: Instant::now() },
        );
        Ok(started)
    }

    async fn finish(&self, flow: &tokio::sync::Mutex<Option<Flow>>, url: &str, code: &str) -> Result<(), String> {
        let Some(flow) = flow.lock().await.take() else { return Err("sign-in already finished".into()) };
        flow.finish(&self.shared.tokens, code).await?;
        self.reconnect(url).await;
        Ok(())
    }

    /// Wait for the browser to finish sign-in `login_id`, or finish it with the pasted
    /// callback URL (or code). Returns the server's name and project root.
    pub async fn login_complete(&self, login_id: &str, code: Option<&str>) -> Result<(String, Option<PathBuf>), String> {
        let (name, root, url, state, flow, sender) = {
            let logins = self.logins.lock().await;
            let l = logins.get(login_id).ok_or("no such sign-in")?;
            (l.name.clone(), l.root.clone(), l.server_url.clone(), l.state.clone(), l.flow.clone(), l.done.clone())
        };
        let mut done = sender.subscribe();
        let result = match code {
            Some(code) => {
                let code = parse_pasted(code, &state, true).map_err(|e| e.to_string())?;
                let result = self.finish(&flow, &url, &code.code).await;
                publish(&sender, result);
                let first = done.borrow().clone();
                first.unwrap_or_else(|| Err("sign-in cancelled".into()))
            }
            None => match tokio::time::timeout(LOGIN_TIMEOUT, done.wait_for(Option::is_some)).await {
                Ok(Ok(result)) => result.clone().unwrap_or_else(|| Err("sign-in cancelled".into())),
                Ok(Err(_)) => Err("sign-in cancelled".into()),
                Err(_) => Err("sign-in timed out".into()),
            },
        };
        if let Some(login) = self.logins.lock().await.remove(login_id) {
            login.task.abort();
        }
        result.map(|()| (name, root))
    }

    pub async fn login_cancel(&self, login_id: &str) {
        if let Some(login) = self.logins.lock().await.remove(login_id) {
            login.task.abort();
        }
    }

    /// Forget the tokens of HTTP server `name` as configured for `root`.
    pub async fn logout(&self, root: Option<&Path>, name: &str) -> Result<(), String> {
        let configured = config::configured(&self.user_config_dir, root).remove(name).ok_or_else(|| format!("no server named {name}"))?;
        let url = configured.entry?.url.ok_or("only HTTP servers sign in")?;
        self.shared.tokens.remove(&url)?;
        self.reconnect(&url).await;
        Ok(())
    }

    /// Restart the servers at `url` (new or no tokens).
    async fn reconnect(&self, url: &str) {
        let canonical = crate::oauth::discover::canonical(url);
        let servers: Vec<_> = self
            .servers
            .lock()
            .await
            .values()
            .filter(|s| s.config.url.as_deref().is_some_and(|u| crate::oauth::discover::canonical(u) == canonical))
            .cloned()
            .collect();
        for server in servers {
            lock(&self.reported).remove(&server.name);
            server.restart().await;
        }
    }
}

/// The first outcome wins (the browser's or a pasted code's).
fn publish(done: &watch::Sender<Done>, result: Result<(), String>) {
    done.send_if_modified(|d| {
        let first = d.is_none();
        if first {
            *d = Some(result);
        }
        first
    });
}

fn uuid() -> String {
    otto_llm::auth::oauth::random_state()
}
