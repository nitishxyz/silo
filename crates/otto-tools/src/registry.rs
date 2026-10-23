use crate::{Tool, ToolError, ToolSource, ToolSpec, builtin::*};
use std::{collections::BTreeMap, sync::Arc};
#[derive(Default)]
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    sources: Vec<Arc<dyn ToolSource>>,
}
impl Registry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&mut self, tool: Arc<dyn Tool>) -> Option<Arc<dyn Tool>> {
        self.tools.insert(tool.spec().name.clone(), tool)
    }
    pub fn lookup(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
    pub fn list_specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec().clone()).collect()
    }
    pub async fn add_source(&mut self, source: Arc<dyn ToolSource>) -> Result<(), ToolError> {
        for tool in source.list().await? {
            self.add(tool);
        }
        self.sources.push(source);
        Ok(())
    }
    pub fn sources(&self) -> &[Arc<dyn ToolSource>] {
        &self.sources
    }
}
pub fn tools_for_family(family: &str) -> Vec<Arc<dyn Tool>> {
    tools_with(family, &shell::Host::default(), &web_fetch::Web::default())
}
/// The built-ins for `family`, whose shells share `host` (jobs, terminals)
/// and whose web tools share `web` (renderer, search engine).
pub fn tools_with(family: &str, host: &shell::Host, web: &web_fetch::Web) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(read::Read::default()),
        Arc::new(write::Write::default()),
        Arc::new(copy_attachment::CopyAttachment::default()),
        Arc::new(ls::Ls::default()),
        Arc::new(tree::Tree::default()),
        Arc::new(glob::Glob::default()),
        Arc::new(search::Search::default()),
        Arc::new(shell::Shell::with(host.clone())),
        Arc::new(shell::Job::new(host.jobs.clone())),
        Arc::new(web_fetch::WebFetch::new(web.clone())),
        Arc::new(web_search::WebSearch::new(web.clone())),
        Arc::new(ask_user::AskUser::default()),
        Arc::new(update_todos::UpdateTodos::default()),
    ];
    if family.eq_ignore_ascii_case("openai") {
        tools.push(Arc::new(apply_patch::ApplyPatch::default()));
    } else {
        tools.push(Arc::new(edit::Edit::default()));
        tools.push(Arc::new(multiedit::Multiedit::default()));
    }
    tools
}
