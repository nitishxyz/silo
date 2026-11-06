//! `generate_video`, `generate_audio` and GPT Image 2.6's settings end to end: real providers, a fake server.
mod fake;

use fake::{b64, json, serve};
use otto_llm::auth::CredentialStore;
use otto_media::Media;
use otto_media::providers::{ElevenLabs, OpenAi, Xai};
use otto_tools::{Approver, OutputPart, ToolCx};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Fixture {
    dir: tempfile::TempDir,
    media: Arc<Media>,
    got: Arc<Mutex<Vec<fake::Got>>>,
}

/// xAI, ElevenLabs and OpenAI at a fake server answering videos, speech and images; the rest at their real hosts
/// (never called: they have no key).
async fn fixture(image: Value) -> Fixture {
    let (base, got) = serve(move |got, base| match got.path.as_str() {
        "/v1/videos/generations" => json(200, json!({"request_id": "v1"})),
        "/v1/videos/v1" => json(200, json!({"status": "done", "video": {"url": format!("{base}/out.mp4")}})),
        "/out.mp4" => fake::bytes("video/mp4", fake::mp4()),
        "/v1/images/generations" | "/v1/images/edits" => json(200, image.clone()),
        _ => fake::bytes("audio/mpeg", fake::mp3()),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(dir.path().join("auth")).unwrap();
    let mut providers = otto_media::providers::with_bases(&|_| None);
    providers[0] = Arc::new(OpenAi::new(&base));
    providers[2] = Arc::new(Xai::new(&base).polling(Duration::from_millis(5)));
    providers[5] = Arc::new(ElevenLabs::new(&base));
    let media = Media::with(store, dir.path().join("state"), providers, Arc::new(|_: &str| None));
    std::fs::create_dir_all(dir.path().join("project")).unwrap();
    Fixture { dir, media, got }
}

impl Fixture {
    async fn run(&self, name: &str, input: Value, approver: Option<Arc<dyn Approver>>) -> Result<otto_tools::ToolOutput, otto_tools::ToolError> {
        let mut cx = ToolCx::new("call1", self.dir.path().join("project"));
        cx.attachments = Some(self.dir.path().join("attachments"));
        cx.sessions = vec!["s1".into()];
        cx.approver = approver;
        cx.vision = true;
        let tool = self.media.tools().into_iter().find(|t| t.spec().name == name).unwrap();
        tool.call(input, cx).await
    }
    fn sent(&self, path: &str) -> fake::Got {
        self.got.lock().unwrap().iter().find(|g| g.path == path).cloned().unwrap()
    }
}

/// `[generated att_…: rest` with the id checked and cut.
fn generated(line: &str) -> String {
    let id = &line["[generated ".len().."[generated att_12345678".len()];
    assert!(otto_tools::attachments::is_id(id), "{line}");
    line["[generated att_12345678".len()..].to_owned()
}

#[tokio::test]
async fn generate_video_stores_the_mp4_with_its_size_and_length() {
    let f = fixture(json!({})).await;
    f.media.set_key("xai", Some("x-key".into())).unwrap();
    let input = json!({"prompt": "Fox running through snow", "duration": 8, "aspect_ratio": "16:9", "sound": false, "path": "clips/fox.mp4"});
    let output = f.run("generate_video", input, None).await.unwrap();
    let text = output.joined_text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "Generated 1 video with xAI grok-imagine-video-1.5.");
    let size = otto_tools::attachments::size(fake::mp4().len() as u64);
    assert_eq!(
        generated(lines[1]),
        format!(": fox-running-through-snow.mp4, video/mp4, {size}, 1280×720, 8 s — copy_attachment can save it into the project]")
    );
    assert_eq!(lines[2], "Saved to clips/fox.mp4");
    assert_eq!(std::fs::read(f.dir.path().join("project/clips/fox.mp4")).unwrap(), fake::mp4());
    assert!(!output.content.iter().any(|p| matches!(p, OutputPart::Image { .. })), "no video parts for the model");
    assert_eq!((output.metadata["media"][0]["width"].as_u64(), output.metadata["media"][0]["duration"].as_f64()), (Some(1280), Some(8.0)));
    assert_eq!(f.sent("/v1/videos/generations").json()["generate_audio"], false);
    let error = f.run("generate_video", json!({"prompt": "x", "aspect_ratio": "4:3"}), None).await.unwrap_err();
    assert_eq!(error.message, "aspect_ratio must be one of 16:9, 9:16, 1:1");
}

/// Answers with `answer`, saving its key first (as the setup card does).
struct Answers {
    media: Arc<Media>,
    answer: &'static str,
    asked: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait::async_trait]
impl Approver for Answers {
    async fn approve(&self, _: &str, _: &str, _: Value) -> bool {
        true
    }
    async fn ask(&self, _: &str, question: &str, options: &[String]) -> Option<String> {
        self.asked.lock().unwrap().push((question.into(), options.to_vec()));
        let provider = self.media.provider(self.answer)?;
        self.media.set_key(provider.id(), Some("key".into())).unwrap();
        Some(self.answer.into())
    }
}

#[tokio::test]
async fn generate_video_and_audio_ask_for_a_provider_of_their_kind() {
    let f = fixture(json!({})).await;
    let asker = Arc::new(Answers { media: f.media.clone(), answer: "xAI", asked: Mutex::default() });
    let output = f.run("generate_video", json!({"prompt": "fox"}), Some(asker.clone())).await.unwrap();
    assert!(output.joined_text().starts_with("Generated 1 video with xAI "), "{}", output.joined_text());
    let asker2 = Arc::new(Answers { media: f.media.clone(), answer: "Not now", asked: Mutex::default() });
    let error = f.run("generate_audio", json!({"type": "music", "text": "lofi"}), Some(asker2.clone())).await.unwrap_err();
    assert_eq!(error.message, "No audio provider is connected. Connect one in Otto settings → Providers.");
    let asked: Vec<_> = asker.asked.lock().unwrap().iter().chain(asker2.asked.lock().unwrap().iter()).cloned().collect();
    assert_eq!(asked[0].0, "Video generation needs a provider. Connect one to continue.");
    assert_eq!(asked[0].1, ["Gemini", "xAI", "Higgsfield", "fal", "Not now"]);
    assert_eq!(asked[1].0, "Audio generation needs a provider. Connect one to continue.");
    assert_eq!(asked[1].1, ["ElevenLabs", "Not now"], "music: ElevenLabs only");
}

#[tokio::test]
async fn generate_audio_prefers_elevenlabs_and_reports_the_length() {
    let f = fixture(json!({})).await;
    f.media.set_key("openai", Some("sk-key".into())).unwrap();
    f.media.set_key("elevenlabs", Some("el-key".into())).unwrap();
    let output = f.run("generate_audio", json!({"type": "speech", "text": "Welcome to the show"}), None).await.unwrap();
    let text = output.joined_text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "Generated 1 audio file with ElevenLabs eleven_v4.");
    let size = otto_tools::attachments::size(fake::mp3().len() as u64);
    assert_eq!(generated(lines[1]), format!(": welcome-to-the-show.mp3, audio/mpeg, {size}, 10 s — copy_attachment can save it into the project]"));
    assert_eq!(lines[2], "Voice George (JBFqnCBsd6RMkjVDRZzb).");
    let output = f.run("generate_audio", json!({"type": "speech", "text": "Hi", "provider": "openai", "voice": "nova"}), None).await.unwrap();
    assert!(output.joined_text().starts_with("Generated 1 audio file with OpenAI gpt-4o-mini-tts."));
    let error = f.run("generate_audio", json!({"type": "sound", "text": "door creak", "provider": "openai"}), None).await.unwrap_err();
    assert!(error.message.starts_with("Unknown sound effects provider openai; one of elevenlabs"), "{}", error.message);
}

const SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="512" height="256"><rect width="512" height="256"/></svg>"#;

#[tokio::test]
async fn gpt_image_2_6_svg_is_kept_as_is_and_measured() {
    let f = fixture(json!({"data": [{"b64_json": b64(SVG.as_bytes())}], "output_format": "svg"})).await;
    f.media.set_key("openai", Some("sk-key".into())).unwrap();
    let input = json!({"prompt": "Bakery logo", "format": "svg", "complexity": "low", "quality": "medium", "path": "brand/logo.svg"});
    let output = f.run("generate_image", input, None).await.unwrap();
    let text = output.joined_text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "Generated 1 image with OpenAI gpt-image-2.6-flare.");
    let size = otto_tools::attachments::size(SVG.len() as u64);
    assert_eq!(generated(lines[1]), format!(": bakery-logo.svg, image/svg+xml, {size}, 512×256 — copy_attachment can save it into the project]"));
    assert_eq!(std::fs::read_to_string(f.dir.path().join("project/brand/logo.svg")).unwrap(), SVG);
    assert!(!output.content.iter().any(|p| matches!(p, OutputPart::Image { .. })), "no image part for an SVG");
    assert_eq!(
        f.sent("/v1/images/generations").json(),
        json!({"model": "gpt-image-2.6-flare", "prompt": "Bakery logo", "n": 1, "quality": "medium", "complexity": "low", "output_format": "svg"})
    );
    let error = f.run("generate_image", json!({"prompt": "logo", "format": "svg", "path": "logo.png"}), None).await.unwrap_err();
    assert!(error.message.starts_with("can't save image/svg+xml as "), "{}", error.message);
}

