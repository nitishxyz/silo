use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use otto_client::{Connection, SessionCreateOptions, SessionFollower, SessionMirror};
use otto_llm::Role;
use otto_proto::{ApprovalDecision, Delivery, InputPart, Method, Part, SessionInfo, TurnStatus};
use std::{
    collections::{HashMap, HashSet},
    io::{self, IsTerminal, Write},
};

#[derive(Args)]
pub struct RunArgs {
    #[arg(long, conflicts_with_all = ["session", "resume"])]
    pub model: Option<String>,
    #[arg(long, conflicts_with_all = ["session", "resume"])]
    pub agent: Option<String>,
    #[arg(long, conflicts_with = "resume")]
    pub session: Option<String>,
    #[arg(long = "continue")]
    pub resume: bool,
    /// Approval handling: auto (default), ask (on effects), ask-all, or plan.
    #[arg(long, default_value = "auto", value_parser = ["auto", "ask", "ask-all", "plan"])]
    pub approve: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub show_reasoning: bool,
    #[arg(allow_hyphen_values = true)]
    pub prompt: Vec<String>,
}
#[derive(Args)]
pub struct AttachArgs {
    pub session: String,
    #[arg(long)]
    pub interactive: bool,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub show_reasoning: bool,
    /// Print the current snapshot and exit instead of watching.
    #[arg(long)]
    pub once: bool,
}
#[derive(Subcommand)]
pub enum SessionCommand {
    /// Set the approval mode: auto (default), ask, ask-all or plan.
    Mode {
        id: String,
        #[arg(value_parser=["auto","ask","ask-all","plan"])]
        mode: String,
    },
    /// Rename a session.
    Rename { id: String, title: String },
    /// Archive a session and remove its spilled tool output.
    Archive { id: String },
    /// Fork a session, optionally at an event seq.
    Fork {
        id: String,
        #[arg(long)]
        at_seq: Option<i64>,
    },
}
fn approval_mode(value: &str) -> otto_proto::ApprovalMode {
    match value {
        "ask" => otto_proto::ApprovalMode::AskOnEffects,
        "ask-all" => otto_proto::ApprovalMode::AskAll,
        "plan" => otto_proto::ApprovalMode::Plan,
        _ => otto_proto::ApprovalMode::Auto,
    }
}
async fn project(connection: &Connection) -> Result<otto_proto::ProjectInfo> {
    let output = tokio::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).output().await;
    let root = match output {
        Ok(output) if output.status.success() => String::from_utf8(output.stdout)?.trim().to_owned(),
        _ => std::env::current_dir()?.to_string_lossy().into_owned(),
    };
    Ok(connection.call_typed(Method::ProjectOpen { root }).await?)
}
pub async fn list(all: bool) -> Result<()> {
    let c = crate::daemon::connect().await?;
    let project_id = if all { None } else { Some(project(&c).await?.id) };
    println!("ID\tSTATUS\tMODE\tAGENT\tMODEL\tTITLE");
    for s in c.session_list(project_id, all).await? {
        println!("{}\t{:?}\t{:?}\t{}\t{}/{}\t{}", s.id, s.status, s.approval_mode, s.agent, s.provider, s.model, s.title);
    }
    Ok(())
}
pub async fn mutate(command: SessionCommand) -> Result<()> {
    let method = match command {
        SessionCommand::Mode { id, mode } => Method::SessionSetMode { session_id: id, approval_mode: approval_mode(&mode) },
        SessionCommand::Rename { id, title } => Method::SessionRename { session_id: id, title },
        SessionCommand::Archive { id } => Method::SessionArchive { session_id: id },
        SessionCommand::Fork { id, at_seq } => Method::SessionFork { session_id: id, at_seq },
    };
    println!("{}", crate::daemon::connect().await?.call(method).await?);
    Ok(())
}
pub async fn cancel(id: &str) -> Result<()> {
    crate::daemon::connect().await?.cancel(id).await?;
    Ok(())
}

