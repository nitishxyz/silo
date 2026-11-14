//! `silo.open`: a layout of blocks built with `window.create`,
//! `block.split` and `layout.ratio`, closed again if a step fails.

use serde::Deserialize;
use serde_json::{Map, Value, json};
use silo_proto::{
    control::RpcError,
    model::{Axis, BlockKind, Layout, WindowInfo},
};

use crate::{Ctx, api::Api};

/// A node of a layout as `silo.layout.*` builds it.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Node {
    Block(Leaf),
    Split { axis: Axis, ratio: f64, first: Box<Node>, second: Box<Node> },
}

/// `silo.layout.block{ kind?, command?, cwd?, url? }`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Leaf {
    #[serde(default, rename = "type")]
    _type: Option<String>,
    #[serde(default)]
    kind: Option<BlockKind>,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

impl Leaf {
    /// The `kind`, `spawn` and `url` params of the call that makes it.
    fn params(&self) -> Map<String, Value> {
        let mut spawn = Map::new();
        if let Some(command) = &self.command {
            spawn.insert("command".into(), json!(command));
        }
        if let Some(cwd) = &self.cwd {
            spawn.insert("cwd".into(), json!(cwd));
        }
        let mut params = Map::new();
        params.insert("kind".into(), json!(self.kind.unwrap_or_default()));
        params.insert("spawn".into(), Value::Object(spawn));
        if let Some(url) = &self.url {
            params.insert("url".into(), json!(url));
        }
        params
    }
}

impl Node {
    /// The leaf made first when this node is built: its top-left one.
    fn first_leaf(&self) -> &Leaf {
        match self {
            Self::Block(leaf) => leaf,
            Self::Split { first, .. } => first.first_leaf(),
        }
    }

    /// # Errors
    /// A ratio outside (0, 1) or a block of an unknown kind.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Block(leaf) if leaf.kind == Some(BlockKind::Unknown) => {
                Err("layout.block: kind is terminal, browser, agent, files or changes".into())
            }
            Self::Block(_) => Ok(()),
            Self::Split { ratio, first, second, .. } => {
                if !(*ratio > 0.0 && *ratio < 1.0) {
                    return Err(format!("layout: ratio {ratio} is not between 0 and 1"));
                }
                first.validate()?;
                second.validate()
            }
        }
    }
}

/// What `silo.open` asks for.
#[derive(Clone, Debug, Deserialize)]
pub struct OpenSpec {
    pub layout: Node,
    #[serde(default)]
    pub session_id: Option<String>,
    /// Build it beside this block (in its window) instead of in a new tab.
    #[serde(default)]
    pub beside: Option<String>,
    /// Which way from `beside`: `horizontal` (right, the default) or
    /// `vertical` (below).
    #[serde(default)]
    pub axis: Option<Axis>,
    /// The new tab's name.
    #[serde(default)]
    pub name: Option<String>,
}

/// What `silo.open` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub window_id: String,
    /// In layout order (reading order).
    pub block_ids: Vec<String>,
}

fn field(created: &Value, key: &str) -> Result<String, RpcError> {
    created.get(key).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| RpcError::internal(format!("the server's answer has no {key}")))
}

/// Where in its window's layout block `id` is: the path to it and, in a
/// float, a block of that float.
fn locate(window: &WindowInfo, id: &str) -> Option<(Vec<usize>, Option<String>)> {
    fn path(layout: &Layout, id: &str, at: &mut Vec<usize>) -> bool {
        match layout {
            Layout::Block { id: leaf } => leaf == id,
            Layout::Split { first, second, .. } => {
                for (i, child) in [first, second].into_iter().enumerate() {
                    at.push(i);
                    if path(child, id, at) {
                        return true;
                    }
                    at.pop();
                }
                false
            }
            Layout::Unknown => false,
        }
    }
    let mut at = Vec::new();
    if path(&window.layout, id, &mut at) {
        return Some((at, None));
    }
    for float in &window.floats {
        let layout = float.layout.clone().unwrap_or_else(|| Layout::block(float.id.clone()));
        let mut at = Vec::new();
        if path(&layout, id, &mut at) {
            return Some((at, Some(id.to_owned())));
        }
    }
    None
}

