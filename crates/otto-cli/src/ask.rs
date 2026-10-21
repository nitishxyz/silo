use std::{
    io::{self, IsTerminal, Write},
    sync::Arc,
};

use anyhow::{Result, bail};
use futures::StreamExt;
use otto_llm::{
    CallCx, CallKind, ErrorClass, Item, LlmError, Media, MediaSource, Origin, Part, Reasoning, Request, ResultPart, Role, StreamEvent, ToolDef,
    Usage, providers::Providers,
};
use otto_tools::{OutputPart, Tool, ToolCx, ToolOutput, builtin};
use tokio_util::sync::CancellationToken;

use crate::{AskArgs, stdin_text};

pub async fn run(args: AskArgs, providers: &Providers, cancel: CancellationToken) -> Result<()> {
    let (provider, model_id) = if let Some(id) = args.model {
        let (provider, model) =
            id.split_once('/').filter(|(p, m)| !p.is_empty() && !m.is_empty()).ok_or_else(|| anyhow::anyhow!("--model must be provider/model"))?;
        (provider.to_owned(), model.to_owned())
    } else {
        providers.default_model().ok_or_else(|| LlmError::new(ErrorClass::Auth, "run `otto-silo auth login <provider>`"))?
    };
    let model = providers.model(&provider, &model_id).await?;
    let mut prompt = args.prompt.join(" ");
    if prompt == "-" {
        prompt = stdin_text().await?;
    } else if !io::stdin().is_terminal() {
        let input = stdin_text().await?;
        if !input.is_empty() {
            if !prompt.is_empty() {
                prompt.push('\n');
            }
            prompt.push_str(&input);
        }
    }
    if prompt.trim().is_empty() {
        bail!("provide a prompt or pipe text on stdin");
    }
    let tools = if args.tools { read_tools() } else { vec![] };
    let request = Request {
        model: model_id.clone(),
        system: args.system.into_iter().collect(),
        items: vec![Item::user_text("user-0", prompt)],
        reasoning: args.effort.map(|effort| serde_json::from_value(serde_json::Value::String(effort)).map(Reasoning::Effort)).transpose()?,
        tools: tools
            .iter()
            .map(|tool| {
                let spec = tool.spec();
                ToolDef {
                    name: spec.name.clone(),
                    description: spec.description.clone(),
                    input_schema: spec.input_schema.clone(),
                    kind: Default::default(),
                    defer_loading: false,
                }
            })
            .collect(),
        ..Request::default()
    };
    converse(model.as_ref(), request, tools, args.json, args.show_reasoning, cancel).await
}

