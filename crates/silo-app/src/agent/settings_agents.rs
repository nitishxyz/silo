//! Otto settings › Agents: the agents of a pane's otto for its project. The
//! list (`agents.list`), New, Clone, Delete / Reset, and an editor for one
//! agent (`agents.get` / `agents.save`) with its tools in three columns
//! (`docs/AGENT_BLOCK.md` §9). It only needs an otto and a folder.

use std::{collections::HashSet, future::Future, rc::Rc};

use gpui::{
    Anchor, AnyElement, AppContext as _, Context, Entity, FontWeight, Hsla, IntoElement, Render, SharedString, Subscription, Window, anchored,
    deferred, div, prelude::*, px, rgb,
};
use otto_client::{ClientError, Connection};
use otto_proto::{AgentDef, AgentInfo, AgentScope, ApprovalMode, Method, ToolCatalogEntry};

use crate::agent::{
    client,
    editor::{Editor, EditorColors, EditorEvent},
};
use crate::surface::Glassy as _;
use crate::theme::Continuous as _;
use crate::{
    assets::icon,
    controls::Segmented,
    text_field::{FieldEvent, TextField},
    theme,
    tooltip::Tooltip,
};

// ---- the model ----

/// Where the editor puts a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Column {
    /// Shown to the model (`tools`).
    Default,
    /// Granted behind `load_tools` (`loadable`).
    Loadable,
    Off,
}

impl Column {
    const ALL: [Self; 3] = [Self::Default, Self::Loadable, Self::Off];

    fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Loadable => "Loadable",
            Self::Off => "Off",
        }
    }

    fn about(self) -> &'static str {
        match self {
            Self::Default => "Shown to the model",
            Self::Loadable => "The model loads them when needed",
            Self::Off => "Not granted",
        }
    }
}

/// `*` matches any run of characters (`mcp__github__*`).
pub(crate) fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let [first, .., last] = parts.as_slice() else { return pattern == name };
    if name.len() < first.len() + last.len() || !name.starts_with(first) || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

/// The MCP server a tool comes from.
fn server(tool: &ToolCatalogEntry) -> Option<&str> {
    tool.source.strip_prefix("mcp:")
}

fn group_pattern(server: &str) -> String {
    format!("mcp__{server}__*")
}

/// An agent's `tools` and `loadable`. Not `custom` (both empty): every
/// tool, each with its own loading.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Grant {
    pub tools: Vec<String>,
    pub loadable: Vec<String>,
    pub custom: bool,
}

impl Grant {
    pub(crate) fn of(def: &AgentDef) -> Self {
        Self { tools: def.tools.clone(), loadable: def.loadable.clone(), custom: !def.tools.is_empty() || !def.loadable.is_empty() }
    }

    pub(crate) fn column(&self, tool: &ToolCatalogEntry, disallowed: &[String]) -> Column {
        if disallowed.iter().any(|p| glob(p, &tool.name)) {
            Column::Off
        } else if !self.custom {
            if tool.loading == "deferred" { Column::Loadable } else { Column::Default }
        } else if self.tools.iter().any(|p| glob(p, &tool.name)) {
            Column::Default
        } else if self.loadable.iter().any(|p| glob(p, &tool.name)) {
            Column::Loadable
        } else {
            Column::Off
        }
    }

    pub(crate) fn columns(&self, catalog: &[ToolCatalogEntry], disallowed: &[String]) -> Vec<Column> {
        catalog.iter().map(|t| self.column(t, disallowed)).collect()
    }

    /// The lists giving each catalog tool its column: an MCP server whose
    /// tools share one column is its `mcp__<server>__*` group; patterns of
    /// `previous` that name no catalog tool (a server not running now) stay.
    pub(crate) fn from_columns(catalog: &[ToolCatalogEntry], columns: &[Column], previous: &Self) -> Self {
        let unknown = |list: &[String]| -> Vec<String> { list.iter().filter(|p| !catalog.iter().any(|t| glob(p, &t.name))).cloned().collect() };
        let mut out = Self { tools: unknown(&previous.tools), loadable: unknown(&previous.loadable), custom: true };
        let mut push = |column: Column, entry: String| {
            let list = match column {
                Column::Default => &mut out.tools,
                Column::Loadable => &mut out.loadable,
                Column::Off => return,
            };
            if !list.contains(&entry) {
                list.push(entry);
            }
        };
        let mut servers = HashSet::new();
        for (i, tool) in catalog.iter().enumerate() {
            let Some(server) = server(tool) else {
                push(columns[i], tool.name.clone());
                continue;
            };
            if !servers.insert(server) {
                continue;
            }
            let members: Vec<usize> = (0..catalog.len()).filter(|m| self::server(&catalog[*m]) == Some(server)).collect();
            if members.iter().all(|m| columns[*m] == columns[i]) {
                push(columns[i], group_pattern(server));
            } else {
                for m in members {
                    push(columns[m], catalog[m].name.clone());
                }
            }
        }
        out
    }

    /// `names` moved to `to`: explicit lists from then on.
    pub(crate) fn moved(&self, catalog: &[ToolCatalogEntry], disallowed: &[String], names: &[String], to: Column) -> Self {
        let mut columns = self.columns(catalog, disallowed);
        for (i, tool) in catalog.iter().enumerate() {
            if names.contains(&tool.name) {
                columns[i] = to;
            }
        }
        Self::from_columns(catalog, &columns, self)
    }
}

/// What a column shows: tools, and each MCP server's tools as one group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Chip {
    Tool(usize),
    Group { server: String, members: Vec<usize> },
}

impl Chip {
    fn names(&self, catalog: &[ToolCatalogEntry]) -> Vec<String> {
        match self {
            Self::Tool(i) => vec![catalog[*i].name.clone()],
            Self::Group { members, .. } => members.iter().map(|m| catalog[*m].name.clone()).collect(),
        }
    }
}

pub(crate) fn chips(catalog: &[ToolCatalogEntry], columns: &[Column], column: Column) -> Vec<Chip> {
    let mut out = Vec::new();
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, tool) in catalog.iter().enumerate().filter(|(i, _)| columns[*i] == column) {
        match server(tool) {
            Some(s) => match groups.iter_mut().find(|(g, _)| g == s) {
                Some((_, members)) => members.push(i),
                None => groups.push((s.to_owned(), vec![i])),
            },
            None => out.push(Chip::Tool(i)),
        }
    }
    out.extend(groups.into_iter().map(|(server, members)| Chip::Group { server, members }));
    out
}

/// `<name>-copy`, numbered when taken.
pub(crate) fn clone_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let base = format!("{name}-copy");
    if !taken(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|n| !taken(n)).unwrap_or(base)
}

fn blank() -> AgentDef {
    AgentDef {
        name: String::new(),
        description: String::new(),
        model: None,
        effort: None,
        approval_mode: None,
        prompt: String::new(),
        tools: Vec::new(),
        loadable: Vec::new(),
        disallowed: Vec::new(),
        scope: AgentScope::User,
        overrides_builtin: false,
    }
}

/// The agent being edited and what it was when opened.
#[derive(Clone, Debug)]
pub(crate) struct Draft {
    pub def: AgentDef,
    pub grant: Grant,
    /// As saved (a built-in: as its override would be); `None`: new.
    pub saved: Option<(AgentDef, Grant)>,
    /// A built-in: saving creates an override.
    pub builtin: bool,
}

impl Draft {
    pub(crate) fn new_agent() -> Self {
        Self { def: blank(), grant: Grant::default(), saved: None, builtin: false }
    }

