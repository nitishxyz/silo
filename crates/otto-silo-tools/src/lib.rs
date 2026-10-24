//! Silo's agent tools (`silo-tools`) as otto tools (`docs/AGENTS.md` §7).
//!
//! The tools are registered whether or not silo runs; the first call
//! connects to the local server (never spawning it) and a closed connection
//! is replaced on the next call. Silo's access table and approvals apply to
//! every inner call; otto's policy applies before it.
//!
//! [`terminals`] runs otto's `shell` commands in parked command blocks next
//! to the agent block showing the session (docs/OTTO.md §6.2); without silo
//! or such a block, pipes run them.
//!
//! [`connect`]'s renderer loads pages for otto's `web_fetch`/`web_search`
//! through silo's `browser_fetch` (a hidden page in the app's off-screen
//! browser host, docs/AGENTS.md).

use std::{collections::BTreeMap, collections::HashMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use otto_tools::{
    Effects, Loading, OutputPart, Source, Tool, ToolCx, ToolError, ToolErrorKind, ToolOutput, ToolSpec,
    builtin::{
        render::{RenderError, RenderRequest, Rendered, Renderer},
        shell::{Finished, Terminal, Terminals},
    },
};
use serde_json::Value;
use silo_client::Control;
use silo_proto::control::{BlockView, Method};
use silo_tools::{Context, Part, ToolDescriptor, ToolResult, Tools, Watch};
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
    /// The agent block last found showing each otto session.
    anchors: Mutex<HashMap<String, String>>,
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

    /// The agent block showing the calling session or, for a subagent, its
    /// nearest ancestor (`cx.sessions`).
    async fn anchor(&self, tools: &Tools, control: &Control, sessions: &[String]) -> Option<String> {
        let first = sessions.first()?;
        let cached = self.anchors.lock().await.get(first).cloned();
        if let Some(block) = cached {
            let view = control.call::<BlockView>(Method::BlockView { block_id: block.clone() }).await;
            if view.is_ok_and(|v| v.block.agent.and_then(|a| a.session_id).is_some_and(|s| sessions.contains(&s))) {
                return Some(block);
            }
        }
        let block = tools.agent_block(sessions).await.ok().flatten()?;
        self.anchors.lock().await.insert(first.clone(), block.clone());
        Some(block)
    }

    async fn control(&self) -> Option<Control> {
        self.live.lock().await.as_ref().map(|(control, _)| control.clone())
    }
}

/// Every silo tool, sharing one connection made on first use.
#[must_use]
pub fn tools(config: Config) -> Vec<Arc<dyn Tool>> {
    silo(config).0
}

/// Every silo tool, and the terminal host for otto's `shell`, sharing one
/// connection made on first use.
#[must_use]
pub fn silo(config: Config) -> (Vec<Arc<dyn Tool>>, Arc<dyn Terminals>) {
    let silo = connect(config);
    (silo.tools, silo.terminals)
}

/// What otto gets from silo, sharing one connection made on first use.
pub struct Silo {
    pub tools: Vec<Arc<dyn Tool>>,
    /// Command blocks for `shell`.
    pub terminals: Arc<dyn Terminals>,
    /// Rendered fetches for `web_fetch`/`web_search` (`browser_fetch`).
    pub renderer: Arc<dyn Renderer>,
}

#[must_use]
pub fn connect(config: Config) -> Silo {
    let shared = Arc::new(Shared { config, live: Mutex::new(None), anchors: Mutex::new(HashMap::new()) });
    let tools =
        silo_tools::descriptors().into_iter().map(|d| Arc::new(SiloTool { spec: spec(d), shared: shared.clone() }) as Arc<dyn Tool>).collect();
    Silo { tools, terminals: Arc::new(SiloTerminals { shared: shared.clone() }), renderer: Arc::new(SiloRenderer { shared }) }
}

struct SiloRenderer {
    shared: Arc<Shared>,
}

#[async_trait]
impl Renderer for SiloRenderer {
    async fn render(&self, request: RenderRequest, cx: &ToolCx) -> Result<Rendered, RenderError> {
        let tools = self.shared.tools(&cx.cwd).await.map_err(|e| RenderError::NoApp(e.message))?;
        let input = serde_json::json!({
            "url": request.url, "timeout_ms": request.timeout_ms, "screenshot": request.screenshot.then_some(true), "selector": request.selector,
        });
        let fetched = tokio::select! {
            biased;
            () = cx.cancel.cancelled() => return Err(RenderError::Failed("cancelled".into())),
            fetched = tools.fetch(&input) => fetched,
        };
        fetched.map(rendered).map_err(render_error)
    }
}

fn rendered(r: silo_tools::BrowserFetchResult) -> Rendered {
    let screenshot = r.screenshot.map(|s| (s.mime, s.data_base64));
    Rendered { url: r.url, title: r.title, status: r.status, html: r.html, timed_out: r.timed_out, blocked: r.blocked, screenshot }
}