async fn converse(
    model: &dyn otto_llm::Model,
    mut request: Request,
    tools: Vec<Arc<dyn Tool>>,
    json: bool,
    show_reasoning: bool,
    cancel: CancellationToken,
) -> Result<()> {
    let mut total = Usage::default();
    for step in 0..8 {
        let mut stream = model.stream(request.clone(), CallCx { cancel: cancel.clone(), session: None }).await?;
        let mut assistant = Item::new(format!("assistant-{step}"), Role::Assistant, vec![]);
        assistant.origin = Some(Origin { provider: model.info().provider.clone(), model: request.model.clone(), response_id: None });
        let mut usage = Usage::default();
        while let Some(event) = stream.next().await {
            let event = event?;
            if json {
                print!("{}", json_line(&event)?);
                io::stdout().flush()?;
            }
            match event {
                StreamEvent::TextDelta { text } if !json => {
                    print!("{text}");
                    io::stdout().flush()?;
                }
                StreamEvent::ReasoningDelta { text, .. } if show_reasoning => {
                    if io::stderr().is_terminal() {
                        eprint!("\x1b[2m{text}\x1b[0m");
                    } else {
                        eprint!("{text}");
                    }
                }
                StreamEvent::PartDone { part } => assistant.parts.push(part),
                StreamEvent::Usage(next) => usage = next,
                StreamEvent::Start { response_id, .. } | StreamEvent::Finish { response_id, .. } if response_id.is_some() => {
                    assistant.origin.as_mut().expect("origin").response_id = response_id;
                }
                _ => {}
            }
        }
        total.input += usage.input;
        total.output += usage.output;
        total.cache_read += usage.cache_read;
        total.cache_write += usage.cache_write;
        total.reasoning += usage.reasoning;
        let calls = assistant
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::ToolCall { call_id, name, input, kind: CallKind::Function, .. } => Some((call_id.clone(), name.clone(), input.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        request.items.push(assistant);
        if calls.is_empty() {
            if !json {
                println!();
            }
            eprint!("Usage: {} input ({} cached), {} output", total.total_input(), total.cache_read, total.output);
            if let Some(pricing) = &model.info().pricing {
                let cost = (total.input as f64 * pricing.input
                    + total.output as f64 * pricing.output
                    + total.cache_read as f64 * pricing.cache_read
                    + total.cache_write as f64 * pricing.cache_write)
                    / 1_000_000.0;
                eprint!("; estimated token cost ${cost:.6}");
            }
            eprintln!();
            return Ok(());
        }
        if tools.is_empty() {
            bail!("model requested tools; enable --tools");
        }
        for (call_id, name, input) in calls {
            eprintln!("tool {name} ({call_id})");
            let output = if let Some(tool) = tools.iter().find(|tool| tool.spec().name == name) {
                let mut cx = ToolCx::new(&call_id, std::env::current_dir()?);
                cx.cancel = cancel.clone();
                match tool.call(input, cx).await {
                    Ok(output) => output,
                    Err(error) => ToolOutput::error(error.to_string()),
                }
            } else {
                ToolOutput::error(format!("unknown or disallowed tool: {name}"))
            };
            request.items.push(Item::new(
                format!("tool-{step}-{call_id}"),
                Role::Tool,
                vec![Part::ToolResult { call_id, is_error: output.is_error, content: result_parts(output.content) }],
            ));
        }
    }
    bail!("tool conversation exceeded 8 model calls")
}

fn read_tools() -> Vec<Arc<dyn Tool>> {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(builtin::read::Read::default()),
        Arc::new(builtin::ls::Ls::default()),
        Arc::new(builtin::glob::Glob::default()),
        Arc::new(builtin::search::Search::default()),
    ];
    tools.into_iter().filter(|tool| tool.spec().effects.read_only()).collect()
}

fn result_parts(parts: Vec<OutputPart>) -> Vec<ResultPart> {
    parts
        .into_iter()
        .map(|part| match part {
            OutputPart::Text { text } => ResultPart::text(text),
            OutputPart::Json { value } => ResultPart::text(value.to_string()),
            OutputPart::Image { mime, data_base64 } => {
                ResultPart::Image(Media { mime, source: MediaSource::Base64 { data: data_base64 }, name: None })
            }
            OutputPart::File { path } => ResultPart::text(path.display().to_string()),
        })
        .collect()
}

fn json_line(event: &StreamEvent) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string(event)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct ToolModel {
        info: otto_llm::ModelInfo,
        path: String,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl otto_llm::Model for ToolModel {
        fn info(&self) -> &otto_llm::ModelInfo {
            &self.info
        }
        async fn stream(&self, req: Request, _cx: CallCx) -> std::result::Result<otto_llm::EventStream, LlmError> {
            use std::sync::atomic::Ordering;
            let part = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Part::ToolCall {
                    call_id: "read-1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": self.path}),
                    kind: CallKind::Function,
                    opaque: None,
                }
            } else {
                assert_eq!(req.items[1].role, Role::Assistant);
                assert_eq!(req.items[1].origin.as_ref().unwrap().provider, self.info.provider);
                assert!(matches!(&req.items[2].parts[0], Part::ToolResult { call_id, is_error: false, content }
                    if call_id == "read-1" && matches!(&content[0], ResultPart::Text { text } if text.contains("round-trip fixture"))));
                Part::text("done")
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(StreamEvent::PartDone { part })])))
        }
    }
    #[tokio::test]
    async fn tool_round_trip_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.txt");
        std::fs::write(&path, "round-trip fixture").unwrap();
        let model = ToolModel {
            info: otto_llm::openai::model_info("gpt-5.3-codex").unwrap(),
            path: path.display().to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let request = Request { model: model.info.id.clone(), items: vec![Item::user_text("user", "read")], ..Request::default() };
        converse(&model, request, read_tools(), false, false, CancellationToken::new()).await.unwrap();
        assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
    #[test]
    fn canonical_jsonl() {
        let event = StreamEvent::TextDelta { text: "hello\nworld".into() };
        let line = json_line(&event).unwrap();
        assert_eq!(line.lines().count(), 1);
        assert!(line.ends_with('\n'));
        assert_eq!(serde_json::from_str::<StreamEvent>(&line).unwrap(), event);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&line).unwrap()["type"], "text_delta");
    }
    #[test]
    fn tools_are_read_only() {
        let tools = read_tools();
        assert_eq!(tools.len(), 4);
        assert!(tools.iter().all(|tool| tool.spec().effects.read_only()));
    }
}