    pub(crate) fn open(mut def: AgentDef) -> Self {
        let builtin = def.scope == AgentScope::Builtin;
        if builtin {
            def.scope = AgentScope::User;
        }
        let grant = Grant::of(&def);
        Self { saved: Some((def.clone(), grant.clone())), def, grant, builtin }
    }

    /// A new agent with `def`'s prompt, tools, model, effort and mode.
    pub(crate) fn cloned(def: &AgentDef, name: String) -> Self {
        let def = AgentDef { name, scope: AgentScope::User, overrides_builtin: false, ..def.clone() };
        Self { grant: Grant::of(&def), def, saved: None, builtin: false }
    }

    pub(crate) fn dirty(&self) -> bool {
        match &self.saved {
            Some((def, grant)) => *def != self.def || *grant != self.grant,
            None => self.def != blank() || self.grant != Grant::default(),
        }
    }

    fn original(&self) -> Option<&AgentDef> {
        self.saved.as_ref().map(|(def, _)| def)
    }

    /// What `agents.save` writes.
    pub(crate) fn to_save(&self) -> AgentDef {
        let mut def = self.def.clone();
        def.name = def.name.trim().to_owned();
        def.description = def.description.trim().to_owned();
        (def.tools, def.loadable) = (self.grant.tools.clone(), self.grant.loadable.clone());
        def
    }

    /// `agents.save`'s `previous_name`: a rename in the same scope.
    pub(crate) fn previous_name(&self) -> Option<String> {
        self.original().filter(|o| !self.builtin && o.scope == self.def.scope && o.name != self.def.name.trim()).map(|o| o.name.clone())
    }

    /// The definition left behind when an agent moves to the other scope.
    pub(crate) fn moved_from(&self) -> Option<(String, AgentScope)> {
        self.original().filter(|o| !self.builtin && o.scope != self.def.scope).map(|o| (o.name.clone(), o.scope))
    }

    /// Why it can't be saved.
    pub(crate) fn problem(&self, taken: impl Fn(&str) -> bool) -> Option<&'static str> {
        let name = self.def.name.trim();
        if name.is_empty() {
            return Some("Name the agent");
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Some("Names use letters, digits, - and _");
        }
        if self.original().is_none_or(|o| o.name != name) && taken(name) {
            return Some("That name is taken");
        }
        if self.grant.custom && self.grant.tools.is_empty() && self.grant.loadable.is_empty() {
            return Some("Grant a tool, or use defaults");
        }
        None
    }

    fn move_tools(&mut self, catalog: &[ToolCatalogEntry], names: &[String], to: Column) {
        if to != Column::Off {
            self.def.disallowed.retain(|d| !names.contains(d));
        }
        self.grant = self.grant.moved(catalog, &self.def.disallowed, names, to);
    }
}

fn scope_label(scope: AgentScope) -> &'static str {
    match scope {
        AgentScope::Builtin => "Built-in",
        AgentScope::User => "Yours",
        AgentScope::Project => "Project",
    }
}

const EFFORTS: [(Option<&str>, &str); 5] =
    [(None, "Default"), (Some("low"), "Low"), (Some("medium"), "Medium"), (Some("high"), "High"), (Some("max"), "Max")];
const MODES: [(Option<ApprovalMode>, &str); 4] = [
    (None, "Session default"),
    (Some(ApprovalMode::Auto), "Auto"),
    (Some(ApprovalMode::AskOnEffects), "Ask"),
    (Some(ApprovalMode::AskAll), "Ask all"),
];

fn effort_label(effort: Option<&str>) -> String {
    EFFORTS.iter().find(|(e, _)| *e == effort).map_or_else(
        || {
            let e = effort.unwrap_or_default();
            let mut c = e.chars();
            c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
        },
        |(_, l)| (*l).to_owned(),
    )
}

fn source_glyph(source: &str) -> &'static str {
    match source {
        "silo" => "laptop",
        s if s.starts_with("mcp:") => "plug",
        s if s.starts_with("ext:") => "agent",
        _ => "bot",
    }
}

/// A model chip's text: the id without its provider.
fn model_label(model: Option<&str>) -> String {
    model.map_or_else(|| "Session model".to_owned(), |m| m.split_once('/').map_or(m, |(_, id)| id).to_owned())
}

/// An error for display: words that look like paths are dropped.
pub(super) fn short_error(error: &str) -> String {
    let text = error.split_whitespace().filter(|w| !w.contains('/')).collect::<Vec<_>>().join(" ");
    let text = text.trim_end_matches(':').trim();
    if text.is_empty() { "Couldn't save".to_owned() } else { text.to_owned() }
}

// ---- the view ----

#[derive(Clone)]
enum Load {
    Idle,
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug)]
struct Listed {
    info: AgentInfo,
    /// A user or project definition replacing a built-in.
    edited: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Menu {
    Model,
    Effort,
    Mode,
}

/// A menu row: its label, checked, and what picking it changes (`None`: a section header).
type MenuOption = (SharedString, bool, Option<Rc<dyn Fn(&mut Draft)>>);

struct Edit {
    draft: Draft,
    /// `agents.get` in flight.
    loading: bool,
    name: Entity<TextField>,
    description: Entity<TextField>,
    prompt: Entity<Editor>,
    search: Entity<TextField>,
    menu: Option<Menu>,
    /// The chip whose move menu is open (column, key).
    chip_menu: Option<(Column, String)>,
    /// MCP servers shown as their tools.
    expanded: HashSet<String>,
    error: Option<String>,
    saving: bool,
    /// Leaving with changes: asking whether to discard them.
    leaving: bool,
    /// Focus the name (and select it) on the next frame.
    focus_name: bool,
    _subs: Vec<Subscription>,
}

/// Tool chips being dragged between columns.
#[derive(Clone)]
struct DraggedTools {
    names: Vec<String>,
    label: SharedString,
    glyph: &'static str,
}

/// The pill following the pointer while chips are dragged.
struct ToolPill {
    label: SharedString,
    glyph: &'static str,
    offset: gpui::Point<gpui::Pixels>,
}

impl Render for ToolPill {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let pill = div()
            .flex()
            .items_center()
            .gap(px(5.0))
            .h(px(22.0))
            .px(px(8.0))
            .glass(6.0)
            .border_color(rgb(theme::accent()))
            .font_family(mono())
            .text_size(px(11.5))
            .text_color(rgb(theme::foreground()))
            .child(icon(self.glyph, 11.0, theme::accent()))
            .child(self.label.clone());
        div().pl(self.offset.x + px(10.0)).pt(self.offset.y + px(10.0)).child(pill)
    }
}

fn mono() -> SharedString {
    theme::summary().1.into()
}

fn hsla(c: u32, alpha: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a *= alpha;
    h
}

pub struct AgentsSettings {
    rt: tokio::runtime::Handle,
    /// The pane's otto.
    target: client::Otto,
    /// The pane's folder, and its project root once loaded.
    cwd: Option<String>,
    root: Option<String>,
    load: Load,
    agents: Vec<Listed>,
    catalog: client::Catalog,
    tools: Vec<ToolCatalogEntry>,
    /// The agent whose Delete / Reset waits for a confirmation.
    confirm: Option<String>,
    /// A failed list action.
    notice: Option<String>,
    edit: Option<Edit>,
    generation: u64,
}

