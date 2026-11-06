//! Media generation end to end through a real daemon: the model's `generate_image` call finds no provider and asks,
//! the client saves a key (`media.set_key`) and answers, the call reaches the provider (a fake OpenAI), the image is
//! stored with the session, saved into the project, and read back over `attachment.get`.
use otto_client::Connection;
use otto_llm::{CallKind, FinishReason, Part, StreamEvent};
use otto_proto::{ApprovalMode, Delivery, InputPart, Method, SessionStatus};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A 1×1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A route's content type and body for a request's head and body.
type Reply = fn(&str) -> (&'static str, Vec<u8>);

/// OpenAI's images: [`PNG`], or [`SVG`] for `output_format: svg`.
fn openai(request: &str) -> (&'static str, Vec<u8>) {
    let image = if request.contains("\"output_format\":\"svg\"") { base64(SVG.as_bytes()) } else { PNG.into() };
    ("application/json", serde_json::json!({"created": 1, "data": [{"b64_json": image}]}).to_string().into_bytes())
}

/// ElevenLabs speech: two seconds of silent MP3 (77 frames of 128 kbps at 44.1 kHz).
fn elevenlabs(_: &str) -> (&'static str, Vec<u8>) {
    let mut frame = vec![0xFF, 0xFB, 0x90, 0x64];
    frame.resize(417, 0);
    ("audio/mpeg", frame.repeat(77))
}

fn base64(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    bytes
        .chunks(3)
        .flat_map(|c| {
            let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
            (0..4).map(move |i| if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' })
        })
        .collect()
}

const SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32"><rect width="64" height="32" fill="red"/></svg>"#;

/// A fake provider: each request's head and body, answered by `reply`.
async fn fake(reply: Reply) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if body.len() >= length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                let (mime, body) = reply(&request);
                log.lock().unwrap().push(request);
                let head = format!("HTTP/1.1 200 OK\r\ncontent-type: {mime}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });
    (base, seen)
}

/// One tool call per step, then a reply.
fn script(path: &Path, calls: &[(&str, serde_json::Value)]) {
    let mut steps: Vec<Vec<StreamEvent>> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, input))| {
            let call = Part::ToolCall {
                call_id: format!("call-{}", i + 1),
                name: (*name).into(),
                input: input.clone(),
                kind: CallKind::Function,
                opaque: None,
            };
            vec![StreamEvent::PartDone { part: call }, StreamEvent::Finish { reason: FinishReason::ToolUse, response_id: None }]
        })
        .collect();
    steps
        .push(vec![StreamEvent::PartDone { part: Part::text("Here it is.") }, StreamEvent::Finish { reason: FinishReason::Stop, response_id: None }]);
    std::fs::write(path, serde_json::to_vec(&serde_json::json!({"steps": steps})).unwrap()).unwrap();
}

