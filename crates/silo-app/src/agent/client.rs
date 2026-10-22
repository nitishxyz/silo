//! The app's one otto daemon connection (`docs/AGENT_BLOCK.md` §1) and the
//! calls the pane makes that are not plain `Connection` methods.

use otto_client::{ClientError, Connection};
use otto_proto::{AgentInfo, AuthStatus, ConfigValue, Method, ModelInfo, ProjectInfo};
use tokio::sync::Mutex;

static CONNECTION: Mutex<Option<Connection>> = Mutex::const_new(None);

/// The shared connection, starting `otto-silo` (next to the app executable)
/// when no daemon runs. A cached connection whose daemon went away is
/// dropped here, so the next call reconnects (or starts the daemon again).
pub async fn connection() -> Result<Connection, ClientError> {
    let mut slot = CONNECTION.lock().await;
    if let Some(connection) = slot.as_ref().filter(|c| !c.is_closed()) {
        return Ok(connection.clone());
    }
    let exe = std::env::current_exe()?;
    let binary = exe.with_file_name(otto_proto::BINARY);
    let connection = otto_client::connect_or_start(&otto_proto::paths::home(), &binary, &["server", "--daemon"]).await?;
    *slot = Some(connection.clone());
    Ok(connection)
}

/// The project of `cwd`: its git toplevel, like `otto-silo sessions`.
pub async fn project(connection: &Connection, cwd: Option<&str>) -> Result<ProjectInfo, ClientError> {
    let cwd = cwd.map(str::to_owned).or_else(|| std::env::var("HOME").ok()).unwrap_or_else(|| "/".into());
    let output = tokio::process::Command::new("git").arg("-C").arg(&cwd).args(["rev-parse", "--show-toplevel"]).output().await;
    let root = match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        _ => cwd,
    };
    connection.call_typed(Method::ProjectOpen { root }).await
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
        catalog.agents =
            vec![AgentInfo { name: "plan".into(), description: String::new(), model: Some("anthropic/claude-z".into()), provider: None }];
        assert_eq!(catalog.default_model("plan"), Some(("anthropic".into(), "claude-z".into())));
        assert_eq!(Catalog::default().default_model("build"), None);
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
}