impl AgentsSettings {
    pub fn new(rt: tokio::runtime::Handle, target: client::Otto, cwd: Option<String>) -> Self {
        Self {
            rt,
            target,
            cwd,
            root: None,
            load: Load::Idle,
            agents: Vec::new(),
            catalog: client::Catalog::default(),
            tools: Vec::new(),
            confirm: None,
            notice: None,
            edit: None,
            generation: 0,
        }
    }

    /// Load once the page shows (otto starts only when it's needed).
    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if matches!(self.load, Load::Idle) {
            self.reload(cx);
        }
    }

    /// Esc: close a menu, else leave the editor. Whether it was used.
    pub fn escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.confirm.take().is_some() {
            cx.notify();
            return true;
        }
        let Some(edit) = &mut self.edit else { return false };
        let menu = edit.menu.take().is_some() | edit.chip_menu.take().is_some();
        if !menu && edit.leaving {
            edit.leaving = false;
        } else if !menu {
            self.leave(cx);
            return true;
        }
        cx.notify();
        true
    }

    /// Whether the page may go away now; with unsaved changes it first
    /// asks (asked already: they are discarded).
    pub fn may_leave(&mut self, cx: &mut Context<Self>) -> bool {
        match &mut self.edit {
            Some(edit) if edit.draft.dirty() && !edit.leaving => {
                edit.leaving = true;
                cx.notify();
                false
            }
            _ => {
                self.edit = None;
                true
            }
        }
    }

    fn leave(&mut self, cx: &mut Context<Self>) {
        if self.may_leave(cx) {
            cx.notify();
        }
    }

    /// Run `work` on the shared connection, then `done` here.
    fn run<T, F, Fut>(&self, cx: &mut Context<Self>, work: F, done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static)
    where
        T: Send + 'static,
        F: FnOnce(Connection) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    {
        let target = self.target.clone();
        let task = self.rt.spawn(async move { work(client::connection(&target).await?).await });
        cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(result) => result.map_err(|e| crate::agent::describe(&e)),
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                done(this, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if matches!(self.load, Load::Idle | Load::Failed(_)) {
            self.load = Load::Loading;
        }
        let cwd = self.cwd.clone();
        self.run(
            cx,
            move |c| async move {
                let root = client::project(&c, cwd.as_deref()).await?.root;
                let catalog = client::catalog(&c).await?;
                let tools: Vec<ToolCatalogEntry> = c.call_typed(Method::ToolsCatalog { cwd: Some(root.clone()) }).await?;
                let mut agents = Vec::new();
                for info in catalog.agents.clone() {
                    let edited = info.scope != AgentScope::Builtin
                        && c.call_typed::<AgentDef>(Method::AgentsGet { name: info.name.clone(), cwd: Some(root.clone()) })
                            .await
                            .is_ok_and(|d| d.overrides_builtin);
                    agents.push(Listed { info, edited });
                }
                Ok((root, catalog, tools, agents))
            },
            |this, result, _| match result {
                Ok((root, catalog, tools, agents)) => {
                    this.root = Some(root);
                    this.catalog = catalog;
                    this.tools = tools;
                    this.agents = agents;
                    this.load = Load::Ready;
                }
                Err(error) => this.load = Load::Failed(error),
            },
        );
    }

    /// The listed names (a new name must not be one).
    fn names(&self) -> Vec<String> {
        self.agents.iter().map(|a| a.info.name.clone()).collect()
    }

    fn project_name(&self) -> Option<String> {
        self.root.as_deref().map(crate::agent::project_name)
    }

    /// Open the editor on `draft`.
    fn start_edit(&mut self, draft: Draft, loading: bool, cx: &mut Context<Self>) {
        let name = cx.new(|cx| TextField::new("name", cx));
        let description = cx.new(|cx| TextField::new("What it's for", cx));
        let search = cx.new(|cx| TextField::new("Filter tools", cx));
        let prompt = cx.new(|cx| {
            let mut e = Editor::new("You are…", cx);
            e.multiline = true;
            e
        });
        let subs = vec![
            cx.subscribe(&name, |this, _, event: &FieldEvent, cx| this.field_event(event, |d, text| d.def.name = text, cx)),
            cx.subscribe(&description, |this, _, event: &FieldEvent, cx| this.field_event(event, |d, text| d.def.description = text, cx)),
            cx.subscribe(&search, |this, _, event: &FieldEvent, cx| this.field_event(event, |_, _| {}, cx)),
            cx.subscribe(&prompt, |this, prompt, event: &EditorEvent, cx| match event {
                EditorEvent::Changed => {
                    let text = prompt.read(cx).text().to_owned();
                    if let Some(edit) = &mut this.edit {
                        edit.draft.def.prompt = text;
                        edit.error = None;
                    }
                    cx.notify();
                }
                EditorEvent::Submit => this.save(cx),
                EditorEvent::Escape => {
                    this.escape(cx);
                }
                _ => {}
            }),
        ];
        let focus_name = draft.saved.is_none();
        self.edit = Some(Edit {
            draft,
            loading,
            name,
            description,
            prompt,
            search,
            menu: None,
            chip_menu: None,
            expanded: HashSet::new(),
            error: None,
            saving: false,
            leaving: false,
            focus_name,
            _subs: subs,
        });
        self.sync_fields(cx);
        self.confirm = None;
        self.notice = None;
        cx.notify();
    }

    /// Show the draft in the fields.
    fn sync_fields(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = &self.edit else { return };
        let def = edit.draft.def.clone();
        edit.name.update(cx, |f, cx| f.replace_text(def.name.clone(), cx));
        edit.description.update(cx, |f, cx| f.replace_text(def.description.clone(), cx));
        // `set_text` reports a change, which writes the same prompt back.
        edit.prompt.update(cx, |e, cx| e.set_text(def.prompt.clone(), cx));
    }

    fn field_event(&mut self, event: &FieldEvent, set: impl FnOnce(&mut Draft, String), cx: &mut Context<Self>) {
        match event {
            FieldEvent::Changed(text) => {
                if let Some(edit) = &mut self.edit {
                    set(&mut edit.draft, text.clone());
                    edit.error = None;
                }
                cx.notify();
            }
            FieldEvent::Submit => self.save(cx),
            FieldEvent::Cancel => {
                self.escape(cx);
            }
            FieldEvent::Up | FieldEvent::Down => {}
        }
    }

    fn open(&mut self, name: String, cx: &mut Context<Self>) {
        let mut placeholder = blank();
        placeholder.name.clone_from(&name);
        self.start_edit(Draft::open(placeholder), true, cx);
        self.generation += 1;
        self.reload_tools(cx);
        let generation = self.generation;
        let cwd = self.root.clone();
        self.run(
            cx,
            move |c| async move { c.call_typed::<AgentDef>(Method::AgentsGet { name, cwd }).await },
            move |this, result, cx| {
                if this.generation != generation || this.edit.is_none() {
                    return;
                }
                match result {
                    Ok(def) => {
                        this.start_edit(Draft::open(def), false, cx);
                    }
                    Err(error) => {
                        this.edit = None;
                        this.notice = Some(short_error(&error));
                    }
                }
            },
        );
    }

    fn new_agent(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.reload_tools(cx);
        self.start_edit(Draft::new_agent(), false, cx);
    }

    fn clone_agent(&mut self, name: String, cx: &mut Context<Self>) {
        self.generation += 1;
        self.reload_tools(cx);
        let generation = self.generation;
        let cwd = self.root.clone();
        self.run(
            cx,
            move |c| async move { c.call_typed::<AgentDef>(Method::AgentsGet { name, cwd }).await },
            move |this, result, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(def) => {
                        let names = this.names();
                        let name = clone_name(&def.name, |n| names.iter().any(|x| x == n));
                        this.start_edit(Draft::cloned(&def, name), false, cx);
                    }
                    Err(error) => this.notice = Some(short_error(&error)),
                }
            },
        );
    }

    /// The tool catalog again as the editor opens: an MCP server added since shows.
    fn reload_tools(&mut self, cx: &mut Context<Self>) {
        let cwd = self.root.clone();
        self.run(
            cx,
            move |c| async move { c.call_typed::<Vec<ToolCatalogEntry>>(Method::ToolsCatalog { cwd }).await },
            |this, result, _| {
                if let Ok(tools) = result {
                    this.tools = tools;
                }
            },
        );
    }

    fn delete(&mut self, name: String, scope: AgentScope, cx: &mut Context<Self>) {
        self.confirm = None;
        let cwd = self.root.clone();
        self.run(
            cx,
            move |c| async move { c.call(Method::AgentsDelete { name, scope, cwd }).await },
            |this, result, cx| match result {
                Ok(_) => this.changed(cx),
                Err(error) => this.notice = Some(short_error(&error)),
            },
        );
    }

    /// An agent was saved or deleted: the list and the pickers of every pane on this otto reload.
    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.set_global(client::OttoChanged(self.target.clone()));
        self.reload(cx);
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let names = self.names();
        let Some(edit) = &self.edit else { return };
        if edit.saving || edit.loading || !edit.draft.dirty() {
            return;
        }
        if let Some(problem) = edit.draft.problem(|n| names.iter().any(|x| x == n)) {
            let problem = problem.to_owned();
            if let Some(edit) = &mut self.edit {
                edit.error = Some(problem);
            }
            cx.notify();
            return;
        }
        let agent = edit.draft.to_save();
        let previous_name = edit.draft.previous_name();
        let moved_from = edit.draft.moved_from();
        let cwd = self.root.clone();
        if let Some(edit) = &mut self.edit {
            edit.saving = true;
            edit.error = None;
        }
        let generation = self.generation;
        self.run(
            cx,
            move |c| async move {
                let saved: AgentDef = c.call_typed(Method::AgentsSave { agent, previous_name, cwd: cwd.clone() }).await?;
                if let Some((name, scope)) = moved_from {
                    c.call(Method::AgentsDelete { name, scope, cwd }).await?;
                }
                Ok(saved)
            },
            move |this, result, cx| {
                let current = this.generation == generation;
                match result {
                    Ok(saved) if current => {
                        let expanded = this.edit.as_ref().map(|e| e.expanded.clone()).unwrap_or_default();
                        this.start_edit(Draft::open(saved), false, cx);
                        if let Some(edit) = &mut this.edit {
                            edit.expanded = expanded;
                            edit.focus_name = false;
                        }
                        this.changed(cx);
                    }
                    Ok(_) => this.changed(cx),
                    Err(error) if current => {
                        if let Some(edit) = &mut this.edit {
                            edit.saving = false;
                            edit.error = Some(short_error(&error));
                        }
                    }
                    Err(_) => {}
                }
            },
        );
    }

    fn update_draft(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Draft)) {
        if let Some(edit) = &mut self.edit {
            f(&mut edit.draft);
            edit.error = None;
            edit.menu = None;
            edit.chip_menu = None;
        }
        cx.notify();
    }

    fn move_tools(&mut self, names: &[String], to: Column, cx: &mut Context<Self>) {
        let tools = self.tools.clone();
        self.update_draft(cx, |d| d.move_tools(&tools, names, to));
    }

    // ---- building blocks ----

    fn button(id: impl Into<gpui::ElementId>, label: &'static str, primary: bool, enabled: bool) -> gpui::Stateful<gpui::Div> {
        let (bg, fg) = if primary && enabled { (rgb(theme::accent()), 0x00ff_ffff) } else { (theme::control_fill(), theme::foreground()) };
        div()
            .id(id)
            .flex_none()
            .h(px(26.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .justify_center()
            .continuous(13.0)
            .whitespace_nowrap()
            .text_size(px(12.5))
            .font_weight(FontWeight::MEDIUM)
            .bg(bg)
            .text_color(rgb(fg))
            .when(!primary || !enabled, |b| b.border_1().border_color(rgb(theme::border())))
            .when(!enabled, |b| b.opacity(0.5))
            .when(enabled, |b| b.cursor_pointer().hover(move |s| if primary { s.opacity(0.88) } else { s.bg(theme::row_fill()) }))
            .child(label)
    }

    fn danger_button(id: impl Into<gpui::ElementId>, label: &'static str) -> gpui::Stateful<gpui::Div> {
        Self::button(id, label, false, true).bg(rgb(theme::danger())).text_color(rgb(0x00ff_ffff)).border_0()
    }

    fn icon_button(id: impl Into<gpui::ElementId>, glyph: &'static str, tip: &'static str) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex_none()
            .size(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .continuous(6.0)
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .tooltip(Tooltip::text(tip))
            .child(icon(glyph, 13.0, theme::muted()))
    }

    fn badge(text: impl Into<SharedString>) -> AnyElement {
        div()
            .flex_none()
            .h(px(17.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .continuous(5.0)
            .bg(theme::control_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .text_size(px(10.5))
            .text_color(rgb(theme::muted()))
            .child(text.into())
            .into_any_element()
    }

    pub(super) fn card(rows: Vec<AnyElement>) -> AnyElement {
        let count = rows.len();
        let mut card =
            div().w_full().flex().flex_col().overflow_hidden().continuous(10.0).border_1().border_color(rgb(theme::border())).bg(theme::group_fill());
        for (index, row) in rows.into_iter().enumerate() {
            card = card.child(row);
            if index + 1 < count {
                card = card.child(div().mx(px(14.0)).h(px(1.0)).bg(rgb(theme::border())));
            }
        }
        card.into_any_element()
    }

    pub(super) fn titled(title: &'static str, extra: Option<AnyElement>, body: AnyElement) -> AnyElement {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .px(px(4.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().flex_1().text_size(px(11.5)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(theme::muted())).child(title))
                    .children(extra),
            )
            .child(body)
            .into_any_element()
    }

    fn row(title: &'static str, control: AnyElement) -> AnyElement {
        div()
            .w_full()
            .min_h(px(44.0))
            .px(px(14.0))
            .py(px(8.0))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(div().flex_none().text_size(px(13.0)).font_weight(FontWeight::MEDIUM).child(title))
            .child(div().min_w_0().flex().items_center().justify_end().child(control))
            .into_any_element()
    }

    fn field_box(field: &Entity<TextField>, width: f32) -> AnyElement {
        div()
            .w(px(width))
            .max_w_full()
            .h(px(26.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .continuous(6.0)
            .bg(theme::well_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .text_size(px(12.5))
            .child(field.clone())
            .into_any_element()
    }

    pub(super) fn note(text: impl Into<SharedString>) -> AnyElement {
        div()
            .px(px(4.0))
            .max_w(px(super::settings::DESCRIPTION_MAX))
            .text_size(px(11.5))
            .line_height(px(16.0))
            .text_color(rgb(theme::muted()))
            .child(text.into())
            .into_any_element()
    }

    pub(super) fn error_line(text: String) -> AnyElement {
        div()
            .px(px(4.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(px(12.0))
            .text_color(rgb(theme::danger()))
            .child(icon("alert", 12.0, theme::danger()))
            .child(text)
            .into_any_element()
    }

    // ---- the list ----

    fn render_list(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let header = div()
            .w_full()
            .min_h(px(44.0))
            .px(px(14.0))
            .py(px(10.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_size(px(13.0)).font_weight(FontWeight::MEDIUM).child("Agents"))
                    .child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(match self.project_name() {
                        Some(project) => format!("Built-in, yours and {project}'s"),
                        None => "Built-in and yours".to_owned(),
                    })),
            )
            .child(Self::button("agents-new", "New", false, true).on_click(cx.listener(|this, _, _, cx| this.new_agent(cx))));
        out.push(Self::card(vec![header.into_any_element()]));
        if let Some(notice) = self.notice.clone() {
            out.push(Self::error_line(notice));
        }
        match self.load.clone() {
            Load::Idle | Load::Loading if self.agents.is_empty() => out.push(Self::note("Loading…")),
            Load::Failed(error) => out.push(Self::error_line(short_error(&error))),
            _ => {}
        }
        let rows: Vec<AnyElement> = self.agents.clone().into_iter().enumerate().map(|(i, agent)| self.agent_row(i, agent, cx)).collect();
        if !rows.is_empty() {
            out.push(Self::card(rows));
        }
        out
    }

    fn agent_row(&self, index: usize, agent: Listed, cx: &mut Context<Self>) -> AnyElement {
        let Listed { info, edited } = agent;
        let name = info.name.clone();
        let builtin = info.scope == AgentScope::Builtin;
        let confirming = self.confirm.as_deref() == Some(name.as_str());
        let (verb, tip) = if edited { ("Reset", "Back to the built-in") } else { ("Delete", "Delete") };
        let actions = if confirming {
            let (n, scope) = (name.clone(), info.scope);
            div()
                .flex()
                .gap(px(6.0))
                .child(Self::button(("agents-cancel", index), "Cancel", false, true).on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.confirm = None;
                    cx.notify();
                })))
                .child(Self::danger_button(("agents-confirm", index), verb).on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.delete(n.clone(), scope, cx);
                })))
                .into_any_element()
        } else {
            let clone = name.clone();
            let remove = name.clone();
            div()
                .flex()
                .items_center()
                .gap(px(2.0))
                .child(Self::icon_button(("agents-clone", index), "copy", "Clone").on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.clone_agent(clone.clone(), cx);
                })))
                .when(!builtin, |el| {
                    el.child(Self::icon_button(("agents-delete", index), if edited { "rotate-ccw" } else { "close" }, tip).on_click(cx.listener(
                        move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.confirm = Some(remove.clone());
                            cx.notify();
                        },
                    )))
                })
                .when(builtin, |el| el.child(div().size(px(24.0))))
                .into_any_element()
        };
        let description: SharedString = if confirming {
            if edited { "Reset to the built-in? Your changes go.".into() } else { format!("Delete {name}?").into() }
        } else {
            info.description.clone().into()
        };
        let model = model_label(info.model.as_deref());
        let open = name.clone();
        div()
            .id(("agents-row", index))
            .w_full()
            .min_h(px(48.0))
            .px(px(14.0))
            .py(px(8.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .on_click(cx.listener(move |this, _, _, cx| this.open(open.clone(), cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(div().text_size(px(13.0)).font_weight(FontWeight::MEDIUM).child(name.clone()))
                            .child(Self::badge(scope_label(info.scope)))
                            .when(edited, |el| el.child(Self::badge("Edited"))),
                    )
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(rgb(if confirming { theme::foreground() } else { theme::muted() }))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(description),
                    ),
            )
            .child(
                div()
                    .id(("agents-model", index))
                    .flex_none()
                    .max_w(px(150.0))
                    .h(px(20.0))
                    .px(px(7.0))
                    .flex()
                    .items_center()
                    .continuous(6.0)
                    .bg(theme::well_fill())
                    .font_family(mono())
                    .text_size(px(11.0))
                    .text_color(rgb(theme::muted()))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(model),
            )
            .child(actions)
            .into_any_element()
    }

    // ---- the editor ----

    fn menu_trigger(&self, menu: Menu, label: String, cx: &mut Context<Self>) -> AnyElement {
        let open = self.edit.as_ref().is_some_and(|e| e.menu == Some(menu));
        let trigger = div()
            .id(("agents-menu", menu as usize))
            .h(px(26.0))
            .min_w(px(180.0))
            .max_w(px(260.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .continuous(7.0)
            .bg(theme::control_fill())
            .border_1()
            .border_color(rgb(if open { theme::accent() } else { theme::border() }))
            .cursor_pointer()
            .hover(|s| s.bg(theme::row_fill()))
            .child(div().flex_1().min_w_0().text_ellipsis().whitespace_nowrap().overflow_hidden().text_size(px(12.5)).child(label))
            .child(icon("chevron", 11.0, theme::muted()))
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(edit) = &mut this.edit {
                    edit.menu = if edit.menu == Some(menu) { None } else { Some(menu) };
                    edit.chip_menu = None;
                }
                cx.notify();
            }));
        let mut root = div().relative().child(trigger);
        if open {
            root = root.child(div().absolute().top(px(30.0)).right_0().child(
                deferred(anchored().anchor(Anchor::TopRight).snap_to_window_with_margin(px(8.0)).child(self.menu_list(menu, cx))).with_priority(1),
            ));
        }
        root.into_any_element()
    }

    /// A menu's rows: section headers (`None` action) and choices.
    fn menu_options(&self, menu: Menu) -> Vec<MenuOption> {
        let Some(edit) = &self.edit else { return Vec::new() };
        let def = &edit.draft.def;
        let mut out: Vec<MenuOption> = Vec::new();
        match menu {
            Menu::Model => {
                out.push(("Session model".into(), def.model.is_none(), Some(Rc::new(|d: &mut Draft| d.def.model = None))));
                let mut group = None;
                for m in self.catalog.usable() {
                    let provider = m.provider.as_str().to_owned();
                    if group.as_deref() != Some(provider.as_str()) {
                        out.push((crate::agent::provider_name(&provider).into(), false, None));
                        group = Some(provider.clone());
                    }
                    let spec = format!("{provider}/{}", m.id);
                    let checked = def.model.as_deref().is_some_and(|d| d == spec || d == m.id);
                    out.push((m.id.clone().into(), checked, Some(Rc::new(move |d: &mut Draft| d.def.model = Some(spec.clone())))));
                }
            }
            Menu::Effort => {
                for (effort, label) in EFFORTS {
                    let checked = def.effort.as_deref() == effort;
                    out.push((label.into(), checked, Some(Rc::new(move |d: &mut Draft| d.def.effort = effort.map(str::to_owned)))));
                }
            }
            Menu::Mode => {
                for (mode, label) in MODES {
                    out.push((label.into(), def.approval_mode == mode, Some(Rc::new(move |d: &mut Draft| d.def.approval_mode = mode))));
                }
            }
        }
        out
    }

    fn menu_list(&self, menu: Menu, cx: &mut Context<Self>) -> AnyElement {
        let _glass = crate::surface::enter();
        let mut list = div()
            .id(("agents-menu-list", menu as usize))
            .w(px(240.0))
            .max_h(px(300.0))
            .overflow_y_scroll()
            .p(px(4.0))
            .flex()
            .flex_col()
            .gap(px(1.0))
            .glass(9.0)
            .font_family(theme::ui_font())
            .text_color(rgb(theme::foreground()))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if let Some(edit) = &mut this.edit {
                    edit.menu = None;
                }
                cx.notify();
            }));
        for (i, (label, checked, pick)) in self.menu_options(menu).into_iter().enumerate() {
            let Some(pick) = pick else {
                list = list.child(
                    div().h(px(24.0)).px(px(8.0)).flex().items_end().pb(px(3.0)).text_size(px(11.0)).text_color(rgb(theme::muted())).child(label),
                );
                continue;
            };
            list = list.child(
                div()
                    .id(("agents-menu-option", i))
                    .h(px(28.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .continuous(6.0)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::row_fill()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_size(px(12.5))
                            .when(menu == Menu::Model && i > 0, |el| el.font_family(mono()).text_size(px(12.0)))
                            .child(label),
                    )
                    .child(div().w(px(12.0)).children(checked.then(|| icon("check", 12.0, theme::accent()))))
                    .on_click(cx.listener(move |this, _, _, cx| this.update_draft(cx, |d| pick(d)))),
            );
        }
        list.into_any_element()
    }

    fn render_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let names = self.names();
        let project = self.project_name();
        let Some(edit) = &mut self.edit else { return Vec::new() };
        if std::mem::take(&mut edit.focus_name) {
            let field = edit.name.clone();
            let focus = field.read(cx).focus.clone();
            window.focus(&focus, cx);
            field.update(cx, |f, cx| f.select_all(cx));
        }
        let draft = edit.draft.clone();
        let dirty = draft.dirty();
        let problem = draft.problem(|n| names.iter().any(|x| x == n));
        let (loading, saving, leaving) = (edit.loading, edit.saving, edit.leaving);
        let error = edit.error.clone();
        let (name_field, description_field, prompt, search) = (edit.name.clone(), edit.description.clone(), edit.prompt.clone(), edit.search.clone());
        let can_save = dirty && !loading && !saving;
        let mut out = Vec::new();

        let title: SharedString = if draft.def.name.trim().is_empty() { "New agent".into() } else { draft.def.name.trim().to_owned().into() };
        let toolbar = div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(Self::icon_button("agents-back", "back", "Agents").on_click(cx.listener(|this, _, _, cx| this.leave(cx))))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .when(draft.builtin, |el| el.child(Self::badge("Built-in")))
            .when(draft.saved.is_none(), |el| el.child(Self::badge("New")))
            .child(
                Self::button("agents-save", if saving { "Saving…" } else { "Save" }, true, can_save)
                    .when(can_save, |b| b.on_click(cx.listener(|this, _, _, cx| this.save(cx))))
                    .when_some(problem.filter(|_| can_save), |b, p| b.tooltip(Tooltip::text(p))),
            );
        out.push(toolbar.into_any_element());
        if leaving {
            out.push(
                div()
                    .w_full()
                    .px(px(12.0))
                    .py(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .continuous(8.0)
                    .bg(theme::hover_fill())
                    .child(div().flex_1().text_size(px(12.5)).child("Discard your changes?"))
                    .child(Self::button("agents-keep", "Keep editing", false, true).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(edit) = &mut this.edit {
                            edit.leaving = false;
                        }
                        cx.notify();
                    })))
                    .child(Self::danger_button("agents-discard", "Discard").on_click(cx.listener(|this, _, _, cx| {
                        this.edit = None;
                        cx.notify();
                    })))
                    .into_any_element(),
            );
        }
        if let Some(error) = error {
            out.push(Self::error_line(error));
        }
        if loading {
            out.push(Self::note("Loading…"));
            return out;
        }

        let scope = {
            let selected = usize::from(draft.def.scope == AgentScope::Project);
            let project_label = project.as_ref().map_or_else(|| "Project".to_owned(), |p| format!("Project · {p}"));
            let entity = cx.entity().downgrade();
            Segmented::new("agents-scope", ["Yours".to_owned(), project_label], selected)
                .segment_width(if project.is_some() { 120.0 } else { 80.0 })
                .when(self.root.is_none(), |s| s.unavailable(1, "Open a project first"))
                .on_select(move |index, _, cx| {
                    let scope = if index == 1 { AgentScope::Project } else { AgentScope::User };
                    let _ = entity.update(cx, |this, cx| this.update_draft(cx, |d| d.def.scope = scope));
                })
                .into_any_element()
        };
        let model = self.menu_trigger(Menu::Model, model_label(draft.def.model.as_deref()), cx);
        let effort = self.menu_trigger(Menu::Effort, effort_label(draft.def.effort.as_deref()), cx);
        let mode = self.menu_trigger(
            Menu::Mode,
            MODES.iter().find(|(m, _)| *m == draft.def.approval_mode).map_or("Session default", |(_, l)| l).to_owned(),
            cx,
        );
        out.push(Self::card(vec![
            Self::row("Name", Self::field_box(&name_field, 220.0)),
            Self::row("Description", Self::field_box(&description_field, 300.0)),
            Self::row("Saved in", scope),
            Self::row("Model", model),
            Self::row("Effort", effort),
            Self::row("Approvals", mode),
        ]));

        let colors = EditorColors {
            text: rgb(theme::foreground()).into(),
            placeholder: rgb(theme::muted()).into(),
            caret: rgb(theme::foreground()).into(),
            selection: hsla(theme::selection(), 0.6),
        };
        let prompt_box = div()
            .w_full()
            .min_h(px(120.0))
            .px(px(12.0))
            .py(px(10.0))
            .continuous(10.0)
            .bg(theme::well_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .font_family(mono())
            .text_size(px(12.5))
            .line_height(px(18.0))
            .child(Editor::element(&prompt, 28, colors, cx));
        out.push(Self::titled("Prompt", None, prompt_box.into_any_element()));
        out.push(self.render_tools(&draft, &search, cx));
        out
    }

    fn render_tools(&self, draft: &Draft, search: &Entity<TextField>, cx: &mut Context<Self>) -> AnyElement {
        let query = search.read(cx).text.trim().to_lowercase();
        let custom = draft.grant.custom;
        let extra = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(if custom { "Custom" } else { "Defaults" }))
            .child(
                Self::button("agents-tools-defaults", "Use defaults", false, custom)
                    .h(px(22.0))
                    .px(px(9.0))
                    .text_size(px(11.5))
                    .when(custom, |b| b.on_click(cx.listener(|this, _, _, cx| this.update_draft(cx, |d| d.grant = Grant::default())))),
            )
            .into_any_element();
        let columns = draft.grant.columns(&self.tools, &draft.def.disallowed);
        let filter = div()
            .w_full()
            .h(px(28.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .continuous(7.0)
            .bg(theme::well_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .text_size(px(12.5))
            .child(icon("settings-search", 12.0, theme::muted()))
            .child(div().flex_1().min_w_0().overflow_hidden().child(search.clone()));
        let mut grid = div().w_full().flex().gap(px(8.0));
        for column in Column::ALL {
            grid = grid.child(self.render_column(column, &columns, &query, cx));
        }
        let body = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(filter)
            .child(grid)
            .when(self.tools.is_empty(), |el| el.child(Self::note("No tools listed.")))
            .when(!custom, |el| el.child(Self::note("Every tool, each with its own loading. Moving one makes the lists yours.")));
        Self::titled("Tools", Some(extra), body.into_any_element())
    }

    fn render_column(&self, column: Column, columns: &[Column], query: &str, cx: &mut Context<Self>) -> AnyElement {
        let catalog = &self.tools;
        let Some(edit) = &self.edit else { return div().into_any_element() };
        let matches =
            |i: usize| query.is_empty() || catalog[i].name.to_lowercase().contains(query) || catalog[i].description.to_lowercase().contains(query);
        let all = chips(catalog, columns, column);
        let count = columns.iter().filter(|c| **c == column).count();
        let mut wrap = div().w_full().flex().flex_wrap().gap(px(4.0));
        for chip in all {
            match &chip {
                Chip::Tool(i) => {
                    if matches(*i) {
                        wrap = wrap.child(self.render_chip(column, &chip, false, cx));
                    }
                }
                Chip::Group { server, members } => {
                    let server_hit = !query.is_empty() && server.to_lowercase().contains(query);
                    let shown: Vec<usize> = members.iter().copied().filter(|m| server_hit || matches(*m)).collect();
                    if shown.is_empty() {
                        continue;
                    }
                    wrap = wrap.child(self.render_chip(column, &chip, false, cx));
                    if edit.expanded.contains(server) || (!query.is_empty() && !server_hit) {
                        for m in shown {
                            wrap = wrap.child(self.render_chip(column, &Chip::Tool(m), true, cx));
                        }
                    }
                }
            }
        }
        div()
            .id(("agents-column", column as usize))
            .flex_1()
            .min_w_0()
            .min_h(px(132.0))
            .p(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .continuous(10.0)
            .bg(theme::group_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .drag_over::<DraggedTools>(|s, _, _, _| s.bg(rgb(theme::accent()).opacity(0.08)).border_color(rgb(theme::accent())))
            .on_drop(cx.listener(move |this, dragged: &DraggedTools, _, cx| this.move_tools(&dragged.names, column, cx)))
            .child(
                div()
                    .id(("agents-column-title", column as usize))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .tooltip(Tooltip::text(column.about()))
                    .child(div().text_size(px(12.0)).font_weight(FontWeight::SEMIBOLD).child(column.label()))
                    .child(div().text_size(px(11.0)).text_color(rgb(theme::muted())).child(count.to_string())),
            )
            .child(wrap)
            .into_any_element()
    }

    fn render_chip(&self, column: Column, chip: &Chip, member: bool, cx: &mut Context<Self>) -> AnyElement {
        let catalog = &self.tools;
        let Some(edit) = &self.edit else { return div().into_any_element() };
        let (key, label, glyph, tip, group) = match chip {
            Chip::Tool(i) => {
                let t = &catalog[*i];
                let mut tip = t.description.clone();
                if !t.effects.is_empty() {
                    tip.push_str(&format!("\nEffects: {}", t.effects.join(", ")));
                }
                let label = match server(t) {
                    Some(s) if member => t.name.strip_prefix(&format!("mcp__{s}__")).unwrap_or(&t.name).to_owned(),
                    _ => t.name.clone(),
                };
                (t.name.clone(), label, source_glyph(&t.source), tip, None)
            }
            Chip::Group { server, members } => {
                let tip = format!("{} tools from {server}", members.len());
                (group_pattern(server), group_pattern(server), "plug", tip, Some((server.clone(), members.len())))
            }
        };
        let names = chip.names(catalog);
        let menu_open = edit.chip_menu.as_ref().is_some_and(|(c, k)| *c == column && *k == key);
        let expanded = group.as_ref().is_some_and(|(s, _)| edit.expanded.contains(s));
        let dragged = DraggedTools { names: names.clone(), label: label.clone().into(), glyph };
        let menu_key = key.clone();
        let element = div()
            .id(SharedString::from(format!("agents-chip-{}-{key}", column as usize)))
            .h(px(22.0))
            .max_w_full()
            .px(px(7.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .continuous(6.0)
            .bg(if member { theme::well_fill() } else { theme::control_fill() })
            .border_1()
            .border_color(rgb(if menu_open { theme::accent() } else { theme::border() }))
            .font_family(mono())
            .text_size(px(11.5))
            .cursor_pointer()
            .hover(|s| s.bg(theme::row_fill()))
            .when(!tip.is_empty() && !menu_open, |el| el.tooltip(Tooltip::text(tip)))
            .child(icon(glyph, 11.0, theme::muted()))
            .child(div().min_w_0().whitespace_nowrap().overflow_hidden().text_ellipsis().child(label))
            .when_some(group.clone(), |el, (_, count)| {
                el.child(div().text_color(rgb(theme::muted())).child(count.to_string())).child(icon(
                    if expanded { "chevron-up" } else { "chevron-down" },
                    10.0,
                    theme::muted(),
                ))
            })
            .on_drag(dragged, |d, offset, _, cx| cx.new(|_| ToolPill { label: d.label.clone(), glyph: d.glyph, offset }))
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(edit) = &mut this.edit {
                    let open = edit.chip_menu.as_ref().is_some_and(|(c, k)| *c == column && *k == menu_key);
                    edit.chip_menu = if open { None } else { Some((column, menu_key.clone())) };
                    edit.menu = None;
                }
                cx.notify();
            }));
        let mut root = div().relative().max_w_full().child(element);
        if menu_open {
            let _glass = crate::surface::enter();
            let mut list = div()
                .id("agents-chip-menu")
                .w(px(170.0))
                .p(px(4.0))
                .flex()
                .flex_col()
                .gap(px(1.0))
                .glass(9.0)
                .font_family(theme::ui_font())
                .text_size(px(12.5))
                .text_color(rgb(theme::foreground()))
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    if let Some(edit) = &mut this.edit {
                        edit.chip_menu = None;
                    }
                    cx.notify();
                }));
            let item = |id: usize, text: String| {
                div()
                    .id(("agents-chip-option", id))
                    .h(px(28.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .continuous(6.0)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::row_fill()))
                    .child(text)
            };
            for (i, to) in Column::ALL.into_iter().enumerate().filter(|(_, c)| *c != column) {
                let names = names.clone();
                list = list
                    .child(item(i, format!("Move to {}", to.label())).on_click(cx.listener(move |this, _, _, cx| this.move_tools(&names, to, cx))));
            }
            if let Some((server, _)) = group {
                list = list.child(item(9, if expanded { "Hide tools" } else { "Show tools" }.to_owned()).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if let Some(edit) = &mut this.edit {
                            if !edit.expanded.remove(&server) {
                                edit.expanded.insert(server.clone());
                            }
                            edit.chip_menu = None;
                        }
                        cx.notify();
                    },
                )));
            }
            root = root.child(
                div()
                    .absolute()
                    .top(px(25.0))
                    .left_0()
                    .child(deferred(anchored().anchor(Anchor::TopLeft).snap_to_window_with_margin(px(8.0)).child(list)).with_priority(1)),
            );
        }
        root.into_any_element()
    }
}