pub async fn run(args: RunArgs) -> Result<()> {
    let c = crate::daemon::connect().await?;
    let mut text = args.prompt.join(" ");
    if text == "-" {
        text = crate::stdin_text().await?;
    } else if !io::stdin().is_terminal() {
        let input = crate::stdin_text().await?;
        if !input.is_empty() {
            text.push('\n');
            text.push_str(&input);
        }
    }
    if text.trim().is_empty() {
        bail!("provide a prompt or pipe text on stdin");
    }
    let project = project(&c).await?;
    let mode = approval_mode(&args.approve);
    let existing = args.session.is_some() || args.resume;
    let mut session = if let Some(id) = args.session {
        c.session_view(&id).await?.info
    } else if args.resume {
        c.session_list(Some(project.id.clone()), false)
            .await?
            .into_iter()
            .max_by_key(|s| s.updated_at)
            .ok_or_else(|| anyhow::anyhow!("no session to continue"))?
    } else {
        let (provider, model) = args
            .model
            .as_deref()
            .map(|s| s.split_once('/').ok_or_else(|| anyhow::anyhow!("--model must be provider/model")))
            .transpose()?
            .map_or((None, None), |(p, m)| (Some(p.into()), Some(m.into())));
        let agent = if args.approve == "plan" { Some("plan".into()) } else { args.agent };
        c.session_create(SessionCreateOptions {
            project_id: Some(project.id),
            provider,
            model,
            agent,
            approval_mode: Some(mode),
            title: Some(text.chars().take(80).collect()),
            ..Default::default()
        })
        .await?
    };
    if existing {
        c.call(Method::SessionSetMode { session_id: session.id.clone(), approval_mode: mode }).await?;
        session = c.session_view(&session.id).await?.info;
    }
    eprintln!("Session: {} (mode: {:?})", session.id, session.approval_mode);
    let follower = SessionFollower::new(c.clone(), &session.id).await?;
    let baseline = follower.current().mirror.last_seq;
    let events = c.events();
    let mut renderer = Renderer::default();
    renderer.seed(&follower.current().mirror);
    c.prompt(&session.id, vec![InputPart::Text { text }], Delivery::Queue).await?;
    let watch = watch(
        &c,
        &session,
        follower,
        events,
        renderer,
        Watch { json: args.json, reasoning: args.show_reasoning, interactive: true, approve: args.approve, stop_after: Some(baseline), once: false },
    );
    tokio::select! {
        result = watch => result,
        _ = tokio::signal::ctrl_c() => {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), c.cancel(&session.id)).await;
            Err(otto_llm::LlmError::new(otto_llm::ErrorClass::Cancelled, "interrupted").into())
        }
    }
}
pub async fn attach(args: AttachArgs) -> Result<()> {
    let c = crate::daemon::connect().await?;
    let events = c.events();
    let follower = SessionFollower::new(c.clone(), &args.session).await?;
    let session = follower.current().mirror.info;
    tokio::select! {
        result = watch(&c, &session, follower, events, Renderer::default(), Watch { json: args.json, reasoning: args.show_reasoning, interactive: args.interactive, approve: "ask".into(), stop_after: None, once: args.once }) => result,
        _ = tokio::signal::ctrl_c() => {
            if args.interactive { let _ = c.cancel(&session.id).await; }
            Err(otto_llm::LlmError::new(otto_llm::ErrorClass::Cancelled, "interrupted").into())
        }
    }
}
struct Watch {
    json: bool,
    reasoning: bool,
    interactive: bool,
    approve: String,
    stop_after: Option<i64>,
    once: bool,
}
async fn watch(
    c: &Connection,
    session: &SessionInfo,
    follower: SessionFollower,
    mut events: otto_client::Events,
    mut renderer: Renderer,
    options: Watch,
) -> Result<()> {
    let mut changes = follower.changes();
    let mut answered = HashSet::new();
    loop {
        let state = changes.borrow_and_update().clone();
        if !options.json {
            renderer.render(&state.mirror, options.reasoning)?;
        }
        if options.once {
            if options.json {
                println!("{}", serde_json::to_string(&c.session_view(&session.id).await?)?);
            }
            return Ok(());
        }
        if options.interactive {
            for approval in &state.mirror.pending_approvals {
                if !answered.insert(approval.approval_id.clone()) {
                    continue;
                }
                let (decision, note) = if options.approve == "auto" {
                    (ApprovalDecision::Allow, None)
                } else if options.approve == "plan" {
                    (ApprovalDecision::Deny, None)
                } else if io::stdin().is_terminal() {
                    approval_reply(&crate::prompt(&format!("{} [y/n/a=always; optional ': note']", approval.summary), false).await?)
                } else {
                    eprintln!("Denied {}: approval requires a TTY; use --approve auto to allow", approval.tool);
                    (ApprovalDecision::Deny, None)
                };
                c.approval_answer(&session.id, &approval.approval_id, decision, note).await?;
            }
            for question in &state.mirror.pending_questions {
                if !answered.insert(question.question_id.clone()) {
                    continue;
                }
                let answer = if io::stdin().is_terminal() {
                    crate::prompt(&format!("{} {}", question.question, question.options.join(" / ")), false).await?
                } else {
                    eprintln!("Cannot answer question without a TTY: {}", question.question);
                    "No interactive user is available.".into()
                };
                c.question_answer(&session.id, &question.question_id, answer).await?;
            }
        }
        if let Some(baseline) = options.stop_after
            && state.mirror.last_seq > baseline
            && let Some((_, status)) = state.mirror.turn
            && status != TurnStatus::Running
        {
            if options.json {
                while let Ok(Ok(message)) = events.try_recv() {
                    print_wire(&message, &session.id)?;
                }
            }
            if !options.json {
                println!();
            }
            return match status {
                TurnStatus::Done => Ok(()),
                TurnStatus::Cancelled | TurnStatus::Interrupted => {
                    Err(otto_llm::LlmError::new(otto_llm::ErrorClass::Cancelled, "turn cancelled").into())
                }
                _ => {
                    let (class, message, _) = state.mirror.last_error.unwrap_or(("other".into(), "turn failed".into(), false));
                    if class == "auth" {
                        Err(otto_llm::LlmError::new(otto_llm::ErrorClass::Auth, message).into())
                    } else {
                        Err(anyhow::anyhow!(message))
                    }
                }
            };
        }
        tokio::select! {
            changed = changes.changed() => { changed?; },
            event = events.recv(), if options.json => { print_wire(&event??, &session.id)?; },
        }
    }
}
fn print_wire(message: &otto_proto::ServerMessage, session: &str) -> Result<()> {
    let value = serde_json::to_value(message)?;
    if value.get("session_id").and_then(|v| v.as_str()) == Some(session) {
        println!("{}", serde_json::to_string(message)?);
    }
    Ok(())
}
fn approval_reply(reply: &str) -> (ApprovalDecision, Option<String>) {
    let (answer, note) = reply.split_once(':').map_or((reply, None), |(answer, note)| (answer, Some(note.trim())));
    let decision = match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Allow,
        "a" => ApprovalDecision::AlwaysAllow,
        _ => ApprovalDecision::Deny,
    };
    (decision, note.filter(|note| !note.is_empty()).map(str::to_owned))
}

