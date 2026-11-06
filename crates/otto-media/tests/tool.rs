//! `generate_image` and `list_media_models` end to end with fake providers.

mod fake;

use otto_llm::auth::CredentialStore;
use otto_media::{Call, Generated, ImageRequest, Media, MediaError, MediaKind, MediaProvider, ModelEntry, Output};
use otto_tools::{Approver, OutputPart, ToolCx};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Fake {
    id: &'static str,
    name: &'static str,
    got: Arc<Mutex<Vec<ImageRequest>>>,
}

#[async_trait::async_trait]
impl MediaProvider for Fake {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn kinds(&self) -> &'static [MediaKind] {
        &[MediaKind::Image]
    }
    fn models(&self) -> Vec<ModelEntry> {
        otto_media::providers::all().into_iter().find(|p| p.id() == self.id).unwrap().models()
    }
    fn env(&self) -> &'static [&'static str] {
        &[]
    }
    async fn generate_image(&self, key: &str, request: ImageRequest, _call: &Call) -> Result<Output, MediaError> {
        assert!(key.starts_with("key-"), "{key}");
        let n = request.n;
        let model = request.model.clone();
        self.got.lock().unwrap().push(request);
        Ok(Output { media: (0..n).map(|_| Generated { bytes: fake::png(), mime: "image/png".into() }).collect(), model, ..Output::default() })
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    media: Arc<Media>,
    got: Arc<Mutex<Vec<ImageRequest>>>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(dir.path().join("auth")).unwrap();
    let got: Arc<Mutex<Vec<ImageRequest>>> = Arc::default();
    let providers: Vec<Arc<dyn MediaProvider>> =
        [("openai", "OpenAI"), ("google", "Gemini"), ("xai", "xAI"), ("higgsfield", "Higgsfield"), ("fal", "fal")]
            .into_iter()
            .map(|(id, name)| Arc::new(Fake { id, name, got: got.clone() }) as Arc<dyn MediaProvider>)
            .chain([Arc::new(otto_media::providers::ElevenLabs::default()) as Arc<dyn MediaProvider>])
            .collect();
    let media = Media::with(store, dir.path().join("state"), providers, Arc::new(|_: &str| None));
    std::fs::create_dir_all(dir.path().join("project")).unwrap();
    Fixture { dir, media, got }
}

impl Fixture {
    fn cx(&self, approver: Option<Arc<dyn Approver>>) -> ToolCx {
        let mut cx = ToolCx::new("call1", self.dir.path().join("project"));
        cx.attachments = Some(self.dir.path().join("attachments"));
        cx.sessions = vec!["s1".into()];
        cx.approver = approver;
        cx
    }
    async fn run(&self, name: &str, input: Value, approver: Option<Arc<dyn Approver>>) -> Result<otto_tools::ToolOutput, otto_tools::ToolError> {
        let tool = self.media.tools().into_iter().find(|t| t.spec().name == name).unwrap();
        tool.call(input, self.cx(approver)).await
    }
}

/// Answers every question with `answer`, saving `key` for it first (as the setup card does).
struct Answers {
    answer: Option<&'static str>,
    key: Option<(Arc<Media>, &'static str)>,
    asked: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait::async_trait]
impl Approver for Answers {
    async fn approve(&self, _: &str, _: &str, _: Value) -> bool {
        true
    }
    async fn ask(&self, _: &str, question: &str, options: &[String]) -> Option<String> {
        self.asked.lock().unwrap().push((question.into(), options.to_vec()));
        if let Some((media, provider)) = &self.key {
            media.set_key(provider, Some(format!("key-{provider}"))).unwrap();
        }
        self.answer.map(str::to_owned)
    }
}

fn answers(answer: Option<&'static str>, key: Option<(Arc<Media>, &'static str)>) -> Arc<Answers> {
    Arc::new(Answers { answer, key, asked: Mutex::default() })
}

fn text(output: &otto_tools::ToolOutput) -> String {
    output.joined_text()
}

#[tokio::test]
async fn generates_stores_saves_and_shows() {
    let f = fixture();
    f.media.set_key("openai", Some("key-openai".into())).unwrap();
    std::fs::write(f.dir.path().join("project/ref.png"), fake::png()).unwrap();
    let attached = otto_tools::attachments::put(&f.dir.path().join("attachments/s1"), "shot.png", "image/png", &fake::png()).unwrap();
    let input = json!({"prompt": "Sunset over the dunes, wide!", "n": 2, "path": "assets/logo.png", "references": ["ref.png", attached.id], "aspect_ratio": "3:2"});
    let output = f.run("generate_image", input, None).await.unwrap();
    let text = text(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "Generated 2 images with OpenAI gpt-image-2.6-flare.");
    for (i, line) in lines[1..3].iter().enumerate() {
        let id = &line["[generated ".len().."[generated att_12345678".len()];
        assert!(otto_tools::attachments::is_id(id), "{line}");
        assert_eq!(
            *line,
            format!(
                "[generated {id}: sunset-over-the-dunes-wide-{}.png, image/png, {}, 2×1 — copy_attachment can save it into the project]",
                i + 1,
                otto_tools::attachments::size(fake::png().len() as u64)
            )
        );
        let (stored, _) = otto_tools::attachments::get(&f.dir.path().join("attachments/s1"), id).unwrap();
        assert_eq!(stored.mime, "image/png");
    }
    assert_eq!(&lines[3..], ["Saved to assets/logo-1.png", "Saved to assets/logo-2.png"]);
    assert_eq!(std::fs::read(f.dir.path().join("project/assets/logo-2.png")).unwrap(), fake::png());
    assert_eq!(output.content.iter().filter(|p| matches!(p, OutputPart::Image { .. })).count(), 2);
    assert_eq!(output.metadata["provider"], "openai");
    assert_eq!(output.metadata["media"][0]["width"], 2);
    assert!(output.metadata["media"][1]["path"].as_str().unwrap().ends_with("assets/logo-2.png"));
    let got = f.got.lock().unwrap();
    assert_eq!((got[0].references.len(), got[0].aspect_ratio.as_deref(), got[0].n), (2, Some("3:2"), 2));
}

