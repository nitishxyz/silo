//! The app's one otto daemon connection (`docs/AGENT_BLOCK.md` §1) and the
//! calls the pane makes that are not plain `Connection` methods.

use otto_client::{ClientError, Connection};
use otto_proto::{AgentInfo, AuthStatus, Method, ModelInfo, ProjectInfo};
use tokio::sync::Mutex;

static CONNECTION: Mutex<Option<Connection>> = Mutex::const_new(None);

/// The shared connection, starting `otto-silo` (next to the app executable)
/// when no daemon runs.
pub async fn connection() -> Result<Connection, ClientError> {
    let mut slot = CONNECTION.lock().await;
    if let Some(connection) = slot.as_ref() {
        return Ok(connection.clone());
    }
    let exe = std::env::current_exe()?;
    let binary = exe.with_file_name(otto_proto::BINARY);
    let connection = otto_client::connect_or_start(&otto_proto::paths::home(), &binary, &["server", "--daemon"]).await?;
    *slot = Some(connection.clone());
    Ok(connection)
}

/// Drop a connection that failed at the transport level; the next call reconnects.
pub async fn forget(error: &ClientError) {
    if matches!(error, ClientError::Closed | ClientError::Io { .. } | ClientError::Timeout) {
        *CONNECTION.lock().await = None;
    }
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

    /// Signed in with a subscription (shown as a marker by the model).
    #[must_use]
    pub fn subscription(&self, provider: &str) -> bool {
        self.auth.iter().any(|a| a.provider == provider && a.signed_in && a.mode == otto_proto::AuthMode::Subscription)
    }

    /// Models of signed-in providers.
    pub fn usable(&self) -> impl Iterator<Item = &ModelInfo> {
        self.models.iter().filter(|m| self.signed_in(m.provider.as_str()))
    }
}

pub async fn catalog(connection: &Connection) -> Result<Catalog, ClientError> {
    let auth = connection.auth_status().await?;
    let models = connection.model_list(None).await.unwrap_or_default();
    let agents = connection.call_typed(Method::AgentsList {}).await.unwrap_or_default();
    Ok(Catalog { auth, models, agents })
}
