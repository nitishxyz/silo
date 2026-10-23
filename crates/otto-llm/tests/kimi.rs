#![cfg(feature = "client")]
//! Kimi Code: request shaping, SSE, the model list, plan usage, headers.
use otto_llm::{
    compat::{
        self,
        kimi::{self, RemoteModel},
        stream,
    },
    *,
};
use serde_json::{Value, json};

const TOOLS: &str = include_str!("fixtures/compat/kimi_code_tools.sse");

fn remote(id: &str, reasoning: bool, efforts: &[Effort]) -> RemoteModel {
    RemoteModel { id: id.into(), name: id.into(), context_window: 262_144, reasoning, image_input: true, efforts: efforts.to_vec(), max_output: 0 }
}
fn k3() -> ModelInfo {
    remote("kimi-k3", true, &[Effort::Low, Effort::High, Effort::Max]).info()
}
const EFFORTS: [Effort; 3] = [Effort::Low, Effort::High, Effort::Max];
fn reasoning_part() -> Part {
    Part::Reasoning {
        summary: None,
        text: Some("Need to read the file.".into()),
        opaque: Some(Opaque { provider: "kimi-code".into(), model: "kimi-k3".into(), data: json!({"reasoning_content": "Need to read the file."}) }),
    }
}
fn call() -> Part {
    Part::ToolCall { call_id: "call_k1".into(), name: "read_file".into(), input: json!({"path":"README.md"}), kind: CallKind::Function, opaque: None }
}
fn history() -> Vec<Item> {
    vec![
        Item::user_text("u", "look"),
        Item::new("a", Role::Assistant, vec![reasoning_part(), call()]),
        Item::new("t", Role::Tool, vec![Part::ToolResult { call_id: "call_k1".into(), content: vec![ResultPart::text("# otto")], is_error: false }]),
    ]
}
fn req(reasoning: Option<Reasoning>) -> Request {
    Request {
        model: "kimi-k3".into(),
        system: vec!["sys".into()],
        items: history(),
        tools: vec![ToolDef {
            name: "read_file".into(),
            description: "d".into(),
            input_schema: json!({
                "type":"object","title":"Args",
                "properties":{
                    "path":{"type":"string","format":"path","minLength":1},
                    "mode":{"const":"r"},
                    "opt":{"type":["string","null"]}
                },
                "required":["path"]
            }),
            kind: ToolKind::Function,
            defer_loading: false,
        }],
        max_output: Some(100),
        temperature: Some(0.2),
        reasoning,
        ..Request::default()
    }
}
fn body(req: &Request, model: &ModelInfo, efforts: &[Effort]) -> Value {
    kimi::request(req, model, efforts, Some("sess-1")).unwrap()
}