#[tokio::test]
async fn saving_as_another_format_converts() {
    let f = fixture();
    f.media.set_key("fal", Some("key-fal".into())).unwrap();
    let output = f.run("generate_image", json!({"prompt": "dot", "path": "dot.jpg"}), None).await.unwrap();
    assert!(text(&output).starts_with("Generated 1 image with fal blackforestlabs/flux-3/text-to-image.\n[generated att_"));
    let saved = std::fs::read(f.dir.path().join("project/dot.jpg")).unwrap();
    assert_eq!(otto_tools::images::sniff(&saved), Some("image/jpeg"));
}

#[tokio::test]
async fn the_default_then_the_first_connected_provider() {
    let f = fixture();
    f.media.set_key("xai", Some("key-xai".into())).unwrap();
    f.media.set_key("google", Some("key-google".into())).unwrap();
    let output = f.run("generate_image", json!({"prompt": "dot"}), None).await.unwrap();
    assert!(text(&output).starts_with("Generated 1 image with Gemini gemini-nano-banana-2.1."), "{}", text(&output));
    f.media.set_default(MediaKind::Image, Some("xai".into()), Some("grok-custom".into())).unwrap();
    let output = f.run("generate_image", json!({"prompt": "dot"}), None).await.unwrap();
    assert!(text(&output).starts_with("Generated 1 image with xAI grok-custom."));
    let output = f.run("generate_image", json!({"prompt": "dot", "provider": "google", "model": "gemini-3-pro-image"}), None).await.unwrap();
    assert!(text(&output).starts_with("Generated 1 image with Gemini gemini-3-pro-image."));
    let error = f.run("generate_image", json!({"prompt": "dot", "provider": "elevenlabs"}), None).await.unwrap_err();
    assert!(error.message.starts_with("Unknown image provider elevenlabs"), "{}", error.message);
    let error = f.run("generate_image", json!({"prompt": "dot", "n": 5}), None).await.unwrap_err();
    assert_eq!(error.message, "n must be 1 to 4");
}

#[tokio::test]
async fn no_provider_asks_and_goes_on_with_the_answer() {
    let f = fixture();
    let asker = answers(Some("FAL"), Some((f.media.clone(), "fal")));
    let output = f.run("generate_image", json!({"prompt": "dot"}), Some(asker.clone())).await.unwrap();
    assert!(text(&output).starts_with("Generated 1 image with fal "), "{}", text(&output));
    let asked = asker.asked.lock().unwrap();
    assert_eq!(asked[0].0, "Image generation needs a provider. Connect one to continue.");
    assert_eq!(asked[0].1, ["OpenAI", "Gemini", "xAI", "Higgsfield", "fal", "Not now"]);
}

#[tokio::test]
async fn no_provider_and_no_answer_fail_plainly() {
    let f = fixture();
    let none = "No image provider is connected. Connect one in Otto settings → Providers.";
    for approver in [Some(answers(Some("Not now"), None) as Arc<dyn Approver>), Some(answers(None, None)), None] {
        let error = f.run("generate_image", json!({"prompt": "dot"}), approver).await.unwrap_err();
        assert_eq!(error.message, none);
    }
    let error = f.run("generate_image", json!({"prompt": "dot"}), Some(answers(Some("fal"), None))).await.unwrap_err();
    assert_eq!(error.message, "No API key for fal");
    // The requested provider without a key asks too.
    f.media.set_key("openai", Some("key-openai".into())).unwrap();
    let asker = answers(Some("xai"), Some((f.media.clone(), "xai")));
    let output = f.run("generate_image", json!({"prompt": "dot", "provider": "xai"}), Some(asker.clone())).await.unwrap();
    assert!(text(&output).starts_with("Generated 1 image with xAI "));
    assert_eq!(asker.asked.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn list_media_models_says_who_is_connected() {
    let f = fixture();
    f.media.set_key("google", Some("key-google".into())).unwrap();
    let output = f.run("list_media_models", json!({"kind": "image"}), None).await.unwrap();
    let text = text(&output);
    assert!(text.starts_with("Image models:\nOpenAI (openai) — not connected\n- gpt-image-2.6-flare (default) — GPT Image 2.6 Flare: "), "{text}");
    assert!(text.contains("Gemini (google) — connected\n- nano-banana-2.1 (default) — Nano Banana 2.1: "));
    assert!(!text.contains("ElevenLabs"));
    let output = f.run("list_media_models", json!({"provider": "elevenlabs"}), None).await.unwrap();
    let audio = output.joined_text();
    assert!(audio.starts_with("Audio models:\nElevenLabs (elevenlabs) — not connected\n- eleven-v4 (default)"), "{audio}");
    assert!(audio.contains("Voices:\n- George (JBFqnCBsd6RMkjVDRZzb, default): "), "{audio}");
    assert!(f.run("list_media_models", json!({"kind": "smell"}), None).await.is_err());
}
