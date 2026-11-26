//! [`otto_tools::studio::Studio`] over the Silo connection: `studio.*` calls on the Studio block holding a cwd,
//! a card resolved with `silo_proto::studio`'s helpers.
use crate::Shared;
use otto_tools::ToolError;
use otto_tools::studio::{Card, CardInput, CardMention, Edited, Format, Makes, Port, Source, SourceMedia, Status};
use serde_json::{Value, json};
use silo_proto::control::Method;
use silo_proto::studio::{
    Canvas, CanvasEdited, EdgeRole, GenKind, GenStatus, NodeBody, Op, Project, StudioCanvasEditParams, StudioCanvasParams, StudioProjectParams,
    StudioWorld,
};
use std::path::Path;
use std::sync::Arc;

pub(crate) struct SiloStudio {
    pub(crate) shared: Arc<Shared>,
}

fn failed(error: anyhow::Error) -> ToolError {
    ToolError::failed(format!("{error:#}"))
}

/// A root-relative file's mtime, unix ms: the unit of the keys the server compares (`Canvas::source_key`).
pub(crate) fn mtime(root: &Path, path: &str) -> Option<u64> {
    let modified = std::fs::metadata(root.join(path)).ok()?.modified().ok()?;
    u64::try_from(modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis()).ok()
}

fn port(role: EdgeRole) -> Port {
    serde_json::from_value(json!(role)).unwrap_or(Port::Unknown)
}

fn makes(kind: GenKind) -> Makes {
    serde_json::from_value(json!(kind)).unwrap_or(Makes::Unknown)
}

fn media(m: silo_proto::studio::SourceMedia) -> SourceMedia {
    serde_json::from_value(json!(m)).unwrap_or(SourceMedia::Unknown)
}

fn status(s: GenStatus) -> Status {
    serde_json::from_value(json!(s)).unwrap_or(Status::Unknown)
}

impl SiloStudio {
    /// The live connection (the block id says which studio); [`Self::block`] made it.
    async fn live(&self) -> Result<silo_client::Control, ToolError> {
        match self.shared.control().await {
            Some(control) if !control.is_closed() => Ok(control),
            _ => Err(ToolError::failed("silo is not connected (find the Studio block first)")),
        }
    }

    async fn call<T: serde::de::DeserializeOwned>(&self, method: Method) -> Result<T, ToolError> {
        self.live().await?.call(method).await.map_err(failed)
    }

    async fn typed_canvas(&self, block: &str, canvas: &str) -> Result<Canvas, ToolError> {
        self.call(Method::StudioCanvas(StudioCanvasParams { block_id: block.into(), name: canvas.into() })).await
    }

    async fn typed_project(&self, block: &str) -> Result<Project, ToolError> {
        self.call(Method::StudioProject(StudioProjectParams { block_id: block.into() })).await
    }

    /// A Studio block on the studio holding `cwd`, in any session.
    async fn any_block(&self, cwd: &Path) -> Result<Option<String>, ToolError> {
        let Ok(dir) = std::fs::canonicalize(cwd) else { return Ok(None) };
        let dir = if dir.is_dir() { dir } else { dir.parent().map_or(dir.clone(), Path::to_path_buf) };
        let root = silo_tools::studio::root_for(&dir);
        let sessions: Vec<silo_proto::model::SessionSummary> = self.call(Method::SessionList {}).await?;
        for session in sessions {
            let Ok(view) = self.call::<silo_proto::model::SessionView>(Method::SessionView { session_id: session.id }).await else { continue };
            if let Some((block, _, _)) = silo_tools::studio::own_block(&view, None, &root) {
                return Ok(Some(block));
            }
        }
        Ok(None)
    }

    /// The block's root, from its view.
    async fn root(&self, block: &str) -> Result<std::path::PathBuf, ToolError> {
        let view: silo_proto::control::BlockView = self.call(Method::BlockView { block_id: block.into() }).await?;
        let studio = view.block.studio.ok_or_else(|| ToolError::failed(format!("block {block} is not a Studio block")))?;
        Ok(studio.root.into())
    }
}

#[async_trait::async_trait]
impl otto_tools::studio::Studio for SiloStudio {
    async fn block(&self, cwd: &Path, sessions: &[String], open: bool) -> Result<Option<String>, ToolError> {
        let tools = self.shared.tools(cwd).await?;
        let anchor = self.shared.anchor(&tools, tools.control(), sessions).await;
        if let Some(found) = tools.studio_block(anchor.as_deref(), cwd, false).await.map_err(failed)? {
            return Ok(Some(found));
        }
        // No agent block (the app's Run, a restart): a Studio block on the root in any session, not only the default one.
        if anchor.is_none()
            && let Some(found) = self.any_block(cwd).await?
        {
            return Ok(Some(found));
        }
        if !open {
            return Ok(None);
        }
        tools.studio_block(anchor.as_deref(), cwd, true).await.map_err(failed)
    }

