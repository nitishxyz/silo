//! The built-in `studio` agent and its bundled skills (docs/STUDIO.md §6.4): what a studio session is shown and
//! granted, skills listed and loaded, and a lint that the prompt and skills name only tools and skills that exist.
use otto_core::*;
use otto_llm::*;
use otto_proto::{Delivery, Event, InputPart};
use otto_store::Store;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};

struct Scripted {
    info: ModelInfo,
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<Request>>,
}
#[async_trait::async_trait]
impl Model for Scripted {
    fn info(&self) -> &ModelInfo {
        &self.info
    }
    async fn stream(&self, request: Request, _cx: CallCx) -> std::result::Result<EventStream, LlmError> {
        self.requests.lock().unwrap().push(request);
        let script = self.scripts.lock().unwrap().pop_front();
        Ok(match script {
            Some(events) => Box::pin(futures::stream::iter(events.into_iter().map(Ok))),
            None => Box::pin(futures::stream::pending()),
        })
    }
}
struct Resolver(Arc<Scripted>);
impl ModelResolver for Resolver {
    fn resolve(&self, _: &str, _: &str) -> Result<Arc<dyn Model>> {
        Ok(self.0.clone())
    }
    fn default_model(&self) -> Option<(String, String)> {
        Some(("test".into(), "test".into()))
    }
    fn list(&self) -> Vec<ModelInfo> {
        vec![self.0.info.clone()]
    }
}
fn text(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::PartDone { part: Part::text(text) }, StreamEvent::Finish { reason: FinishReason::Stop, response_id: None }]
}
fn call(id: &str, name: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::PartDone { part: Part::ToolCall { call_id: id.into(), name: name.into(), input, kind: CallKind::Function, opaque: None } },
        StreamEvent::Finish { reason: FinishReason::ToolUse, response_id: None },
    ]
}

/// A runtime with silo's and the media tools, as the daemon has them.
async fn runtime(dir: &tempfile::TempDir, scripts: Vec<Vec<StreamEvent>>) -> (Runtime, Arc<Scripted>) {
    let info =
        ModelInfo { id: "test".into(), provider: "test".into(), name: "test".into(), caps: Caps::default(), pricing: None, list_pricing: None };
    let model = Arc::new(Scripted { info, scripts: Mutex::new(scripts.into()), requests: Mutex::new(vec![]) });
    let r = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        models: Arc::new(Resolver(model.clone())),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    r.add_tools(otto_silo_tools::tools(otto_silo_tools::Config { home: dir.path().join("silo"), client_name: "otto test".into(), token: None }));
    let media = otto_media::Media::with(
        otto_llm::auth::CredentialStore::open(dir.path().join("auth")).unwrap(),
        dir.path().join("media"),
        otto_media::providers::all(),
        Arc::new(|_: &str| None),
    );
    r.add_tools(media.tools());
    (r, model)
}