fn render_error(error: anyhow::Error) -> RenderError {
    use silo_proto::control::RpcError;
    let message = format!("{error:#}");
    match error.downcast_ref::<RpcError>().map(|e| e.code.as_str()) {
        Some(RpcError::NO_APP | RpcError::METHOD_NOT_FOUND) => RenderError::NoApp(message),
        Some(RpcError::TIMEOUT) => RenderError::Timeout(message),
        Some(RpcError::NAVIGATION) => RenderError::Navigation(message),
        _ => RenderError::Failed(message),
    }
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

    async fn call(&self, mut input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let tools = self.shared.tools(&cx.cwd).await?;
        // One connection serves every session: relative paths start from
        // this session's folder, not the first caller's.
        if self.spec.name == "files_show"
            && let Some(input) = input.as_object_mut()
        {
            input.entry("cwd").or_insert_with(|| Value::String(cx.cwd.to_string_lossy().into_owned()));
        }
        let anchor = match self.shared.control().await {
            Some(control) => self.shared.anchor(&tools, &control, &cx.sessions).await,
            None => None,
        };
        let cancel = cx.cancel.clone();
        match tools.call_in(&self.spec.name, input, cancel.clone(), anchor).await {
            Ok(result) => Ok(output(result)),
            Err(_) if cancel.is_cancelled() => Err(ToolError::new(ToolErrorKind::Cancelled, "cancelled; the block's program was not stopped")),
            Err(error) => Ok(ToolOutput::error(format!("{error:#}"))),
        }
    }
}

struct SiloTerminals {
    shared: Arc<Shared>,
}

#[async_trait]
impl Terminals for SiloTerminals {
    async fn start(&self, command: &str, cwd: &std::path::Path, env: &[(&str, &str)], cx: &ToolCx) -> Result<Option<Arc<dyn Terminal>>, ToolError> {
        // No silo, or no agent block showing the session: pipes.
        let Ok(tools) = self.shared.tools(&cx.cwd).await else { return Ok(None) };
        let Some(control) = self.shared.control().await else { return Ok(None) };
        let Some(owner) = self.shared.anchor(&tools, &control, &cx.sessions).await else { return Ok(None) };
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
        let env: BTreeMap<String, String> = env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        let block = tools
            .spawn_command(&owner, &shell, command, Some(cwd.to_string_lossy().into_owned()), env)
            .await
            .map_err(|e| ToolError::failed(format!("could not start the command in Silo: {e:#}")))?;
        Ok(Some(Arc::new(SiloTerminal { tools, block })))
    }
}

struct SiloTerminal {
    tools: Arc<Tools>,
    block: String,
}

#[async_trait]
impl Terminal for SiloTerminal {
    fn id(&self) -> &str {
        &self.block
    }

    async fn wait(&self, prompt: bool, screen: &mut (dyn for<'s> FnMut(&'s str) + Send)) -> Result<Finished, ToolError> {
        match self.tools.watch(&self.block, prompt, |s: &str| screen(s)).await {
            Ok(Watch::Exited { code, text }) => Ok(Finished::Exited { code, text }),
            Ok(Watch::Waiting { screen }) => Ok(Finished::Waiting { screen }),
            Ok(Watch::Closed) => Ok(Finished::Closed),
            Err(error) => Err(ToolError::failed(format!("{error:#}"))),
        }
    }

    async fn stop(&self) -> Result<String, ToolError> {
        self.tools.stop(&self.block).await.map_err(|e| ToolError::failed(format!("{e:#}")))
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
        let show = spec("files_show");
        assert_eq!((show.loading, show.effects.read_only()), (Loading::Deferred, true), "loaded on demand, like the browser tools");
    }

    #[test]
    fn fetch_errors_map_to_render_errors() {
        use silo_proto::control::RpcError;
        let rpc = |code: &str| anyhow::Error::new(RpcError::new(code, "m"));
        assert!(matches!(render_error(rpc(RpcError::NO_APP)), RenderError::NoApp(_)));
        assert!(matches!(render_error(rpc(RpcError::METHOD_NOT_FOUND)), RenderError::NoApp(_)), "an older server");
        assert!(matches!(render_error(rpc(RpcError::TIMEOUT)), RenderError::Timeout(_)));
        assert!(matches!(render_error(rpc(RpcError::NAVIGATION)), RenderError::Navigation(_)));
        assert!(matches!(render_error(rpc("bad_request")), RenderError::Failed(_)));
        assert!(matches!(render_error(anyhow::anyhow!("url must be an http or https URL")), RenderError::Failed(_)));
    }

    #[tokio::test]
    async fn no_server_is_no_app() {
        let home = tempfile::tempdir().unwrap();
        let silo = connect(Config { home: home.path().into(), client_name: "otto test".into(), token: None });
        let request = RenderRequest { url: "https://example.com".into(), ..Default::default() };
        let error = silo.renderer.render(request, &ToolCx::new("c1", home.path())).await.unwrap_err();
        assert!(matches!(&error, RenderError::NoApp(m) if m.contains("silo is not running")), "{error:?}");
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