    async fn project(&self, block: &str) -> Result<Value, ToolError> {
        self.call(Method::StudioProject(StudioProjectParams { block_id: block.into() })).await
    }

    async fn canvas(&self, block: &str, canvas: &str) -> Result<Value, ToolError> {
        self.call(Method::StudioCanvas(StudioCanvasParams { block_id: block.into(), name: canvas.into() })).await
    }

    async fn world(&self, block: &str) -> Result<Value, ToolError> {
        self.call(Method::StudioWorld(StudioProjectParams { block_id: block.into() })).await
    }

    async fn edit(&self, block: &str, canvas: &str, ops: Vec<Value>) -> Result<Edited, ToolError> {
        let ops: Vec<Op> =
            ops.into_iter().map(serde_json::from_value).collect::<Result<_, _>>().map_err(|e| ToolError::invalid(format!("bad canvas op: {e}")))?;
        let params = StudioCanvasEditParams { block_id: block.into(), name: canvas.into(), ops, base_rev: None };
        let edited: CanvasEdited = self.call(Method::StudioCanvasEdit(params)).await?;
        Ok(Edited { rev: edited.rev, ids: edited.ids, failed: edited.failed.into_iter().map(|f| f.message).collect() })
    }

    async fn card(&self, block: &str, canvas_name: &str, node: &str) -> Result<Card, ToolError> {
        let (canvas, project, root) = (self.typed_canvas(block, canvas_name).await?, self.typed_project(block).await?, self.root(block).await?);
        let found = canvas.node(node).ok_or_else(|| ToolError::invalid(format!("no node {node} on canvas {canvas_name}")))?;
        let NodeBody::Gen { makes: kind, prompt, model, mode, options, status: state, takes, .. } = &found.body else {
            return Err(ToolError::invalid(format!("{node} is not a card (a gen node)")));
        };
        let sections = &project.sections;
        let inputs = canvas
            .inputs_of(node)
            .into_iter()
            .filter_map(|edge| {
                let out = canvas.output_of(&edge.from, sections)?;
                Some(CardInput {
                    port: port(edge.role),
                    from: edge.from.clone(),
                    media: media(out.media),
                    path: out.path,
                    text: out.text,
                    label: out.label,
                    entity: match canvas.node(&edge.from).map(|n| &n.body) {
                        Some(NodeBody::Entity { id }) => Some(id.clone()),
                        _ => None,
                    },
                    pending: out.pending,
                    problem: out.problem,
                })
            })
            .collect();
        let at = |path: &str| mtime(&root, path);
        let keys = canvas.input_keys(node, sections, at);
        let format = project.format_for(&canvas, found.parent.as_deref()).map(|(name, f)| Format {
            name,
            aspect: f.aspect,
            resolution: f.resolution,
            fps: f.fps,
        });
        Ok(Card {
            label: canvas.label_of(node, sections),
            root,
            canvas: canvas_name.into(),
            node: node.into(),
            frame: found.parent.clone(),
            makes: makes(*kind),
            prompt: prompt.clone(),
            model: model.clone(),
            mode: mode.clone(),
            options: options.clone(),
            status: status(*state),
            takes: takes.iter().filter_map(|t| serde_json::to_value(t).ok()).collect(),
            inputs,
            keys,
            format,
            mentions: canvas
                .mentions_of(node, sections)
                .into_iter()
                .map(|(t, label)| {
                    let look = t.look(sections).map(|(_, l)| l);
                    CardMention { token: t.token(), label, path: look.map(|l| l.path.clone()), defines: look.and_then(|l| l.defines.clone()) }
                })
                .collect(),
        })
    }

    async fn sources(&self, block: &str, canvas_name: &str, paths: &[String]) -> Result<Vec<Option<Source>>, ToolError> {
        let canvas = self.typed_canvas(block, canvas_name).await?;
        let world: StudioWorld = self.call(Method::StudioWorld(StudioProjectParams { block_id: block.into() })).await?;
        Ok(paths
            .iter()
            .map(|path| {
                let node = canvas.nodes.iter().find(|n| canvas.output_of(&n.id, &world.sections).is_some_and(|o| o.path.as_deref() == Some(path)));
                if let Some(node) = node {
                    return Some(Source { node: Some(node.id.clone()), entity: None });
                }
                let entity =
                    world.sections.iter().flat_map(|s| &s.entities).find(|e| e.sheet.as_deref() == Some(path) || e.refs.iter().any(|r| r == path))?;
                // An Entity node already there outputs its sheet; a ref of its gets its own Asset node.
                Some(Source { node: None, entity: Some(entity.id.clone()) })
            })
            .collect())
    }
}
