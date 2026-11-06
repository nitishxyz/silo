//! Background generations (docs/MEDIA.md §6.5): a session job with a recording mailbox, a fake xAI.
mod fake;

use fake::{json, serve};
use otto_llm::auth::CredentialStore;
use otto_media::Media;
use otto_media::providers::Xai;
use otto_tools::jobs::{JobKind, Jobs};
use otto_tools::{Mailbox, ToolCx, ToolErrorKind};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Mail(Mutex<Vec<String>>);
impl Mailbox for Mail {
    fn deliver(&self, text: String) {
        self.0.lock().unwrap().push(text);
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    media: Arc<Media>,
    jobs: Arc<Jobs>,
    mail: Arc<Mail>,
    got: Arc<Mutex<Vec<fake::Got>>>,
}

/// xAI at a fake server whose video is done after `polls` pending answers (never with `None`).
async fn fixture(polls: Option<usize>) -> Fixture {
    let seen = Arc::new(Mutex::new(0));
    let (base, got) = serve(move |got, base| match got.path.as_str() {
        "/v1/videos/generations" => json(200, json!({"request_id": "v1"})),
        "/v1/videos/v1" => {
            let mut n = seen.lock().unwrap();
            *n += 1;
            if polls.is_some_and(|p| *n > p) {
                json(200, json!({"status": "done", "video": {"url": format!("{base}/out.mp4")}}))
            } else {
                json(200, json!({"status": "pending", "progress": 40}))
            }
        }
        "/v1/images/generations" => json(200, json!({"data": [{"b64_json": fake::b64(&fake::png())}]})),
        _ => fake::bytes("video/mp4", fake::mp4()),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(dir.path().join("auth")).unwrap();
    let mut providers = otto_media::providers::with_bases(&|_| None);
    providers[2] = Arc::new(Xai::new(&base).polling(Duration::from_millis(5)));
    let media = Media::with(store, dir.path().join("state"), providers, Arc::new(|_: &str| None));
    std::fs::create_dir_all(dir.path().join("project")).unwrap();
    Fixture { dir, media, jobs: Arc::default(), mail: Arc::default(), got }
}

impl Fixture {
    async fn run(&self, name: &str, input: Value) -> Result<otto_tools::ToolOutput, otto_tools::ToolError> {
        let mut cx = ToolCx::new("call1", self.dir.path().join("project"));
        cx.attachments = Some(self.dir.path().join("attachments"));
        cx.sessions = vec!["s1".into()];
        cx.mailbox = Some(self.mail.clone());
        cx.jobs = Some(self.jobs.clone());
        let tool = self.media.tools().into_iter().find(|t| t.spec().name == name).unwrap();
        tool.call(input, cx).await
    }
    async fn report(&self) -> String {
        for _ in 0..500 {
            if let Some(text) = self.mail.0.lock().unwrap().pop() {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no report")
    }
    fn polls(&self) -> usize {
        self.got.lock().unwrap().iter().filter(|g| g.path == "/v1/videos/v1").count()
    }
}

#[tokio::test]
async fn a_video_runs_as_a_job_and_reports_its_result() {
    let f = fixture(Some(3)).await;
    f.media.set_key("xai", Some("x-key".into())).unwrap();
    let out = f.run("generate_video", json!({"prompt": "Fox running through snow", "aspect_ratio": "9:16", "path": "clips/fox.mp4"})).await.unwrap();
    let id = out.metadata["job_id"].as_str().unwrap().to_owned();
    assert_eq!(
        out.joined_text(),
        format!(
            "Started media {id} (video with xAI Grok Imagine Video 1.5); the result arrives as a message when it's done. Keep talking with the user; don't wait or poll for it."
        )
    );
    assert_eq!(
        out.metadata,
        json!({"job_id": id, "background": true, "kind": "video", "provider": "xai", "model": "grok-imagine-video-1.5", "aspect_ratio": "9:16"})
    );
    let listed = f.jobs.list("s1").await;
    assert_eq!((listed[0].kind, listed[0].command.as_str()), (JobKind::Media, "generate_video: Fox running through snow"));
    let report = f.report().await;
    let mut lines = report.lines();
    assert_eq!(lines.next(), Some(format!("Background media {id} (video) finished.").as_str()));
    assert_eq!(lines.next(), Some("Generated 1 video with xAI grok-imagine-video-1.5."));
    let line = lines.next().unwrap();
    assert!(
        line.starts_with("[generated att_") && line.contains("fox-running-through-snow.mp4, video/mp4, ") && line.contains("1280×720, 8 s"),
        "{line}"
    );
    assert_eq!(lines.next(), Some("Saved to clips/fox.mp4"));
    // Stored like a foreground result.
    let att = &line["[generated ".len()..line.find(':').unwrap()];
    let stored = otto_tools::attachments::dir(&f.dir.path().join("attachments"), "s1");
    assert!(std::fs::read_dir(stored).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(att)), "{att} stored");
    assert_eq!(std::fs::read(f.dir.path().join("project/clips/fox.mp4")).unwrap(), fake::mp4());
    let listed = f.jobs.list("s1").await;
    assert!(listed[0].completed);
    assert!(listed[0].progress.as_deref().is_some_and(|p| p.starts_with("Generating with xAI… 40 %")), "{listed:?}");
}

#[tokio::test]
async fn kill_stops_the_generation() {
    let f = fixture(None).await;
    f.media.set_key("xai", Some("x-key".into())).unwrap();
    let out = f.run("generate_video", json!({"prompt": "slow"})).await.unwrap();
    let id = out.metadata["job_id"].as_str().unwrap().to_owned();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(f.jobs.kill("s1", &id).await);
    assert_eq!(f.report().await, format!("Background media {id} (video) was stopped."));
    let polled = f.polls();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.polls(), polled, "no more polling");
}

#[tokio::test]
async fn validation_and_setup_stay_in_the_call() {
    let f = fixture(Some(0)).await;
    let error = f.run("generate_video", json!({"prompt": "x", "aspect_ratio": "4:3"})).await.unwrap_err();
    assert_eq!(error.kind, ToolErrorKind::InvalidInput);
    let error = f.run("generate_video", json!({"prompt": "x", "background": "yes"})).await.unwrap_err();
    assert_eq!(error.message, "background must be true or false");
    // No provider and no one to ask: the call fails, no job starts.
    let error = f.run("generate_video", json!({"prompt": "x"})).await.unwrap_err();
    assert!(error.message.starts_with("No video provider is connected"), "{}", error.message);
    assert!(f.jobs.list("s1").await.is_empty());
}

#[tokio::test]
async fn images_stay_in_the_call_unless_asked() {
    let f = fixture(Some(0)).await;
    f.media.set_key("xai", Some("x-key".into())).unwrap();
    let out = f.run("generate_image", json!({"prompt": "a dot"})).await.unwrap();
    assert!(out.joined_text().starts_with("Generated 1 image with xAI "), "{}", out.joined_text());
    assert!(f.jobs.list("s1").await.is_empty());
    let out = f.run("generate_image", json!({"prompt": "a dot", "n": 2, "background": true})).await.unwrap();
    assert_eq!((out.metadata["kind"].as_str(), out.metadata["n"].as_u64()), (Some("image"), Some(2)));
    let report = f.report().await;
    assert!(report.contains("(image) finished.\nGenerated ") && report.contains("[generated att_"), "{report}");
}

#[tokio::test]
async fn a_foreground_generation_can_be_detached() {
    let f = fixture(Some(10)).await;
    f.media.set_key("xai", Some("x-key".into())).unwrap();
    let call = f.run("generate_video", json!({"prompt": "fox", "background": false}));
    let detach = async {
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(f.jobs.detach("s1", "call1"));
    };
    let (out, ()) = tokio::join!(call, detach);
    let out = out.unwrap();
    let id = out.metadata["job_id"].as_str().unwrap().to_owned();
    assert!(out.joined_text().starts_with(&format!("Started media {id} (video with xAI Grok Imagine Video 1.5): the user detached")));
    assert_eq!(out.metadata["detached"], true);
    let report = f.report().await;
    assert!(
        report.starts_with(&format!("Background media {id} (video) finished.\nGenerated 1 video")) && report.contains("[generated att_"),
        "{report}"
    );
}