#[test]
fn golden_request_body() {
    let body = body(&req(Some(Reasoning::Effort(Effort::High))), &k3(), &EFFORTS);
    assert_eq!(
        body,
        json!({
            "model": "kimi-k3",
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [
                {"role":"system","content":"sys"},
                {"role":"user","content":"look"},
                {
                    "role":"assistant",
                    "reasoning_content":"Need to read the file.",
                    "tool_calls":[{"id":"call_k1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}}]
                },
                {"role":"tool","tool_call_id":"call_k1","content":"# otto"}
            ],
            "tools": [{"type":"function","function":{"name":"read_file","description":"d","parameters":{
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "mode":{"enum":["r"]},
                    "opt":{"anyOf":[{"type":"string"},{"type":"null"}]}
                },
                "required":["path"]
            }}}],
            "tool_choice": "auto",
            "max_completion_tokens": 100,
            "temperature": 1,
            "thinking": {"type":"enabled"},
            "reasoning_effort": "high",
            "prompt_cache_key": "sess-1"
        })
    );
}

#[test]
fn caller_temperature_cache_key_and_replay_rules() {
    let mut request = req(None);
    request.cache.key = Some("explicit".into());
    let plain = body(&request, &k3(), &EFFORTS);
    assert_eq!(plain["temperature"], 1);
    assert_eq!(plain["prompt_cache_key"], "explicit", "the request's key wins over the session");
    assert!(plain.get("thinking").is_none() && plain.get("reasoning_effort").is_none(), "no reasoning chosen: the server default");
    request.cache.disabled = true;
    assert!(body(&request, &k3(), &EFFORTS).get("prompt_cache_key").is_none());
    // Another provider's reasoning is not replayed; a tool-call turn still carries an empty one while thinking is on.
    let mut foreign = req(None);
    foreign.items[1] = Item::new(
        "a",
        Role::Assistant,
        vec![
            Part::Reasoning {
                summary: None,
                text: None,
                opaque: Some(Opaque { provider: "deepseek".into(), model: "m".into(), data: json!({"reasoning_content":"x"}) }),
            },
            call(),
        ],
    );
    let assistant = &body(&foreign, &k3(), &EFFORTS)["messages"][2];
    assert_eq!(assistant["reasoning_content"], "");
    assert!(assistant.get("content").is_none());
}

#[test]
fn thinking_and_efforts() {
    let thinking = |model: &ModelInfo, efforts: &[Effort], reasoning| {
        let body = body(&req(Some(reasoning)), model, efforts);
        (body.get("thinking").cloned(), body.get("reasoning_effort").cloned())
    };
    let effort = Reasoning::Effort;
    let enabled = Some(json!({"type":"enabled"}));
    // low / high / max: medium goes up, xhigh to max, minimal to low.
    assert_eq!(thinking(&k3(), &EFFORTS, effort(Effort::Medium)), (enabled.clone(), Some(json!("high"))));
    assert_eq!(thinking(&k3(), &EFFORTS, effort(Effort::Xhigh)), (enabled.clone(), Some(json!("max"))));
    assert_eq!(thinking(&k3(), &EFFORTS, effort(Effort::Minimal)), (enabled.clone(), Some(json!("low"))));
    assert_eq!(thinking(&k3(), &EFFORTS, Reasoning::Budget(8000)), (enabled.clone(), Some(json!("high"))));
    // Off turns thinking off, with no effort.
    assert_eq!(thinking(&k3(), &EFFORTS, effort(Effort::None)), (Some(json!({"type":"disabled"})), None));
    assert_eq!(thinking(&k3(), &EFFORTS, Reasoning::Budget(0)), (Some(json!({"type":"disabled"})), None));
    // k3.1-pro rejects disabled: asking for off keeps thinking on at its lowest effort.
    let pro = remote("kimi-k3.1-pro", true, &EFFORTS).info();
    assert_eq!(thinking(&pro, &EFFORTS, effort(Effort::None)), (enabled.clone(), Some(json!("low"))));
    assert_eq!(thinking(&pro, &EFFORTS, effort(Effort::High)), (enabled.clone(), Some(json!("high"))));
    // A model that lists no efforts gets the switch only; one that cannot reason gets nothing.
    assert_eq!(thinking(&remote("kimi-k2.7-code", true, &[]).info(), &[], effort(Effort::High)), (enabled.clone(), None));
    assert_eq!(thinking(&remote("kimi-x", false, &[]).info(), &[], effort(Effort::High)), (None, None));
}

#[test]
fn sanitized_schema_keeps_property_names() {
    let mut request = req(None);
    request.tools[0].input_schema = json!({
        "type":"object",
        "properties":{"title":{"type":"string","description":"t"},"kind":{"oneOf":[{"const":1},{"enum":["a",null]}]}},
        "$schema":"x","additionalProperties":false
    });
    let parameters = &body(&request, &k3(), &EFFORTS)["tools"][0]["function"]["parameters"];
    assert_eq!(
        parameters,
        &json!({
            "type":"object",
            "properties":{"title":{"type":"string","description":"t"},"kind":{"anyOf":[{"enum":[1]},{"enum":["a"]}]}},
            "additionalProperties":false
        })
    );
}

#[test]
fn models_fixture() {
    let list = parse_models_fixture();
    assert_eq!(
        list,
        [
            RemoteModel {
                id: "kimi-k3".into(),
                name: "Kimi K3".into(),
                context_window: 1_048_576,
                reasoning: true,
                image_input: true,
                efforts: vec![Effort::Low, Effort::High, Effort::Max],
                max_output: 0,
            },
            RemoteModel { id: "kimi-for-coding".into(), name: "kimi-for-coding".into(), context_window: 262_144, ..RemoteModel::default() },
        ]
    );
    let info = list[0].info();
    assert_eq!((info.provider.as_str(), info.name.as_str()), ("kimi-code", "Kimi K3"));
    assert!(info.caps.reasoning && info.caps.vision && info.caps.reasoning_replay && info.caps.context_window == 1_048_576);
    assert!(kimi::parse_models(&json!({"error":"x"})).is_empty());
}
fn parse_models_fixture() -> Vec<RemoteModel> {
    kimi::parse_models(&json!({"object":"list","data":[
        {"id":"kimi-k3","display_name":"Kimi K3","context_length":1_048_576,"supports_reasoning":true,"supports_image_in":true,"supports_video_in":false,"think_efforts":["low","high","max","off"]},
        {"id":"kimi-for-coding","context_length":262_144,"supports_reasoning":false,"supports_image_in":false},
        {"display_name":"no id"}
    ]}))
}

#[tokio::test]
async fn sse_reasoning_tool_calls_usage_and_replay() {
    for seed in 0..10 {
        let events = testing::collect_events(stream::stream(testing::chunked(TOOLS.as_bytes(), seed), "kimi-k3", kimi::ID)).await;
        assert!(events.contains(&StreamEvent::ToolInputStart { call_id: "call_k1".into(), name: "read_file".into() }));
        let parts: Vec<Part> = events.iter().filter_map(|e| if let StreamEvent::PartDone { part } = e { Some(part.clone()) } else { None }).collect();
        let Part::Reasoning { text: Some(text), opaque: Some(opaque), .. } = &parts[0] else { panic!("reasoning: {parts:?}") };
        assert_eq!((text.as_str(), opaque.provider.as_str()), ("Need to read the file.", "kimi-code"));
        assert!(matches!(&parts[1], Part::ToolCall { call_id, input, .. } if call_id == "call_k1" && *input == json!({"path":"README.md"})));
        let usage = events.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap();
        assert_eq!(usage, Usage { input: 44, output: 20, cache_read: 256, cache_write: 0, reasoning: 0 });
        assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::ToolUse, .. })));
        // What the stream produced goes back as `reasoning_content` on the next turn.
        let mut next = req(Some(Reasoning::Effort(Effort::High)));
        next.items[1] = Item::new("a", Role::Assistant, parts);
        assert_eq!(body(&next, &k3(), &EFFORTS)["messages"][2]["reasoning_content"], "Need to read the file.");
    }
}

