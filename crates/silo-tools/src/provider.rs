//! Providing tools through Silo (`tools.provide`, `docs/AGENTS.md` §10):
//! offer tools on a control connection and answer the calls the server
//! forwards (`tools.request`) with `tools.reply`.

use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::bail;
use silo_client::{Control, Events};
use silo_proto::{
    control::{Event, Method},
    tools::{ProvidedTool, ToolOutcome, ToolRequest, ToolsProvideParams, ToolsReplyParams},
};

/// Runs one forwarded call.
pub type Handler = Arc<dyn Fn(ToolRequest) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send>> + Send + Sync>;

/// Wrap an async function as a [`Handler`].
pub fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(ToolRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ToolOutcome> + Send + 'static,
{
    Arc::new(move |request| Box::pin(f(request)))
}

/// Offer `tools` (for `block`, else the server) on `control`, and answer
/// every call with `handler`, each in its own task, until the connection
/// closes or, for a block's tools, the block does. `ready` runs once the
/// tools are offered.
///
/// # Errors
/// A server without provided tools, or a refused offer (an invalid tool, a
/// name another client provides there, too few rights).
pub async fn serve(
    control: Control,
    mut events: Events,
    block: Option<String>,
    tools: Vec<ProvidedTool>,
    handler: Handler,
    ready: impl FnOnce(),
) -> anyhow::Result<()> {
    if !control.peer_supports(silo_proto::compat::feature::TOOLS) {
        bail!("this Silo server cannot take tools (update it)");
    }
    control.subscribe().await?;
    let watched = block.clone();
    control.call_value(Method::ToolsProvide(ToolsProvideParams { block_id: block, tools })).await?;
    ready();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some((_, Event::ToolRequest(request))) => {
                    let (control, handler) = (control.clone(), handler.clone());
                    tasks.spawn(async move {
                        let request_id = request.request_id.clone();
                        let outcome = handler(request).await;
                        let result = serde_json::to_value(outcome).ok();
                        let _ = control.call_value(Method::ToolsReply(ToolsReplyParams { request_id, result, error: None })).await;
                    });
                }
                Some((_, Event::ToolsChanged {})) if watched.is_some() => {
                    // Our block closed: its tools went with it, and so do we.
                    let list: silo_proto::tools::ToolList = control.call(Method::ToolsList {}).await?;
                    let me = control.client_id();
                    if !list.tools.iter().any(|t| t.provider_id == me && t.block_id == watched) {
                        break;
                    }
                }
                Some(_) => {}
                None => break,
            },
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    tasks.abort_all();
    Ok(())
}
