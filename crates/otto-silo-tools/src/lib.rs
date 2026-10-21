//! Silo's agent tools (`silo-tools`) as otto tools (`docs/AGENTS.md` §7).
//!
//! The tools are registered whether or not silo runs; the first call
//! connects to the local server (never spawning it) and a closed connection
//! is replaced on the next call. Silo's access table and approvals apply to
//! every inner call; otto's policy applies before it.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use otto_tools::{Effects, Loading, OutputPart, Source, Tool, ToolCx, ToolError, ToolErrorKind, ToolOutput, ToolSpec};
use serde_json::Value;
use silo_client::Control;
use silo_tools::{Context, Part, ToolDescriptor, ToolResult, Tools};
use tokio::sync::Mutex;

/// Where the tools connect: the silo home (socket inside) and the client
/// name the server shows.
#[derive(Clone, Debug)]
pub struct Config {
    pub home: PathBuf,
    pub client_name: String,
    /// `SILO_TOKEN`: the connection gets that grant's access only.
    pub token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self { home: silo_proto::paths::home(), client_name: "otto".into(), token: std::env::var("SILO_TOKEN").ok().filter(|t| !t.trim().is_empty()) }
    }
}

/// One lazily opened connection shared by every tool.
struct Shared {
    config: Config,
    live: Mutex<Option<(Control, Arc<Tools>)>>,
}

impl Shared {
    async fn tools(&self, cwd: &std::path::Path) -> Result<Arc<Tools>, ToolError> {
        let mut live = self.live.lock().await;
        if let Some((control, tools)) = live.as_ref()
            && !control.is_closed()
        {
            return Ok(tools.clone());
        }
        silo_client::set_token(self.config.token.clone());
        let socket = silo_proto::paths::socket(&self.config.home);
        let (control, events) = Control::connect(&socket, &self.config.client_name)
            .await
            .map_err(|e| ToolError::new(ToolErrorKind::Failed, format!("silo is not running ({e:#}); start it with `silo up`")))?;
        let cx = Context { cwd: Some(cwd.to_path_buf()), ..Context::default() };
        let tools = Arc::new(Tools::new(control.clone(), events, cx));
        *live = Some((control, tools.clone()));
        Ok(tools)
    }
}

/// Every silo tool, sharing one connection made on first use.
#[must_use]
pub fn tools(config: Config) -> Vec<Arc<dyn Tool>> {
    let shared = Arc::new(Shared { config, live: Mutex::new(None) });
    silo_tools::descriptors().into_iter().map(|d| Arc::new(SiloTool { spec: spec(d), shared: shared.clone() }) as Arc<dyn Tool>).collect()
}

fn spec(d: ToolDescriptor) -> ToolSpec {
    let effects = if d.effects.process || d.effects.write { Effects::PROCESS } else { Effects::READ };
    ToolSpec {
        name: d.name,
        description: d.description,
        input_schema: d.input_schema,
        output_schema: None,
        effects,
        sensitive: vec![],
        loading: if d.deferred { Loading::Deferred } else { Loading::Eager },
        mode: Default::default(),
        presentations: vec![],
        source: Source::Silo,
        exclusive: false,
    }
}

struct SiloTool {
    spec: ToolSpec,
    shared: Arc<Shared>,
}

#[async_trait]
impl Tool for SiloTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let tools = self.shared.tools(&cx.cwd).await?;
        let cancel = cx.cancel.clone();
        match tools.call(&self.spec.name, input, cancel.clone()).await {
            Ok(result) => Ok(output(result)),
            Err(_) if cancel.is_cancelled() => Err(ToolError::new(ToolErrorKind::Cancelled, "cancelled; the block's program was not stopped")),
            Err(error) => Ok(ToolOutput::error(format!("{error:#}"))),
        }
    }
}

fn output(result: ToolResult) -> ToolOutput {
    let content = result
        .content
        .into_iter()
        .map(|part| match part {
            Part::Text(text) => OutputPart::Text { text },
            Part::Json(value) => OutputPart::Json { value },
            Part::Image { mime, data_base64 } => OutputPart::Image { mime, data_base64 },
        })
        .collect();
    ToolOutput { content, is_error: result.is_error, metadata: result.metadata, truncated: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_silo_tool_is_offered() {
        let tools = tools(Config::default());
        assert_eq!(tools.len(), silo_tools::descriptors().len());
        let spec = |name: &str| tools.iter().find(|t| t.spec().name == name).unwrap().spec().clone();
        assert_eq!(spec("terminal_run").source, Source::Silo);
        assert!(!spec("terminal_run").effects.read_only());
        assert!(spec("terminal_read").effects.read_only());
    }

    #[tokio::test]
    async fn no_server_is_a_clear_failure() {
        let home = tempfile::tempdir().unwrap();
        let tools = tools(Config { home: home.path().into(), client_name: "otto test".into(), token: None });
        let tool = tools.iter().find(|t| t.spec().name == "session_list").unwrap();
        let error = tool.call(serde_json::json!({}), ToolCx::new("c1", home.path())).await.unwrap_err();
        assert!(error.message.contains("silo is not running"), "{error}");
    }
}
