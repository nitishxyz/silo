//! Attachments: stored per session, noted in the prompt (images downscaled),
//! copied by `copy_attachment`, removed with the session.
use otto_core::*;
use otto_llm::*;
use otto_proto::{Delivery, Event, InputPart};
use otto_store::Store;
use std::{
    collections::VecDeque,
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
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(w, h).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}
/// Auto approval (the fresh default); the model reads images and, when `pdf`, PDFs.
async fn setup(pdf: bool) -> (tempfile::TempDir, Runtime, SessionHandle, Arc<Scripted>) {
    let dir = tempfile::tempdir().unwrap();
    let mut info = ModelInfo { id: "test".into(), provider: "test".into(), name: "test".into(), caps: Caps::default(), pricing: None };
    info.caps.vision = true;
    info.caps.pdf = pdf;
    let model = Arc::new(Scripted { info, scripts: Mutex::new(VecDeque::new()), requests: Mutex::new(vec![]) });
    let runtime = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        models: Arc::new(Resolver(model.clone())),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap();
    let session = runtime
        .create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), agent: "build".into(), ..Default::default() })
        .await
        .unwrap();
    (dir, runtime, session, model)
}
async fn finish(live: &mut LiveEvents) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if matches!(live.recv().await.unwrap().1, Event::TurnFinished { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn prompt_notes_attachments_and_sends_images_downscaled() {
    let (dir, r, s, model) = setup(false).await;
    model.scripts.lock().unwrap().push_back(text("seen"));
    let shot = r.attachment_put(&s.id, "shot.png", "image/png", png(3024, 1964), None).await.unwrap();
    // A chunked upload: the second call appends.
    let notes = r.attachment_put(&s.id, "notes.txt", "text/plain", b"hello ".to_vec(), None).await.unwrap();
    let notes = r.attachment_put(&s.id, "notes.txt", "text/plain", b"world".to_vec(), Some(&notes.id)).await.unwrap();
    assert_eq!(notes.size, 11);
    let stored = dir.path().join("state/attachments").join(&s.id);
    assert_eq!(std::fs::read(stored.join(format!("{}-notes.txt", notes.id))).unwrap(), b"hello world");
    assert!(r.attachment_put(&s.id, "big.bin", "", vec![0; otto_proto::MAX_ATTACHMENT + 1], None).await.is_err());
    // Unknown ids are refused before anything is queued.
    assert!(r.prompt(&s.id, vec![InputPart::Attachment { id: "att_0badf00d".into() }], Delivery::Queue).await.is_err());
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    let input =
        vec![InputPart::Attachment { id: shot.id.clone() }, InputPart::Attachment { id: notes.id.clone() }, InputPart::Text { text: "look".into() }];
    r.prompt(&s.id, input, Delivery::Queue).await.unwrap();
    finish(&mut live).await;
    let parts = model.requests.lock().unwrap()[0].items[0].parts.clone();
    assert_eq!(parts.len(), 4, "{parts:?}");
    assert_eq!(
        parts[0],
        Part::text(format!(
            "[attachment {}: shot.png, image/png, {} — copy_attachment can save it into the project]",
            shot.id,
            otto_core::attachments::size(shot.size)
        ))
    );
    let Part::Image(Media { name, source: MediaSource::Base64 { data }, .. }) = &parts[1] else { panic!("{:?}", parts[1]) };
    assert_eq!(name.as_deref(), Some("shot.png"));
    let sent = image::load_from_memory(&otto_core::attachments::decode(data).unwrap()).unwrap();
    assert_eq!((sent.width(), sent.height()), (1568, 1018));
    assert_eq!(
        parts[2],
        Part::text(format!("[attachment {}: notes.txt, text/plain, 11 B — copy_attachment can save it into the project]", notes.id))
    );
    // copy_attachment is deferred: in load_tools' index, not in the first request's tools.
    let first = model.requests.lock().unwrap()[0].clone();
    assert!(!first.tools.iter().any(|t| t.name == "copy_attachment"));
    assert!(first.tools.iter().any(|t| t.name == "load_tools" && t.description.contains("copy_attachment: Save a file the user attached")));
    assert!(first.tools.iter().any(|t| t.name == "read"));
    assert_eq!(parts[3], Part::text("look"));
    // Forks keep them; archiving removes the session's folder.
    let fork = r.fork(&s.id, s.view().await.unwrap().seq).await.unwrap();
    assert!(dir.path().join("state/attachments").join(&fork.id).join(format!("{}.json", shot.id)).exists());
    r.archive(&s.id, true).await.unwrap();
    assert!(!stored.exists());
    assert!(r.attachment_put(&s.id, "late.txt", "text/plain", b"x".to_vec(), None).await.is_err());
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn copy_attachment_and_read_reach_the_sessions_files() {
    let (dir, r, s, model) = setup(false).await;
    let original = png(40, 30);
    let shot = r.attachment_put(&s.id, "shot.png", "image/png", original.clone(), None).await.unwrap();
    std::fs::write(dir.path().join("spec.pdf"), b"%PDF-1.4\n%fake\n").unwrap();
    model.scripts.lock().unwrap().extend([
        call("c1", "copy_attachment", serde_json::json!({"attachment_id":shot.id,"path":"assets/shot.png"})),
        call("c2", "read", serde_json::json!({"attachment_id":shot.id})),
        call("c3", "read", serde_json::json!({"path":"spec.pdf"})),
        text("done"),
    ]);
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    r.prompt(&s.id, vec![InputPart::Text { text: "add it".into() }], Delivery::Queue).await.unwrap();
    finish(&mut live).await;
    assert_eq!(std::fs::read(dir.path().join("assets/shot.png")).unwrap(), original);
    let requests = model.requests.lock().unwrap().clone();
    let result = |i: usize| match requests[i].items.last().unwrap().parts.last().unwrap() {
        Part::ToolResult { content, .. } => content.clone(),
        other => panic!("{other:?}"),
    };
    assert!(matches!(&result(2)[1], ResultPart::Image(_)), "{:?}", result(2));
    // This model reads no PDFs: the result says so instead of sending it.
    let pdf = result(3);
    assert!(!pdf.iter().any(|p| matches!(p, ResultPart::Document(_))));
    assert!(serde_json::to_string(&pdf).unwrap().contains("spec.pdf (application/pdf) not sent"));
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_write_grant_without_copy_attachment_still_reaches_it() {
    let (dir, r, _, model) = setup(false).await;
    std::fs::create_dir_all(dir.path().join(".otto/agents")).unwrap();
    std::fs::write(dir.path().join(".otto/agents/writer.md"), "---\ntools: Read, Write\n---\nWrite.").unwrap();
    let project = r.open_project(dir.path()).await.unwrap();
    let s = r
        .create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), agent: "writer".into(), ..Default::default() })
        .await
        .unwrap();
    let shot = r.attachment_put(&s.id, "shot.png", "image/png", png(8, 8), None).await.unwrap();
    model.scripts.lock().unwrap().extend([
        call("c1", "load_tools", serde_json::json!({"names":["copy_attachment"]})),
        call("c2", "copy_attachment", serde_json::json!({"attachment_id":shot.id,"path":"shot.png"})),
        text("done"),
    ]);
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    r.prompt(&s.id, vec![InputPart::Text { text: "add it".into() }], Delivery::Queue).await.unwrap();
    finish(&mut live).await;
    assert!(dir.path().join("shot.png").exists());
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn pdfs_go_as_documents_to_models_that_read_them() {
    let (dir, r, s, model) = setup(true).await;
    std::fs::write(dir.path().join("spec.pdf"), b"%PDF-1.4\n%fake\n").unwrap();
    model.scripts.lock().unwrap().extend([call("c1", "read", serde_json::json!({"path":"spec.pdf"})), text("done")]);
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    r.prompt(&s.id, vec![InputPart::Text { text: "read".into() }], Delivery::Queue).await.unwrap();
    finish(&mut live).await;
    let requests = model.requests.lock().unwrap().clone();
    let Part::ToolResult { content, .. } = requests[1].items.last().unwrap().parts.last().unwrap() else { panic!() };
    assert!(
        matches!(&content[1], ResultPart::Document(Media { mime, name, .. }) if mime == "application/pdf" && name.as_deref() == Some("spec.pdf"))
    );
    r.shutdown().await.unwrap();
}