async fn turn(r: &Runtime, agent: &str, dir: &std::path::Path) -> SessionHandle {
    let project = r.open_project(dir).await.unwrap();
    let s = r.create_session(CreateSession { project_id: project.id, cwd: dir.into(), agent: agent.into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    s.prompt(vec![InputPart::Text { text: "hello".into() }], Delivery::Queue).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async { while !matches!(live.recv().await.unwrap().1, Event::TurnFinished { .. }) {} })
        .await
        .unwrap();
    s
}

const SHOWN: &[&str] = &[
    "studio_open",
    "studio_read",
    "canvas_read",
    "canvas_create",
    "canvas_edit",
    "canvas_run",
    "generate_image",
    "generate_video",
    "generate_audio",
    "generate_media",
    "list_media_models",
    "media_model_schema",
    "read",
    "write",
    "edit",
    "ls",
    "glob",
    "search",
    "shell",
    "job",
    "web_fetch",
    "web_search",
    "ask_user",
    "update_todos",
    "skill",
    "subagent",
    "load_tools",
];

#[tokio::test]
async fn a_studio_session_shows_its_essentials_and_loads_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![call("c1", "load_tools", serde_json::json!({"names": ["browser_click", "files_show"]})), text("done")];
    let (r, model) = runtime(&dir, scripts).await;
    assert_eq!(r.agents(Some(dir.path())).iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["build", "studio"]);
    let s = turn(&r, "studio", dir.path()).await;
    let requests = model.requests.lock().unwrap().clone();
    let names = |i: usize| requests[i].tools.iter().map(|t| t.name.clone()).collect::<BTreeSet<_>>();
    let first = names(0);
    for name in SHOWN {
        assert!(first.contains(*name), "{name} not shown up front: {first:?}");
    }
    for name in ["browser_click", "files_show", "terminal_run"] {
        assert!(!first.contains(name), "{name} waits behind load_tools");
    }
    // Still granted: load_tools brings them in.
    let second = names(1);
    assert!(second.contains("browser_click") && second.contains("files_show"), "{second:?}");
    let result = requests[1].items.iter().flat_map(|i| &i.parts).find_map(|p| match p {
        Part::ToolResult { is_error, .. } => Some(*is_error),
        _ => None,
    });
    assert_eq!(result, Some(false));
    // The prompt is the studio's, with every bundled skill listed.
    let system = requests[0].system.join("\n");
    assert!(system.contains("Studio"), "{system}");
    for (name, _) in otto_core::skills::BUNDLED {
        assert!(system.contains(&format!("\n{name}: ")), "{name} not listed");
    }
    drop(s);
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn build_lists_the_bundled_skills_too_and_skill_loads_them() {
    let dir = tempfile::tempdir().unwrap();
    let (r, model) = runtime(&dir, vec![call("c1", "skill", serde_json::json!({"name": "shot-listing"})), text("done")]).await;
    turn(&r, "build", dir.path()).await;
    let requests = model.requests.lock().unwrap().clone();
    let system = requests[0].system.join("\n");
    for (name, _) in otto_core::skills::BUNDLED {
        assert!(system.contains(&format!("\n{name}: ")), "{name} not listed for build");
    }
    let loaded = requests[1].items.iter().flat_map(|i| &i.parts).find_map(|p| match p {
        Part::ToolResult { content, is_error, .. } => Some((serde_json::to_string(content).unwrap(), *is_error)),
        _ => None,
    });
    let (body, error) = loaded.unwrap();
    assert!(!error, "{body}");
    assert!(body.contains("script.md") && !body.contains("description:"), "the body, without frontmatter: {body}");
    let listed: Vec<String> = r.skills(Some(dir.path())).into_iter().map(|s| s.name).collect();
    assert_eq!(listed.len(), 17, "{listed:?}");
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn studio_and_its_skills_can_be_overridden() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".otto/agents")).unwrap();
    std::fs::write(root.join(".otto/agents/studio.md"), "---\ndescription: Ours\n---\nOur studio.").unwrap();
    std::fs::create_dir_all(root.join(".otto/skills/shot-listing")).unwrap();
    std::fs::write(root.join(".otto/skills/shot-listing/SKILL.md"), "---\nname: shot-listing\ndescription: Our shots\n---\nOurs.").unwrap();
    let (r, _) = runtime(&dir, vec![]).await;
    let agents = r.agents(Some(root));
    let studio = agents.iter().find(|a| a.name == "studio").unwrap();
    assert_eq!(studio.description, "Ours");
    let skills = r.skills(Some(root));
    assert_eq!(skills.len(), 17);
    assert_eq!(skills.iter().find(|s| s.name == "shot-listing").unwrap().description, "Our shots");
    let found = otto_core::skills::discover(root, &root.join("user"));
    let ours = found.iter().find(|s| s.name == "shot-listing").unwrap();
    assert!(ours.path().is_some_and(|p| p.ends_with(".otto/skills/shot-listing/SKILL.md")), "{:?}", ours.path());
    r.shutdown().await.unwrap();
}

#[test]
fn bundled_skills_have_valid_frontmatter() {
    assert_eq!(otto_core::skills::BUNDLED.len(), 17);
    let folders: BTreeSet<String> = std::fs::read_dir("skills").unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    let bundled: BTreeSet<String> = otto_core::skills::BUNDLED.iter().map(|(n, _)| (*n).to_owned()).collect();
    assert_eq!(folders, bundled, "every folder bundled");
    for (folder, text) in otto_core::skills::BUNDLED {
        let head = text.strip_prefix("---\n").unwrap_or_else(|| panic!("{folder}: no frontmatter"));
        let (front, body) = head.split_once("\n---\n").unwrap_or_else(|| panic!("{folder}: unterminated frontmatter"));
        let field = |key: &str| front.lines().find_map(|l| l.strip_prefix(&format!("{key}: "))).map(str::trim);
        assert_eq!(field("name"), Some(*folder), "{folder}: name");
        let description = field("description").unwrap_or_default();
        assert!(!description.is_empty(), "{folder}: description");
        // One line: the description may not continue on following lines.
        let lines: Vec<&str> = front.lines().collect();
        let at = lines.iter().position(|l| l.starts_with("description:")).unwrap();
        assert!(lines.get(at + 1).is_none_or(|l| l.contains(": ")), "{folder}: description spans lines: {:?}", lines.get(at + 1));
        assert!(body.trim().len() > 200, "{folder}: a body");
    }
}