impl Render for AgentsSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_loaded(cx);
        let children = if self.edit.is_some() { self.render_editor(window, cx) } else { self.render_list(cx) };
        div().w_full().flex().flex_col().gap(px(14.0)).children(children)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, source: &str, loading: &str) -> ToolCatalogEntry {
        ToolCatalogEntry { name: name.into(), description: String::new(), source: source.into(), loading: loading.into(), effects: Vec::new() }
    }

    fn catalog() -> Vec<ToolCatalogEntry> {
        vec![
            tool("read", "builtin", "default"),
            tool("shell", "builtin", "default"),
            tool("web_search", "builtin", "deferred"),
            tool("mcp__github__issues", "mcp:github", "deferred"),
            tool("mcp__github__prs", "mcp:github", "deferred"),
        ]
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn agents_globs_match_prefixes_and_names() {
        assert!(glob("read", "read") && !glob("read", "reader"));
        assert!(glob("mcp__github__*", "mcp__github__prs"));
        assert!(!glob("mcp__github__*", "mcp__gitlab__prs"));
        assert!(glob("*", "anything") && glob("a*c*e", "abcde") && !glob("a*c*e", "abde"));
    }

    #[test]
    fn agents_columns_follow_the_lists() {
        let catalog = catalog();
        use Column::{Default as D, Loadable as L, Off as O};
        assert_eq!(Grant::default().columns(&catalog, &[]), vec![D, D, L, L, L], "defaults: each tool's own loading");
        let grant = Grant { tools: strings(&["read", "mcp__github__*"]), loadable: strings(&["web_search"]), custom: true };
        assert_eq!(grant.columns(&catalog, &[]), vec![D, O, L, D, D]);
        assert_eq!(grant.columns(&catalog, &strings(&["mcp__github__prs"])), vec![D, O, L, D, O], "disallowed is off");
        let both = Grant { tools: strings(&["read"]), loadable: strings(&["read"]), custom: true };
        assert_eq!(both.column(&catalog[0], &[]), D, "tools wins");
    }

    #[test]
    fn agents_lists_rebuild_from_columns_with_mcp_groups() {
        let catalog = catalog();
        use Column::{Default as D, Loadable as L, Off as O};
        let grant = Grant::from_columns(&catalog, &[D, O, L, L, L], &Grant::default());
        assert_eq!(grant, Grant { tools: strings(&["read"]), loadable: strings(&["web_search", "mcp__github__*"]), custom: true });
        let split = Grant::from_columns(&catalog, &[D, D, O, D, O], &Grant::default());
        assert_eq!(split.tools, strings(&["read", "shell", "mcp__github__issues"]), "a split server lists its tools");
        assert!(split.loadable.is_empty());
        // A pattern naming no listed tool (a server not running) stays.
        let previous = Grant { tools: strings(&["mcp__linear__*", "read"]), loadable: Vec::new(), custom: true };
        assert_eq!(Grant::from_columns(&catalog, &[D, O, O, O, O], &previous).tools, strings(&["mcp__linear__*", "read"]));
        // Round trip.
        let columns = grant.columns(&catalog, &[]);
        assert_eq!(Grant::from_columns(&catalog, &columns, &grant), grant);
    }

    #[test]
    fn agents_moving_a_tool_makes_lists_and_use_defaults_clears_them() {
        let catalog = catalog();
        let moved = Grant::default().moved(&catalog, &[], &strings(&["shell"]), Column::Off);
        assert_eq!(moved, Grant { tools: strings(&["read"]), loadable: strings(&["web_search", "mcp__github__*"]), custom: true });
        let one = moved.moved(&catalog, &[], &strings(&["mcp__github__prs"]), Column::Default);
        assert_eq!(one.tools, strings(&["read", "mcp__github__prs"]));
        assert_eq!(one.loadable, strings(&["web_search", "mcp__github__issues"]));
        let back = one.moved(&catalog, &[], &strings(&["mcp__github__prs"]), Column::Loadable);
        assert_eq!(back, moved, "the group comes back whole");

        let mut draft = Draft::open(AgentDef { name: "x".into(), ..blank() });
        draft.move_tools(&catalog, &strings(&["read", "shell", "web_search", "mcp__github__issues", "mcp__github__prs"]), Column::Off);
        assert!(draft.grant.custom && draft.grant.tools.is_empty());
        assert_eq!(draft.problem(|_| false), Some("Grant a tool, or use defaults"));
        draft.grant = Grant::default();
        assert_eq!(draft.problem(|_| false), None);
        assert!(!draft.dirty(), "use defaults is where it started");
        assert!(draft.to_save().tools.is_empty() && draft.to_save().loadable.is_empty());
    }

    #[test]
    fn agents_chips_group_mcp_servers() {
        let catalog = catalog();
        let columns = Grant::default().columns(&catalog, &[]);
        assert_eq!(chips(&catalog, &columns, Column::Default), vec![Chip::Tool(0), Chip::Tool(1)]);
        let loadable = chips(&catalog, &columns, Column::Loadable);
        assert_eq!(loadable, vec![Chip::Tool(2), Chip::Group { server: "github".into(), members: vec![3, 4] }]);
        assert_eq!(loadable[1].names(&catalog), strings(&["mcp__github__issues", "mcp__github__prs"]));
    }

    #[test]
    fn agents_clone_names_count_up() {
        assert_eq!(clone_name("build", |_| false), "build-copy");
        let taken = ["build-copy", "build-copy-2"];
        assert_eq!(clone_name("build", |n| taken.contains(&n)), "build-copy-3");
        let def = AgentDef { name: "build".into(), prompt: "p".into(), model: Some("a/b".into()), scope: AgentScope::Builtin, ..blank() };
        let draft = Draft::cloned(&def, "build-copy".into());
        assert_eq!((draft.def.prompt.as_str(), draft.def.model.as_deref(), draft.def.scope), ("p", Some("a/b"), AgentScope::User));
        assert!(draft.saved.is_none() && draft.dirty(), "a clone is new and unsaved");
    }

    #[test]
    fn agents_dirty_state_and_saving() {
        let def = AgentDef { name: "review".into(), description: "Reviews".into(), ..blank() };
        let mut draft = Draft::open(def.clone());
        assert!(!draft.dirty());
        draft.def.prompt = "Be brief".into();
        assert!(draft.dirty());
        draft.def.prompt.clear();
        assert!(!draft.dirty(), "back to as saved");

        assert!(!Draft::new_agent().dirty(), "a blank new agent has nothing to lose");
        assert_eq!(Draft::new_agent().problem(|_| false), Some("Name the agent"));

        draft.def.name = "review2".into();
        assert_eq!(draft.previous_name().as_deref(), Some("review"));
        assert_eq!(draft.problem(|n| n == "review2"), Some("That name is taken"));
        assert_eq!(draft.problem(|n| n == "review"), None, "its own name is fine");
        draft.def.name = "bad name".into();
        assert!(draft.problem(|_| false).is_some());
        draft.def.name = "review".into();
        draft.def.scope = AgentScope::Project;
        assert_eq!(draft.previous_name(), None);
        assert_eq!(draft.moved_from(), Some(("review".into(), AgentScope::User)));

        let builtin = Draft::open(AgentDef { name: "build".into(), scope: AgentScope::Builtin, ..blank() });
        assert!(builtin.builtin && !builtin.dirty(), "a built-in opens unchanged, saved as yours");
        assert_eq!(builtin.def.scope, AgentScope::User);
        assert_eq!((builtin.previous_name(), builtin.moved_from()), (None, None));
    }

    #[test]
    fn agents_labels_are_short() {
        assert_eq!(model_label(None), "Session model");
        assert_eq!(model_label(Some("anthropic/claude-z")), "claude-z");
        assert_eq!(effort_label(Some("xhigh")), "Xhigh");
        assert_eq!(effort_label(None), "Default");
        assert_eq!(short_error("write /Users/me/.config/otto/agents/x.md: permission denied"), "write permission denied");
        for (_, glyph) in
            [("builtin", source_glyph("builtin")), ("mcp", source_glyph("mcp:x")), ("ext", source_glyph("ext:y")), ("silo", source_glyph("silo"))]
        {
            assert!(crate::assets::has_icon(glyph));
        }
    }
}