#[test]
fn usage_with_a_five_hour_window_and_a_monthly_allowance() {
    const NOW: i64 = 1_800_000_000;
    let reset = |s: i64| chrono_free_rfc3339(NOW + s);
    let body = json!({
        "user": {"membership": {"level": "LEVEL_INTERMEDIATE"}},
        "usage": {"limit": "1000", "remaining": "250", "resetTime": reset(20 * 86_400)},
        "limits": [{
            "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
            "detail": {"limit": "100", "used": "40", "resetTime": reset(3_600)}
        }]
    });
    let usage = kimi::parse_usage(&body, NOW);
    assert_eq!(
        usage,
        PlanUsage {
            windows: vec![
                PlanWindow { name: "5h".into(), label: "5-hour".into(), used: 0.4, resets_at: Some(NOW + 3_600) },
                PlanWindow { name: "30d".into(), label: "Monthly".into(), used: 0.75, resets_at: Some(NOW + 20 * 86_400) },
            ]
        }
    );
    assert_eq!(usage.tightest().map(|w| w.name.as_str()), Some("30d"));
    // Legacy plans: the overall allowance resets within the week; a limit's window in hours or days names itself.
    let legacy = json!({
        "usage": {"limit": 100, "used": 100, "resetTime": reset(2 * 86_400)},
        "limits": [{"window": {"duration": 5, "timeUnit": "TIME_UNIT_HOUR"}, "detail": {"limit": 10, "remaining": 10}}]
    });
    let usage = kimi::parse_usage(&legacy, NOW);
    assert_eq!(
        usage.windows,
        [
            PlanWindow { name: "5h".into(), label: "5-hour".into(), used: 0.0, resets_at: None },
            PlanWindow { name: "7d".into(), label: "Weekly".into(), used: 1.0, resets_at: Some(NOW + 2 * 86_400) },
        ]
    );
    assert!(kimi::parse_usage(&json!({"usage":{"limit":0}}), NOW).is_empty());
}

/// RFC 3339 UTC with nanoseconds, as Kimi writes reset times.
fn chrono_free_rfc3339(unix: i64) -> String {
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.123456789Z", secs / 3600, secs % 3600 / 60, secs % 60)
}

#[test]
fn headers_are_stable_per_install() {
    let dir = std::env::temp_dir().join(format!("otto-kimi-{}", rand::random::<u64>()));
    let headers = kimi::kimi_headers(&dir).unwrap();
    let get = |name: &str| headers[name].to_str().unwrap().to_owned();
    assert_eq!(get("x-msh-platform"), "kimi_cli");
    assert_eq!(get("x-msh-version"), kimi::VERSION);
    assert_eq!(get("user-agent"), format!("KimiCLI/{}", kimi::VERSION));
    for name in ["x-msh-device-name", "x-msh-device-model", "x-msh-os-version"] {
        assert!(get(name).is_ascii() && !get(name).is_empty(), "{name}");
    }
    let id = get("x-msh-device-id");
    assert_eq!(id.len(), 32);
    assert_eq!(kimi::kimi_headers(&dir).unwrap()["x-msh-device-id"], id.as_str(), "the same id on every call");
    assert_eq!(std::fs::read_to_string(dir.join("kimi-device-id")).unwrap(), id);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_dialect_is_in_the_table() {
    let dialect = compat::dialect("kimi-code").unwrap();
    assert_eq!((dialect.base(None).as_deref(), dialect.key_required), (Some("https://api.kimi.ai/coding/v1"), true));
}