/// Every tool otto has with silo and media, by name.
async fn tool_names() -> BTreeSet<String> {
    let dir = tempfile::tempdir().unwrap();
    let mut names: BTreeSet<String> = ["skill", "subagent", "load_tools"].into_iter().map(String::from).collect();
    for family in ["openai", "anthropic", "generic"] {
        for tool in otto_tools::registry::tools_for_family(family) {
            names.insert(tool.spec().name.clone());
            names.extend(tool.spec().presentations.iter().map(|p| p.name.clone()));
        }
    }
    let silo = otto_silo_tools::tools(otto_silo_tools::Config { home: dir.path().join("silo"), client_name: "t".into(), token: None });
    names.extend(silo.iter().map(|t| t.spec().name.clone()));
    let media = otto_media::Media::with(
        otto_llm::auth::CredentialStore::open(dir.path().join("a")).unwrap(),
        dir.path().join("m"),
        vec![],
        Arc::new(|_: &str| None),
    );
    names.extend(media.tools().iter().map(|t| t.spec().name.clone()));
    names
}

/// The names in `text` that look like Studio, canvas or media tools, and the skills it points to.
fn references(text: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut tools = BTreeSet::new();
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    for token in text.split(|c: char| !word(c)) {
        let token = token.trim_matches('-');
        if ["studio_", "canvas_", "generate_", "media_", "list_media"].iter().any(|p| token.starts_with(p))
            && token.len() > 7
            && !token.ends_with('_')
        {
            tools.insert(token.to_owned());
        }
    }
    let mut skills = BTreeSet::new();
    // `x` skill, skill `x`, skill {name: "x"}, and the backticked names of a "## Skills" section.
    let ticked: Vec<(usize, &str)> = text
        .match_indices('`')
        .collect::<Vec<_>>()
        .chunks(2)
        .filter_map(|p| {
            let [(a, _), (b, _)] = p else { return None };
            Some((*a, &text[a + 1..*b]))
        })
        .collect();
    let section = text.find("## Skills").map(|at| (at, text[at + 1..].find("\n## ").map_or(text.len(), |e| at + 1 + e)));
    for (at, name) in ticked {
        let kebab = !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-') && name.contains('-')
            || name.chars().all(|c| c.is_ascii_lowercase()) && !name.is_empty();
        if !kebab {
            continue;
        }
        let after = &text[at + name.len() + 2..];
        let before = &text[..at];
        let near = after.trim_start().starts_with("skill") || before.trim_end().ends_with("skill") || before.trim_end().ends_with("Load");
        // A section entry: `name` (what it is for).
        let in_section = section.is_some_and(|(s, e)| at > s && at < e) && after.starts_with(" (");
        if near || in_section {
            skills.insert(name.to_owned());
        }
    }
    for (at, _) in text.match_indices("skill {name: \"") {
        let rest = &text[at + 14..];
        if let Some(end) = rest.find('"') {
            skills.insert(rest[..end].to_owned());
        }
    }
    (tools, skills)
}

#[tokio::test]
async fn the_prompt_and_skills_name_only_tools_and_skills_that_exist() {
    let tools = tool_names().await;
    let skills: BTreeSet<&str> = otto_core::skills::BUNDLED.iter().map(|(n, _)| *n).collect();
    let mut files = vec![("agents/studio.md".to_owned(), std::fs::read_to_string("agents/studio.md").unwrap())];
    for (name, text) in otto_core::skills::BUNDLED {
        files.push((format!("skills/{name}/SKILL.md"), (*text).to_owned()));
    }
    let mut problems = Vec::new();
    let mut mentioned = BTreeSet::new();
    for (file, text) in &files {
        let (t, s) = references(text);
        for name in t.iter().filter(|n| !tools.contains(*n)) {
            problems.push(format!("{file}: no tool {name}"));
        }
        for name in s.iter().filter(|n| !skills.contains(n.as_str())) {
            problems.push(format!("{file}: no skill {name}"));
        }
        mentioned.extend(s);
    }
    assert!(problems.is_empty(), "{problems:#?}");
    // The agent's prompt points to every bundled skill.
    let (_, from_prompt) = references(&files[0].1);
    let missing: Vec<&&str> = skills.iter().filter(|s| !from_prompt.contains(**s)).collect();
    assert!(missing.is_empty(), "agents/studio.md never names {missing:?}");
    assert!(references("use `canvas_nope` then the `nope-skill` skill").0.contains("canvas_nope"), "the lint catches a bad tool");
    assert!(references("use `canvas_nope` then the `nope-skill` skill").1.contains("nope-skill"), "the lint catches a bad skill");
}