async fn start(home: &Path, script: &Path, bases: &[(&str, &str)]) -> Server {
    let mut c = Command::new(env!("CARGO_BIN_EXE_otto-silo"));
    c.env("OTTO_SILO_HOME", home)
        .env("OTTO_SILO_CONFIG_DIR", home.join("config"))
        .env("OTTO_SILO_OFFLINE", "1")
        .env("OTTO_SILO_TEST_SCRIPT", script)
        .env("OTTO_SILO_TEST_REQUESTS", script.with_extension("requests.jsonl"))
        .envs(bases.iter().map(|(id, base)| (format!("OTTO_MEDIA_{}_BASE_URL", id.to_ascii_uppercase()), *base)))
        .env_remove("OPENAI_API_KEY")
        .env_remove("ELEVENLABS_API_KEY")
        .env_remove("XAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GOOGLE_API_KEY")
        .env_remove("FAL_KEY")
        .env_remove("HF_CREDENTIALS")
        .env_remove("HF_API_KEY_ID")
        .env_remove("OTTO_SILO_MODEL")
        .args(["server", "--foreground"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut server = Server(c.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(connection) = Connection::connect(home).await {
            tokio::time::timeout(Duration::from_secs(10), connection.ready(|_| {})).await.unwrap().unwrap();
            return server;
        }
        assert!(server.0.try_wait().unwrap().is_none(), "server exited during startup");
        assert!(Instant::now() < deadline, "server startup timeout");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_generation_asks_for_a_key_then_stores_saves_and_serves_the_image() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let script_path = dir.path().join("script.json");
    script(&script_path, &[("generate_image", serde_json::json!({"prompt": "A red fox in the snow", "provider": "openai", "path": "art/fox.png"}))]);
    let (base, seen) = fake(openai).await;
    let _server = start(&home, &script_path, &[("openai", &base)]).await;
    let c = Connection::connect(&home).await.unwrap();
    let session = c
        .session_create(otto_client::SessionCreateOptions {
            project_root: Some(project.display().to_string()),
            approval_mode: Some(ApprovalMode::Auto),
            ..Default::default()
        })
        .await
        .unwrap();
    let prompt = Method::SessionPrompt {
        session_id: session.id.clone(),
        input: vec![InputPart::Text { text: "draw a fox".into() }],
        delivery: Delivery::Queue,
    };
    c.call(prompt).await.unwrap();

    // No key anywhere: the call asks which provider to connect.
    let deadline = Instant::now() + Duration::from_secs(10);
    let question = loop {
        let view = c.session_view(&session.id).await.unwrap();
        if let Some(q) = view.pending_questions.first() {
            break q.clone();
        }
        assert!(Instant::now() < deadline, "no setup question");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(question.call_id, "call-1");
    assert_eq!(question.question, "Image generation needs a provider. Connect one to continue.");
    assert_eq!(question.options, ["OpenAI", "Gemini", "xAI", "Higgsfield", "fal", "Not now"]);
    assert!(seen.lock().unwrap().is_empty());

    // The setup card's path: save the key, then answer with the provider.
    let info = c.media_set_key("openai", Some("sk-test-media".into())).await.unwrap();
    assert!(info.connected);
    assert_eq!(info.source, Some(otto_proto::MediaKeySource::Media));
    c.question_answer(&session.id, &question.question_id, "OpenAI".into()).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let view = loop {
        let view = c.session_view(&session.id).await.unwrap();
        if view.info.status == SessionStatus::Idle && view.items.iter().any(|i| i.parts.iter().any(|p| matches!(p, Part::ToolResult { .. }))) {
            break view;
        }
        assert!(Instant::now() < deadline, "the turn never finished");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    // The provider got the prompt with the media key.
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /v1/images/generations "), "{}", requests[0]);
    assert!(requests[0].to_ascii_lowercase().contains("authorization: bearer sk-test-media"));
    assert!(requests[0].contains("A red fox in the snow"));

    // The result names the stored image in the line the app parses, never the key.
    let result = view
        .items
        .iter()
        .flat_map(|i| &i.parts)
        .find_map(|p| match p {
            Part::ToolResult { content, is_error, .. } => Some((serde_json::to_string(content).unwrap(), *is_error)),
            _ => None,
        })
        .unwrap();
    assert!(!result.1, "{}", result.0);
    assert!(!result.0.contains("sk-test-media"));
    let line = result.0.split("[generated ").nth(1).expect("a [generated …] line");
    let id = line.split(':').next().unwrap();
    assert!(line.contains("a-red-fox-in-the-snow.png, image/png, ") && line.contains(", 1×1 — "), "{line}");
    assert!(view.items.iter().flat_map(|i| &i.parts).any(|p| matches!(p, Part::Text { text, .. } if text.contains("Here it is."))));

    // Saved into the project, and served back byte for byte.
    let png = std::fs::read(project.join("art/fox.png")).unwrap();
    let data = c.attachment_get(&session.id, id, 0, None).await.unwrap();
    assert_eq!((data.mime.as_str(), data.size, data.offset), ("image/png", png.len() as u64, 0));
    assert_eq!(data.data_base64, PNG);
    let tail = c.attachment_get(&session.id, id, 10, Some(5)).await.unwrap();
    assert_eq!((tail.offset, tail.size), (10, png.len() as u64));
    assert!(c.attachment_get(&session.id, "att_00000000", 0, None).await.is_err());

    // The key shows as connected and is never in the status.
    let status = c.call(Method::MediaStatus {}).await.unwrap();
    assert!(!status.to_string().contains("sk-test-media"));
    assert_eq!(status["providers"][0]["connected"], true);
}

/// Each tool result's text and error flag by call id.
fn results(view: &otto_proto::SessionView) -> std::collections::HashMap<String, (String, bool)> {
    view.items
        .iter()
        .flat_map(|i| &i.parts)
        .filter_map(|p| match p {
            Part::ToolResult { call_id, content, is_error } => Some((call_id.clone(), (serde_json::to_string(content).unwrap(), *is_error))),
            _ => None,
        })
        .collect()
}

/// `[generated <id>: …` in `text`: its id and the line up to ` — `.
fn generated(text: &str) -> (String, String) {
    let line = text.split("[generated ").nth(1).unwrap_or_else(|| panic!("no [generated …] line: {text}"));
    let line = line.split(" — ").next().unwrap();
    (line.split(':').next().unwrap().to_owned(), line.to_owned())
}

/// GPT Image 2.6 SVG, ElevenLabs speech and a video with no provider, in one turn through the daemon.
#[tokio::test(flavor = "multi_thread")]
async fn svg_speech_and_an_unconnected_video_in_one_turn() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let script_path = dir.path().join("script.json");
    script(
        &script_path,
        &[
            (
                "generate_image",
                serde_json::json!({"prompt": "Logo for a bakery", "model": "gpt-image-2.6-flare", "format": "svg", "complexity": "low"}),
            ),
            ("generate_audio", serde_json::json!({"type": "speech", "text": "Welcome to the bakery"})),
            ("generate_video", serde_json::json!({"prompt": "A fox running through snow"})),
        ],
    );
    let (openai_base, openai_seen) = fake(openai).await;
    let (eleven_base, eleven_seen) = fake(elevenlabs).await;
    let _server = start(&home, &script_path, &[("openai", &openai_base), ("elevenlabs", &eleven_base)]).await;
    let c = Connection::connect(&home).await.unwrap();
    c.media_set_key("openai", Some("sk-test-media".into())).await.unwrap();
    c.media_set_key("elevenlabs", Some("el-test-key".into())).await.unwrap();
    let session = c
        .session_create(otto_client::SessionCreateOptions {
            project_root: Some(project.display().to_string()),
            approval_mode: Some(ApprovalMode::Auto),
            ..Default::default()
        })
        .await
        .unwrap();
    let input = vec![InputPart::Text { text: "make the bakery kit".into() }];
    c.call(Method::SessionPrompt { session_id: session.id.clone(), input, delivery: Delivery::Queue }).await.unwrap();

    // The video finds no provider: the setup question offers the video providers only.
    let deadline = Instant::now() + Duration::from_secs(15);
    let question = loop {
        if let Some(q) = c.session_view(&session.id).await.unwrap().pending_questions.first() {
            break q.clone();
        }
        assert!(Instant::now() < deadline, "no setup question");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(question.call_id, "call-3");
    assert_eq!(question.question, "Video generation needs a provider. Connect one to continue.");
    assert_eq!(question.options, ["Gemini", "xAI", "Higgsfield", "fal", "Not now"]);
    c.question_answer(&session.id, &question.question_id, "Not now".into()).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let view = loop {
        let view = c.session_view(&session.id).await.unwrap();
        if view.info.status == SessionStatus::Idle && results(&view).len() == 3 {
            break view;
        }
        assert!(Instant::now() < deadline, "the turn never finished");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let results = results(&view);

    // SVG from GPT Image 2.6 Flare with the 2.6 fields, stored and served as is.
    let request = openai_seen.lock().unwrap()[0].clone();
    for field in ["\"model\":\"gpt-image-2.6-flare\"", "\"output_format\":\"svg\"", "\"complexity\":\"low\""] {
        assert!(request.contains(field), "{field} missing: {request}");
    }
    let (text, error) = &results["call-1"];
    assert!(!error, "{text}");
    let (id, line) = generated(text);
    assert!(line.ends_with(&format!(".svg, image/svg+xml, {} B, 64×32", SVG.len())), "{line}");
    assert_eq!(c.attachment_get(&session.id, &id, 0, None).await.unwrap().data_base64, base64(SVG.as_bytes()));

    // Speech from ElevenLabs with its key and default voice, its length read from the MP3.
    let request = eleven_seen.lock().unwrap()[0].clone();
    assert!(request.starts_with("POST /v1/text-to-speech/JBFqnCBsd6RMkjVDRZzb"), "{request}");
    assert!(request.to_ascii_lowercase().contains("xi-api-key: el-test-key"));
    assert!(request.contains("Welcome to the bakery"));
    let (text, error) = &results["call-2"];
    assert!(!error, "{text}");
    let (id, line) = generated(text);
    assert!(line.contains(".mp3, audio/mpeg, 31 KB, 2 s"), "{line}");
    assert_eq!(c.attachment_get(&session.id, &id, 0, None).await.unwrap().size, 77 * 417);

    // `Not now`: the video fails with where to connect one, and the turn goes on.
    let (text, error) = &results["call-3"];
    assert!(error, "{text}");
    assert!(text.contains("No video provider is connected"), "{text}");
    for (text, _) in results.values() {
        assert!(!text.contains("sk-test-media") && !text.contains("el-test-key"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_mention_is_resolved_and_noted_for_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let script_path = dir.path().join("script.json");
    script(&script_path, &[]);
    let _server = start(&home, &script_path, &[]).await;
    let c = Connection::connect(&home).await.unwrap();
    let session = c
        .session_create(otto_client::SessionCreateOptions { project_root: Some(dir.path().display().to_string()), ..Default::default() })
        .await
        .unwrap();
    let mention = |target: &str| InputPart::Mention { kind: otto_llm::MentionKind::Model, target: target.into() };
    let input = vec![
        InputPart::Text { text: "draw a fox with ".into() },
        mention("gpt-image-2.6-flare"),
        InputPart::Text { text: " ".into() },
        mention("no-such-model"),
    ];
    c.call(Method::SessionPrompt { session_id: session.id.clone(), input, delivery: Delivery::Queue }).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let view = loop {
        let view = c.session_view(&session.id).await.unwrap();
        if view.info.status == SessionStatus::Idle && view.items.len() >= 2 {
            break view;
        }
        assert!(Instant::now() < deadline, "the turn never finished");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let found: Vec<(&str, bool)> = view.items[0].mentions.iter().map(|m| (m.target.as_str(), m.missing)).collect();
    assert_eq!(found, [("gpt-image-2.6-flare", false), ("no-such-model", true)]);
    // What the model saw: the text, then the note naming the tool call to make.
    let log = std::fs::read_to_string(script_path.with_extension("requests.jsonl")).unwrap();
    let items: Vec<otto_llm::Item> = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    let user = items.iter().find(|i| i.role == otto_llm::Role::User).unwrap();
    assert_eq!(
        user.parts.last().unwrap(),
        &Part::text(
            "Mentioned models: GPT Image 2.6 Flare (image) → generate_media {model: \"gpt-image-2.6-flare\"}. Use them for the matching generation; don't ask which model. Call media_model_schema first if the request needs more than a prompt."
        )
    );
}

/// xAI video: submit, one poll (done, pointing back at this server by its `host`), download a tiny MP4.
fn xai(request: &str) -> (&'static str, Vec<u8>) {
    let host = request.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("host:").map(|h| h.trim().to_owned())).unwrap_or_default();
    let json = |v: serde_json::Value| ("application/json", v.to_string().into_bytes());
    if request.starts_with("POST /v1/videos/generations ") {
        json(serde_json::json!({"request_id": "v1"}))
    } else if request.starts_with("GET /v1/videos/v1 ") {
        json(serde_json::json!({"status": "done", "video": {"url": format!("http://{host}/out.mp4")}}))
    } else {
        ("video/mp4", mp4())
    }
}

/// A tiny MP4: `ftyp`, then `moov` with an `mvhd` (8 s) and a 1280×720 `tkhd`.
fn mp4() -> Vec<u8> {
    let boxed = |kind: &[u8; 4], body: &[u8]| [&u32::try_from(body.len() + 8).unwrap().to_be_bytes()[..], kind, body].concat();
    let mut mvhd = vec![0; 12];
    mvhd.extend(1000u32.to_be_bytes());
    mvhd.extend(8000u32.to_be_bytes());
    mvhd.resize(100, 0);
    let mut tkhd = vec![0; 76];
    tkhd.extend((1280u32 << 16).to_be_bytes());
    tkhd.extend((720u32 << 16).to_be_bytes());
    let moov = boxed(b"moov", &[boxed(b"mvhd", &mvhd), boxed(b"trak", &boxed(b"tkhd", &tkhd))].concat());
    [boxed(b"ftyp", b"isom\0\0\0\0isom"), moov].concat()
}

/// A background video (§6.5): the call returns at once and the turn ends; `job.list` shows the media job; its report
/// arrives later as a message in the session, with the `[generated …]` line.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_video_reports_after_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let script_path = dir.path().join("script.json");
    script(&script_path, &[("generate_video", serde_json::json!({"prompt": "A fox running through snow", "provider": "xai", "background": true}))]);
    let (base, seen) = fake(xai).await;
    let _server = start(&home, &script_path, &[("xai", &base)]).await;
    let c = Connection::connect(&home).await.unwrap();
    c.media_set_key("xai", Some("x-test-key".into())).await.unwrap();
    let options = otto_client::SessionCreateOptions {
        project_root: Some(project.display().to_string()),
        approval_mode: Some(ApprovalMode::Auto),
        ..Default::default()
    };
    let session = c.session_create(options).await.unwrap();
    let input = vec![InputPart::Text { text: "make a fox video".into() }];
    c.call(Method::SessionPrompt { session_id: session.id.clone(), input, delivery: Delivery::Queue }).await.unwrap();

    // The turn ends with the call's `Started media job-…`, before xAI's first poll (2 s).
    let deadline = Instant::now() + Duration::from_secs(10);
    let view = loop {
        let view = c.session_view(&session.id).await.unwrap();
        if view.info.status == SessionStatus::Idle && results(&view).contains_key("call-1") {
            break view;
        }
        assert!(Instant::now() < deadline, "the turn never finished");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let (text, error) = &results(&view)["call-1"];
    assert!(!error && text.contains("Started media job-") && !text.contains("[generated "), "{text}");
    let job = text.split("Started media ").nth(1).unwrap().split(' ').next().unwrap().to_owned();
    assert!(!seen.lock().unwrap().iter().any(|r| r.starts_with("GET /v1/videos/v1 ")), "returned before the first poll");
    let jobs = c.jobs(&session.id).await.unwrap();
    let listed = jobs.iter().find(|j| j.job_id == job).expect("job.list shows it");
    assert_eq!(
        (listed.kind, listed.command.as_str(), listed.call_id.as_str()),
        (otto_proto::JobKind::Media, "generate_video: A fox running through snow", "call-1")
    );

    // The report arrives as a message, then the session goes idle again.
    let head = format!("Background media {job} (video) finished.");
    let deadline = Instant::now() + Duration::from_secs(20);
    let report = loop {
        let view = c.session_view(&session.id).await.unwrap();
        let report = view.items.iter().flat_map(|i| &i.parts).find_map(|p| match p {
            Part::System { text } if text.starts_with(&head) => Some(text.clone()),
            _ => None,
        });
        if let Some(report) = report.filter(|_| view.info.status == SessionStatus::Idle) {
            break report;
        }
        assert!(Instant::now() < deadline, "no report");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let (id, line) = generated(&report);
    assert!(line.contains("a-fox-running-through-snow.mp4, video/mp4, ") && line.ends_with("1280×720, 8 s"), "{line}");
    assert_eq!(c.attachment_get(&session.id, &id, 0, None).await.unwrap().size, mp4().len() as u64);
    assert!(!report.contains("x-test-key"));
    assert!(c.jobs(&session.id).await.unwrap().iter().any(|j| j.job_id == job && j.completed));
}

/// Higgsfield: its catalog (the otto-media fixture), an upload slot, a submit, a completed status, the MP4.
fn higgsfield(request: &str) -> (&'static str, Vec<u8>) {
    let host = request.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("host:").map(|h| h.trim().to_owned())).unwrap_or_default();
    let json = |v: serde_json::Value| ("application/json", v.to_string().into_bytes());
    let line = request.lines().next().unwrap_or_default();
    if line.starts_with("GET /models?include_schema=true ") {
        ("application/json", include_bytes!("../../otto-media/tests/fixtures/higgsfield-models.json").to_vec())
    } else if line.starts_with("POST /files/generate-upload-url ") {
        json(serde_json::json!({"upload_url": format!("http://{host}/upload"), "public_url": "https://cdn.test/fox.png"}))
    } else if line.starts_with("PUT /upload ") {
        ("text/plain", Vec::new())
    } else if line.starts_with("GET /requests/r1/status ") {
        json(serde_json::json!({"status": "completed", "video": {"url": format!("http://{host}/out.mp4")}}))
    } else if line.starts_with("POST ") {
        json(serde_json::json!({"status": "queued", "request_id": "r1", "status_url": format!("http://{host}/requests/r1/status")}))
    } else {
        ("video/mp4", mp4())
    }
}

/// Chaining (docs/MEDIA.md §3.2): an image from OpenAI, then its att_ id as the frame of Seedance 2.5's
/// image-to-video on Higgsfield, found in its live catalog, the image uploaded to Higgsfield's storage first.
#[tokio::test(flavor = "multi_thread")]
async fn an_image_then_image_to_video_with_its_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let script_path = dir.path().join("script.json");
    let media = serde_json::json!({"model": "seedance-2.5", "mode": "image-to-video", "background": false, "input": {"prompt": "The fox runs off", "image_url": "{{last_generated}}", "duration": 5}});
    script(&script_path, &[("generate_image", serde_json::json!({"prompt": "A red fox", "provider": "openai"})), ("generate_media", media)]);
    let (openai_base, _) = fake(openai).await;
    let (hf_base, hf_seen) = fake(higgsfield).await;
    // fal's catalog points at a server that has none: no network.
    let _server = start(&home, &script_path, &[("openai", &openai_base), ("higgsfield", &hf_base), ("fal", &openai_base)]).await;
    let c = Connection::connect(&home).await.unwrap();
    c.media_set_key("openai", Some("sk-test-media".into())).await.unwrap();
    c.media_set_key("higgsfield", Some("hf-id:hf-secret-value".into())).await.unwrap();
    let options = otto_client::SessionCreateOptions {
        project_root: Some(project.display().to_string()),
        approval_mode: Some(ApprovalMode::Auto),
        ..Default::default()
    };
    let session = c.session_create(options).await.unwrap();
    let input = vec![InputPart::Text { text: "draw a fox, then animate it".into() }];
    c.call(Method::SessionPrompt { session_id: session.id.clone(), input, delivery: Delivery::Queue }).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let view = loop {
        let view = c.session_view(&session.id).await.unwrap();
        if view.info.status == SessionStatus::Idle && results(&view).len() == 2 {
            break view;
        }
        assert!(Instant::now() < deadline, "the turn never finished");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let results = results(&view);
    let (image, _) = generated(&results["call-1"].0);
    let (text, error) = &results["call-2"];
    assert!(!error, "{text}");
    assert!(text.contains("Generated 1 video with Higgsfield bytedance/seedance-2.5/image-to-video."), "{text}");
    let (video, line) = generated(text);
    assert!(line.contains("the-fox-runs-off.mp4, video/mp4, ") && line.ends_with("1280×720, 8 s"), "{line}");
    assert_eq!(c.attachment_get(&session.id, &video, 0, None).await.unwrap().size, mp4().len() as u64);
    // The image was uploaded first, and the model got its link.
    let seen = hf_seen.lock().unwrap().clone();
    assert_eq!(c.attachment_get(&session.id, &image, 0, None).await.unwrap().mime, "image/png");
    let slot = seen.iter().find(|r| r.starts_with("POST /files/generate-upload-url ")).expect("an upload slot");
    assert!(slot.contains("\"content_type\":\"image/png\""), "{slot}");
    let upload = seen.iter().find(|r| r.starts_with("PUT /upload ")).expect("an upload");
    assert!(upload.split("\r\n\r\n").nth(1).unwrap().contains("PNG"), "the PNG");
    let submit = seen.iter().find(|r| r.starts_with("POST /bytedance/seedance-2.5/image-to-video ")).expect("a submit");
    assert!(submit.to_ascii_lowercase().contains("authorization: key hf-id:hf-secret-value"));
    let body: serde_json::Value = serde_json::from_str(submit.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body, serde_json::json!({"prompt": "The fox runs off", "image_url": "https://cdn.test/fox.png", "duration": 5}));
    for (text, _) in results.values() {
        assert!(!text.contains("hf-secret-value") && !text.contains("sk-test-media"));
    }
}