#[tokio::test]
async fn gpt_image_2_6_gif_sends_frames_and_shows_the_first_one() {
    let f = fixture(json!({"data": [{"b64_json": b64(&fake::gif())}], "output_format": "gif"})).await;
    f.media.set_key("openai", Some("sk-key".into())).unwrap();
    let input = json!({"prompt": "Lantern loop", "format": "gif", "frame_count": 12, "frame_duration_ms": 100, "path": "loop.gif"});
    let output = f.run("generate_image", input, None).await.unwrap();
    let text = output.joined_text();
    let size = otto_tools::attachments::size(fake::gif().len() as u64);
    assert_eq!(
        generated(text.lines().nth(1).unwrap()),
        format!(": lantern-loop.gif, image/gif, {size}, 2×1 — copy_attachment can save it into the project]")
    );
    assert_eq!(std::fs::read(f.dir.path().join("project/loop.gif")).unwrap(), fake::gif(), "a .gif path keeps the bytes");
    let parts: Vec<&str> =
        output.content.iter().filter_map(|p| if let OutputPart::Image { mime, .. } = p { Some(mime.as_str()) } else { None }).collect();
    assert_eq!(parts, ["image/png"], "the first frame");
    let sent = f.sent("/v1/images/generations").json();
    assert_eq!(
        (sent["output_format"].as_str(), sent["frame_count"].as_u64(), sent["frame_duration_ms"].as_u64()),
        (Some("gif"), Some(12), Some(100))
    );
}

/// A 2×1 PNG, `alpha` opaque or clear.
fn png(alpha: u8) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, alpha])).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

#[tokio::test]
async fn markup_goes_with_the_references_to_the_edits_endpoint() {
    let f = fixture(json!({"data": [{"b64_json": b64(&png(255))}]})).await;
    f.media.set_key("openai", Some("sk-key".into())).unwrap();
    std::fs::write(f.dir.path().join("project/photo.png"), png(255)).unwrap();
    std::fs::write(f.dir.path().join("project/circle.png"), png(0)).unwrap();
    let input = json!({"prompt": "Make the circled vase blue", "references": ["photo.png"], "markup": "circle.png", "complexity": "low"});
    f.run("generate_image", input, None).await.unwrap();
    let body = f.sent("/v1/images/edits").text();
    assert_eq!(body.matches("name=\"image[]\"").count(), 1, "{body}");
    assert!(body.contains("name=\"markup\"; filename=\"markup.png\"\r\nContent-Type: image/png"), "{body}");
    assert!(body.contains("name=\"complexity\"\r\n\r\nlow"));
    let error = f.run("generate_image", json!({"prompt": "x", "references": ["photo.png"], "markup": "photo.png"}), None).await.unwrap_err();
    assert!(error.message.contains("must be a PNG with transparent pixels"), "{}", error.message);
    // Older models say what they ignored.
    let input = json!({"prompt": "x", "model": "gpt-image-2", "complexity": "high", "format": "svg"});
    let output = f.run("generate_image", input, None).await.unwrap();
    let text = output.joined_text();
    assert!(text.contains("\ncomplexity is not supported by gpt-image-2; ignored."), "{text}");
    assert!(text.contains("\nformat svg is not supported by gpt-image-2 (GPT Image 2.6 makes it); ignored, made PNG."), "{text}");
}

#[tokio::test]
async fn contradictory_image_settings_are_refused() {
    let f = fixture(json!({})).await;
    std::fs::write(f.dir.path().join("project/photo.png"), png(255)).unwrap();
    for (input, message) in [
        (json!({"prompt": "x", "format": "svg", "n": 2}), "format svg makes one image: n must be 1"),
        (json!({"prompt": "x", "format": "gif", "references": ["photo.png"]}), "format gif is for new images only: not with references"),
        (json!({"prompt": "x", "format": "svg", "references": ["photo.png"]}), "format svg is for new images only: not with references"),
        (json!({"prompt": "x", "frame_count": 4}), "frame_count and frame_duration_ms are for format gif"),
        (json!({"prompt": "x", "format": "gif", "frame_count": 65}), "frame_count must be 2 to 64"),
        (json!({"prompt": "x", "markup": "photo.png"}), "markup needs references: it marks up the first one"),
        (json!({"prompt": "x", "format": "jpeg", "transparent": true}), "transparent needs format png or webp, not jpeg"),
    ] {
        let error = f.run("generate_image", input, None).await.unwrap_err();
        assert_eq!(error.message, message);
    }
}