#[test]
fn approval_notes() {
    assert_eq!(approval_reply("n: unsafe path"), (ApprovalDecision::Deny, Some("unsafe path".into())));
    assert_eq!(approval_reply(" Y : reviewed: safe "), (ApprovalDecision::Allow, Some("reviewed: safe".into())));
    assert_eq!(approval_reply("a:"), (ApprovalDecision::AlwaysAllow, None));
    assert_eq!(approval_reply(""), (ApprovalDecision::Deny, None));
}
#[derive(Default)]
struct Renderer {
    text: HashMap<String, String>,
    reasoning: HashMap<String, String>,
    tools: HashMap<String, String>,
}
impl Renderer {
    fn seed(&mut self, mirror: &SessionMirror) {
        for item in &mirror.items {
            self.text.insert(item.id.clone(), item.text());
        }
    }
    fn render(&mut self, mirror: &SessionMirror, show_reasoning: bool) -> Result<()> {
        for item in &mirror.items {
            if item.role == Role::Assistant {
                let text = item.text();
                let old = self.text.entry(item.id.clone()).or_default();
                if let Some(delta) = text.strip_prefix(old.as_str()) {
                    print!("{delta}");
                }
                *old = text;
                if show_reasoning {
                    let text: String = item
                        .parts
                        .iter()
                        .filter_map(|p| match p {
                            Part::Reasoning { summary, text, .. } => summary.as_ref().or(text.as_ref()),
                            _ => None,
                        })
                        .cloned()
                        .collect();
                    let old = self.reasoning.entry(item.id.clone()).or_default();
                    if let Some(delta) = text.strip_prefix(old.as_str()) {
                        if io::stderr().is_terminal() {
                            eprint!("\x1b[2m{delta}\x1b[0m");
                        } else {
                            eprint!("{delta}");
                        }
                    }
                    *old = text;
                }
            }
            for part in &item.parts {
                if let Part::ToolResult { call_id, content, is_error } = part {
                    let value = serde_json::to_string(content)?;
                    if self.tools.insert(format!("result:{call_id}"), value.clone()).as_ref() != Some(&value) {
                        eprintln!("tool {call_id} {}: {}", if *is_error { "error" } else { "done" }, value.chars().take(400).collect::<String>());
                    }
                }
            }
        }
        for (id, tool) in &mirror.tools {
            let value = format!("{} {:?}", tool.name, tool.status);
            if self.tools.insert(id.clone(), value.clone()).as_ref() != Some(&value) {
                eprintln!("tool {id}: {value}");
            }
        }
        io::stdout().flush()?;
        Ok(())
    }
}