struct Build<'a> {
    api: &'a Api,
    window: String,
    float: Option<String>,
    made: Vec<String>,
}

impl Build<'_> {
    /// Build `node` where block `at` (its first leaf, already made) is,
    /// at `path` of the window's layout; its blocks in reading order.
    async fn node(&mut self, node: &Node, at: String, path: Vec<usize>) -> Result<Vec<String>, RpcError> {
        let Node::Split { axis, ratio, first, second } = node else {
            return Ok(vec![at]);
        };
        let mut params = second.first_leaf().params();
        params.insert("block_id".into(), json!(at));
        params.insert("axis".into(), json!(axis));
        let created = self.api.call("block.split", Value::Object(params)).await?;
        let new = field(&created, "block_id")?;
        self.made.push(new.clone());
        let mut ratio_params = json!({"window_id": self.window, "path": path, "ratio": ratio});
        if let Some(float) = &self.float {
            ratio_params["float"] = json!(float);
        }
        self.api.call("layout.ratio", ratio_params).await?;
        let mut left = path.clone();
        left.push(0);
        let mut right = path;
        right.push(1);
        let mut blocks = Box::pin(self.node(first, at, left)).await?;
        blocks.extend(Box::pin(self.node(second, new, right)).await?);
        Ok(blocks)
    }
}

/// Build `spec`: a new tab of its session (else `ctx`'s, else the API's),
/// or beside block `beside`. A failure part-way closes what was made.
///
/// # Errors
/// No session to open in, a bad layout, or a call's error.
pub async fn open(api: &Api, ctx: &Ctx, spec: OpenSpec) -> Result<Opened, RpcError> {
    spec.layout.validate().map_err(RpcError::bad_request)?;
    let first = spec.layout.first_leaf().params();
    let (session, window, block, new_window) = match &spec.beside {
        Some(beside) => {
            let mut params = first;
            params.insert("block_id".into(), json!(beside));
            params.insert("axis".into(), json!(spec.axis.unwrap_or(Axis::Horizontal)));
            let created = api.call("block.split", Value::Object(params)).await?;
            (field(&created, "session_id")?, field(&created, "window_id")?, field(&created, "block_id")?, false)
        }
        None => {
            let session = spec
                .session_id
                .clone()
                .or_else(|| ctx.session_id.clone())
                .or_else(|| api.session_id.clone())
                .ok_or_else(|| RpcError::bad_request("silo.open: no session to open in (give session_id or beside)"))?;
            let mut params = first;
            params.insert("session_id".into(), json!(session));
            let created = api.call("window.create", Value::Object(params)).await?;
            (session, field(&created, "window_id")?, field(&created, "block_id")?, true)
        }
    };
    let mut build = Build { api, window: window.clone(), float: None, made: vec![block.clone()] };
    let built = async {
        let mut path = Vec::new();
        if !new_window && matches!(spec.layout, Node::Split { .. }) {
            let view = api.call("session.view", json!({"session_id": session})).await?;
            let windows: Vec<WindowInfo> = serde_json::from_value(view.pointer("/session/windows").cloned().unwrap_or_default())
                .map_err(|e| RpcError::internal(format!("session.view: {e}")))?;
            let (at, float) = windows
                .iter()
                .find(|w| w.id == window)
                .and_then(|w| locate(w, &block))
                .ok_or_else(|| RpcError::internal(format!("block {block} is not in window {window}")))?;
            path = at;
            build.float = float;
        }
        let blocks = build.node(&spec.layout, block.clone(), path).await?;
        if new_window && let Some(name) = &spec.name {
            api.call("window.rename", json!({"window_id": window, "name": name})).await?;
        }
        Ok::<_, RpcError>(blocks)
    }
    .await;
    match built {
        Ok(block_ids) => Ok(Opened { window_id: window, block_ids }),
        Err(error) => {
            if new_window {
                let _ = api.call("window.close", json!({"window_id": window, "force": true})).await;
            } else {
                for block in build.made.iter().rev() {
                    let _ = api.call("block.close", json!({"block_id": block, "force": true})).await;
                }
            }
            Err(error)
        }
    }
}
