//! Live catalogs with input schemas (docs/MEDIA.md §3.2): grouping, caching, `list_media_models`,
//! `media_model_schema` and `generate_media` against fake Higgsfield and fal servers.
mod fake;

use fake::{Got, json, mp3, mp4, png, serve};
use otto_llm::auth::CredentialStore;
use otto_media::providers::{Fal, Higgsfield};
use otto_media::{Media, MediaKind, MediaProvider};
use otto_tools::ToolCx;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HIGGSFIELD: &str = include_str!("fixtures/higgsfield-models.json");
const FAL_MODELS: &str = include_str!("fixtures/fal-models.json");
const FAL_OPENAPI: &str = include_str!("fixtures/fal-openapi.json");

/// Higgsfield: the catalog, uploads (`https://cdn.test/<n>`), a submit to any model, a completed status, the files.
async fn higgsfield() -> (String, Arc<Mutex<Vec<Got>>>) {
    let uploads = Arc::new(AtomicUsize::new(0));
    serve(move |got, base| match (got.method.as_str(), got.path.as_str()) {
        ("GET", "/models?include_schema=true") => fake::bytes("application/json", HIGGSFIELD.as_bytes().to_vec()),
        ("POST", "/files/generate-upload-url") => {
            let n = uploads.fetch_add(1, Ordering::SeqCst) + 1;
            json(200, json!({"upload_url": format!("{base}/upload/{n}"), "public_url": format!("https://cdn.test/{n}")}))
        }
        ("PUT", _) => fake::bytes("text/plain", vec![]),
        ("GET", "/requests/r1/status") => json(
            200,
            json!({"status": "completed", "request_id": "r1", "video": {"url": format!("{base}/out.mp4")}, "status_url": "https://x.test/s"}),
        ),
        ("GET", "/out.mp4") => fake::bytes("video/mp4", mp4()),
        ("POST", _) => json(200, json!({"status": "queued", "request_id": "r1", "status_url": format!("{base}/requests/r1/status")})),
        _ => json(404, json!({"detail": "no"})),
    })
    .await
}

/// fal: listings by category, one endpoint's OpenAPI, storage, the queue.
async fn fal() -> (String, Arc<Mutex<Vec<Got>>>) {
    serve(move |got, base| {
        let path = got.path.as_str();
        match (got.method.as_str(), path) {
            ("GET", p) if p.contains("expand=openapi-3.0") => fake::bytes("application/json", FAL_OPENAPI.as_bytes().to_vec()),
            ("GET", p) if p.starts_with("/v1/models?category=image-to-video") => fake::bytes("application/json", FAL_MODELS.as_bytes().to_vec()),
            ("GET", p) if p.starts_with("/v1/models") => json(200, json!({"models": []})),
            ("POST", p) if p.starts_with("/storage/upload/initiate") => {
                json(200, json!({"upload_url": format!("{base}/put/1"), "file_url": "https://v3.fal.media/files/in.png"}))
            }
            ("PUT", _) => fake::bytes("text/plain", vec![]),
            ("POST", _) => json(
                200,
                json!({"request_id": "q1", "status_url": format!("{base}/q/status"), "response_url": format!("{base}/q/result"), "cancel_url": format!("{base}/q/cancel")}),
            ),
            ("GET", "/q/status") => json(200, json!({"status": "COMPLETED"})),
            ("GET", "/q/result") => json(200, json!({"video": {"url": format!("{base}/v.mp4"), "content_type": "video/mp4"}, "seed": 7})),
            ("GET", "/v.mp4") => fake::bytes("video/mp4", mp4()),
            _ => json(404, json!({"detail": "no"})),
        }
    })
    .await
}

struct Fixture {
    dir: tempfile::TempDir,
    media: Arc<Media>,
}

fn media_at(dir: &std::path::Path, higgsfield: &str, fal: &str) -> Arc<Media> {
    let store = CredentialStore::open(dir.join("auth")).unwrap();
    let providers: Vec<Arc<dyn MediaProvider>> = vec![
        Arc::new(Higgsfield::new(higgsfield, Duration::from_millis(5))),
        Arc::new(Fal::new(fal, fal, fal).storage(fal).polling(Duration::from_millis(5))),
    ];
    Media::with(store, dir.join("state"), providers, Arc::new(|_: &str| None))
}

fn fixture(higgsfield: &str, fal: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("project")).unwrap();
    let media = media_at(dir.path(), higgsfield, fal);
    Fixture { dir, media }
}

impl Fixture {
    fn cx(&self) -> ToolCx {
        let mut cx = ToolCx::new("call1", self.dir.path().join("project"));
        cx.attachments = Some(self.dir.path().join("attachments"));
        cx.sessions = vec!["s1".into()];
        cx
    }
    async fn run(&self, name: &str, input: Value) -> Result<otto_tools::ToolOutput, otto_tools::ToolError> {
        let tool = self.media.tools().into_iter().find(|t| t.spec().name == name).unwrap();
        tool.call(input, self.cx()).await
    }
    /// An attachment of the session: its id.
    fn attach(&self, name: &str, mime: &str, bytes: &[u8]) -> String {
        otto_tools::attachments::put(&self.dir.path().join("attachments/s1"), name, mime, bytes).unwrap().id
    }
}

fn text(output: &otto_tools::ToolOutput) -> String {
    output.joined_text()
}

#[tokio::test]
async fn higgsfield_catalog_groups_each_family_into_modes() {
    let (base, got) = higgsfield().await;
    let endpoints = Higgsfield::new(&base, Duration::from_millis(5)).endpoints(&reqwest::Client::new(), Some("id:secret")).await.unwrap();
    assert_eq!(got.lock().unwrap()[0].header("authorization"), Some("Key id:secret"));
    assert_eq!(endpoints.len(), 32);
    assert!(endpoints.iter().all(|e| e.schema.as_ref().is_some_and(|s| s["properties"].is_object())));
    let families = otto_media::families(&endpoints);
    let modes = |key: &str| -> (String, Vec<String>) {
        let f = families.iter().find(|f| f.key == key).unwrap_or_else(|| panic!("no {key}"));
        (f.name.clone(), f.modes.iter().map(|(m, _)| m.clone()).collect())
    };
    let seedance = modes("bytedance/seedance-2.5");
    assert_eq!(seedance.0, "Seedance 2.5");
    assert_eq!(seedance.1, ["image-to-video", "reference-to-video", "text-to-video", "video-edit", "video-extend"]);
    assert_eq!(modes("higgsfield/genjutsu/v1.0"), ("Genjutsu".into(), vec!["motion-transfer".into(), "object-swap".into(), "restyle".into()]));
    assert_eq!(modes("kling-video/v4.0").1, ["image-to-video", "reference-to-video", "text-to-video", "video-edit"]);
    assert_eq!(modes("higgsfield-ai/soul/v2").1, ["image-to-image", "standard"]);
    // The same title twice says where they differ.
    assert_eq!(modes("kling-video/v3/pro").0, "Kling 3.0 (pro)");
    assert_eq!(modes("marketing-studio/image/flare"), ("Marketing Studio Image (flare)".into(), vec![String::new()]));
    assert_eq!(families.iter().find(|f| f.key == "bytedance/seedance-2.5").unwrap().kind, MediaKind::Video);
    assert!(endpoints.iter().find(|e| e.id == "higgsfield/genjutsu/restyle/v1.0").unwrap().operations.contains(&"video2video".into()));
}

#[tokio::test]
async fn the_catalog_is_cached_on_disk_refreshed_when_stale_and_keeps_built_in_handles() {
    let (base, got) = higgsfield().await;
    let f = fixture(&base, "http://127.0.0.1:9");
    let handles = |m: &Media| -> Vec<(String, String, String)> {
        m.status().providers.iter().flat_map(|p| p.models.iter().map(|m| (p.id.clone(), m.id.clone(), m.handle.clone()))).collect()
    };
    let before = handles(&f.media);
    f.media.set_key("higgsfield", Some("id:secret".into())).unwrap();
    f.media.refresh_catalogs(false).await;
    let fetches = || got.lock().unwrap().iter().filter(|g| g.path.starts_with("/models")).count();
    assert_eq!(fetches(), 1);
    let file = f.dir.path().join("state/media-catalog/higgsfield.json");
    let saved: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert!(saved["fetched_at"].as_u64().unwrap() > 1_700_000_000);
    assert_eq!(saved["endpoints"].as_array().unwrap().len(), 32);
    // Fresh: not fetched again; forced: fetched.
    f.media.refresh_catalogs(false).await;
    assert_eq!(fetches(), 1);
    f.media.refresh_catalogs(true).await;
    assert_eq!(fetches(), 2);
    // Built-in handles stay; live families follow them.
    let after = handles(&f.media);
    assert!(before.iter().all(|b| after.contains(b)), "{after:?}");
    let live: Vec<&str> = after.iter().filter(|a| !before.contains(a)).map(|(_, _, h)| h.as_str()).collect();
    for handle in ["genjutsu", "genjutsu-light", "higgsfield-kling-4.0", "higgsfield-kling-3.0-pro", "kling-4.0-flash"] {
        assert!(live.contains(&handle), "{handle} in {live:?}");
    }
    assert_eq!(f.media.resolve_handle("genjutsu").unwrap().2, "higgsfield/genjutsu/v1.0");
    // Another start reads the file, offline; stale, it tries again and keeps what it had.
    let other = media_at(f.dir.path(), "http://127.0.0.1:9", "http://127.0.0.1:9");
    assert!(other.resolve_handle("genjutsu").is_some());
    let mut stale = saved.clone();
    stale["fetched_at"] = json!(1);
    std::fs::write(&file, stale.to_string()).unwrap();
    let other = media_at(f.dir.path(), &base, "http://127.0.0.1:9");
    other.refresh_catalogs(false).await;
    assert_eq!(fetches(), 3);
    assert!(other.resolve_handle("genjutsu").is_some());
}

#[tokio::test]
async fn list_media_models_shows_modes_and_media_inputs() {
    let (base, _) = higgsfield().await;
    let f = fixture(&base, "http://127.0.0.1:9");
    f.media.set_key("higgsfield", Some("id:secret".into())).unwrap();
    let all = text(&f.run("list_media_models", json!({"kind": "video", "provider": "higgsfield"})).await.unwrap());
    let line = all.lines().find(|l| l.starts_with("- seedance-2.5 ")).unwrap_or_else(|| panic!("{all}"));
    assert!(line.starts_with("- seedance-2.5 — Seedance 2.5: Generate, edit and extend with multimodal references."), "{line}");
    assert!(
        line.contains(" Modes: image-to-video (image_url, end_image_url?), reference-to-video (audio_urls ≤10?, image_urls ≤30?, video_urls ≤10?), text-to-video, video-edit (video_url, audio_urls ≤10?, image_urls ≤30?, video_urls ≤10?), "),
        "{line}"
    );
    let genjutsu = all.lines().find(|l| l.starts_with("- genjutsu ")).unwrap();
    assert!(
        genjutsu
            .contains("motion-transfer (image_urls 1–8, video_url), object-swap (image_urls 1–8, video_url), restyle (video_url, image_urls ≤5?)"),
        "{genjutsu}"
    );
    assert!(all.ends_with("then generate_media {model, mode, input}."));
    // By what it takes, or by words.
    let videos = text(&f.run("list_media_models", json!({"input": "video"})).await.unwrap());
    assert!(videos.contains("- genjutsu ") && !videos.contains("z-image") && !videos.contains("Image models"), "{videos}");
    let audio = text(&f.run("list_media_models", json!({"input": "audio", "provider": "higgsfield"})).await.unwrap());
    assert!(audio.contains("- seedance-2.5 ") && !audio.contains("- genjutsu "), "{audio}");
    let swap = text(&f.run("list_media_models", json!({"query": "object swap"})).await.unwrap());
    let swaps: Vec<&str> = swap.lines().filter(|l| l.starts_with("- ")).map(|l| l.split(' ').nth(1).unwrap()).collect();
    assert_eq!(swaps, ["genjutsu-light", "genjutsu"]);
}

#[tokio::test]
async fn media_model_schema_gives_each_mode_with_its_media_fields() {
    let (base, _) = higgsfield().await;
    let f = fixture(&base, "http://127.0.0.1:9");
    f.media.set_key("higgsfield", Some("id:secret".into())).unwrap();
    let out = text(&f.run("media_model_schema", json!({"model": "genjutsu", "mode": "motion"})).await.unwrap());
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "Genjutsu (genjutsu) · Higgsfield higgsfield (connected) · makes video");
    assert_eq!(
        lines[2],
        "Mode motion-transfer — endpoint higgsfield/genjutsu/motion-transfer/v1.0. Run: generate_media {model: \"genjutsu\", mode: \"motion-transfer\", input}."
    );
    assert_eq!(lines[3], "Media fields (project paths, att_ ids or https links): image_urls 1–8 (image), video_url (video)");
    let schema: Value = serde_json::from_str(lines[4].strip_prefix("Input schema: ").unwrap()).unwrap();
    assert_eq!(schema["properties"]["image_urls"]["maxItems"], 8);
    assert!(schema["properties"]["prompt"].get("title").is_none(), "titles are trimmed");
    // An endpoint id picks its mode; no mode shows them all.
    let one = text(&f.run("media_model_schema", json!({"model": "higgsfield/bytedance/seedance-2.5/video-extend"})).await.unwrap());
    assert!(one.contains("Mode video-extend — ") && !one.contains("Mode video-edit"), "{one}");
    let all = text(&f.run("media_model_schema", json!({"model": "seedance-2.5"})).await.unwrap());
    assert_eq!(all.matches("\nMode ").count(), 5);
    assert!(all.contains("Modes: image-to-video, reference-to-video, text-to-video, video-edit, video-extend. Pass mode to generate_media."));
    let error = f.run("media_model_schema", json!({"model": "seedance-2.5", "mode": "video"})).await.unwrap_err();
    assert!(error.message.starts_with("Seedance 2.5 has no mode video; its modes: "), "{}", error.message);
    let error = f.run("media_model_schema", json!({"model": "nope"})).await.unwrap_err();
    assert_eq!(error.message, "No media model nope; list_media_models lists them");
}

/// What reached `path` on the fake, as JSON.
fn sent(got: &Arc<Mutex<Vec<Got>>>, path: &str) -> Value {
    got.lock().unwrap().iter().find(|g| g.method == "POST" && g.path == path).unwrap_or_else(|| panic!("nothing sent to {path}")).json()
}

#[tokio::test]
async fn inputs_are_checked_against_the_schema() {
    let (base, got) = higgsfield().await;
    let f = fixture(&base, "http://127.0.0.1:9");
    f.media.set_key("higgsfield", Some("id:secret".into())).unwrap();
    let run = |mode: &str, input: Value| f.run("generate_media", json!({"model": "seedance-2.5", "mode": mode, "input": input}));
    let error = run("video-edit", json!({"prompt": "make it night"})).await.unwrap_err();
    assert_eq!(
        error.message,
        "Seedance 2.5 video-edit input: \"video_url\" is a required property. media_model_schema {model: \"seedance-2.5\", mode: \"video-edit\"} shows its fields"
    );
    let error = run("text-to-video", json!({"prompt": "a fox", "resolution": "8k"})).await.unwrap_err();
    assert!(error.message.contains("resolution: \"8k\" is not one of "), "{}", error.message);
    let error = run("text-to-video", json!({"prompt": "a fox", "duration": 99})).await.unwrap_err();
    assert!(error.message.contains("duration: 99 is greater than the maximum of 30"), "{}", error.message);
    let images: Vec<String> = (0..9).map(|i| format!("https://x.test/{i}.png")).collect();
    let error = f
        .run(
            "generate_media",
            json!({"model": "genjutsu", "mode": "motion-transfer", "input": {"video_url": "https://x.test/v.mp4", "image_urls": images}}),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("image_urls: ") && error.message.contains("has more than 8 items"), "{}", error.message);
    // A file of the wrong kind, and a missing one.
    let song = f.attach("song.mp3", "audio/mpeg", &mp3());
    let error = run("video-edit", json!({"prompt": "p", "video_url": song})).await.unwrap_err();
    assert_eq!(error.message, format!("video_url: {song} is audio/mpeg; this field takes a video"));
    let error = run("video-edit", json!({"prompt": "p", "video_url": "nope.mp4"})).await.unwrap_err();
    assert!(error.message.contains("nope.mp4"), "{}", error.message);
    // Several modes, none named: the first the input fits.
    let error = f.run("generate_media", json!({"model": "seedance-2.5", "input": {"seed": 1}})).await.unwrap_err();
    assert!(error.message.starts_with("Pick a mode of Seedance 2.5: image-to-video, "), "{}", error.message);
    assert!(!got.lock().unwrap().iter().any(|g| g.method == "POST" && g.path.starts_with("/bytedance")), "nothing was submitted");
}

#[tokio::test]
async fn genjutsu_motion_transfer_uploads_a_video_and_two_images() {
    let (base, got) = higgsfield().await;
    let f = fixture(&base, "http://127.0.0.1:9");
    f.media.set_key("higgsfield", Some("id:secret".into())).unwrap();
    let clip = f.attach("dance.mp4", "video/mp4", &mp4());
    let fox = f.attach("fox.png", "image/png", &png());
    std::fs::write(f.dir.path().join("project/cat.png"), png()).unwrap();
    let input = json!({"prompt": "the fox dances", "video_url": clip, "image_urls": [fox, "cat.png"]});
    let out = f.run("generate_media", json!({"model": "genjutsu", "mode": "motion-transfer", "input": input, "background": false})).await.unwrap();
    let body = text(&out);
    assert!(body.starts_with("Generated 1 video with Higgsfield higgsfield/genjutsu/motion-transfer/v1.0.\n[generated att_"), "{body}");
    assert!(body.contains(": the-fox-dances.mp4, video/mp4, ") && body.contains(", 1280×720, 8 s"), "{body}");
    assert_eq!(out.metadata["mode"], "motion-transfer");
    let got_now = got.lock().unwrap().clone();
    let slots: Vec<String> =
        got_now.iter().filter(|g| g.path == "/files/generate-upload-url").map(|g| g.json()["content_type"].as_str().unwrap().to_owned()).collect();
    assert_eq!(slots, ["image/png", "image/png", "video/mp4"], "in field order");
    let puts: Vec<&Got> = got_now.iter().filter(|g| g.method == "PUT").collect();
    assert_eq!(puts.len(), 3);
    assert_eq!(puts[2].body, mp4(), "the video as it is");
    assert!(puts.iter().all(|p| p.header("authorization").is_none()));
    assert_eq!(
        sent(&got, "/higgsfield/genjutsu/motion-transfer/v1.0"),
        json!({"prompt": "the fox dances", "video_url": "https://cdn.test/3", "image_urls": ["https://cdn.test/1", "https://cdn.test/2"]})
    );
}

#[tokio::test]
async fn seedance_reference_to_video_takes_images_videos_and_audio() {
    let (base, got) = higgsfield().await;
    let f = fixture(&base, "http://127.0.0.1:9");
    f.media.set_key("higgsfield", Some("id:secret".into())).unwrap();
    let (fox, clip, song) =
        (f.attach("fox.png", "image/png", &png()), f.attach("run.mp4", "video/mp4", &mp4()), f.attach("song.mp3", "audio/mpeg", &mp3()));
    let input = json!({"prompt": "the fox runs to the beat", "image_urls": [fox, "https://example.com/hat.png"], "video_urls": [clip], "audio_urls": [song], "duration": 8});
    // The endpoint id names the mode; video defaults to the call when there's no job registry.
    let out =
        f.run("generate_media", json!({"model": "bytedance/seedance-2.5/reference-to-video", "input": input, "path": "out/fox.mp4"})).await.unwrap();
    assert!(text(&out).contains("Saved to out/fox.mp4"), "{}", text(&out));
    assert_eq!(std::fs::read(f.dir.path().join("project/out/fox.mp4")).unwrap(), mp4());
    let types: Vec<String> = got
        .lock()
        .unwrap()
        .iter()
        .filter(|g| g.path == "/files/generate-upload-url")
        .map(|g| g.json()["content_type"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(types, ["audio/mpeg", "image/png", "video/mp4"]);
    let body = sent(&got, "/bytedance/seedance-2.5/reference-to-video");
    assert_eq!(body["image_urls"], json!(["https://cdn.test/2", "https://example.com/hat.png"]), "links pass as they are");
    assert_eq!((body["video_urls"].clone(), body["audio_urls"].clone()), (json!(["https://cdn.test/3"]), json!(["https://cdn.test/1"])));
    assert_eq!(body["duration"], 8);
}

#[tokio::test]
async fn fal_fetches_an_endpoint_schema_on_demand_and_uploads_to_its_storage() {
    let (base, got) = fal().await;
    let f = fixture("http://127.0.0.1:9", &base);
    f.media.set_key("fal", Some("fal-key".into())).unwrap();
    let id = "fal-ai/kling-video/v3/pro/image-to-video";
    let out = text(&f.run("media_model_schema", json!({"model": id})).await.unwrap());
    let schema_line = out.lines().find(|l| l.starts_with("Input schema: ")).unwrap_or_else(|| panic!("{out}"));
    let schema: Value = serde_json::from_str(schema_line.strip_prefix("Input schema: ").unwrap()).unwrap();
    assert_eq!(schema["required"], json!(["start_image_url"]));
    assert_eq!(schema["properties"]["end_image_url"]["type"], "string", "anyOf [X, null] is X");
    assert!(!schema_line.contains("x-fal") && !schema_line.contains("$ref"), "{schema_line}");
    let fields = out.lines().find(|l| l.starts_with("Media fields")).unwrap();
    assert!(fields.contains("start_image_url (image)") && fields.contains("end_image_url? (image)"), "{fields}");
    assert!(fields.contains("elements[].frontal_image_url? (image)"), "{fields}");
    let fetched = got.lock().unwrap().iter().filter(|g| g.path.contains("expand=openapi-3.0")).count();
    assert_eq!(fetched, 1);
    // Cached: the second look needs no fetch; then a run uploads the local frame to fal storage.
    std::fs::write(f.dir.path().join("project/fox.png"), png()).unwrap();
    let input = json!({"prompt": "the fox turns", "start_image_url": "fox.png", "duration": "5"});
    let out = f.run("generate_media", json!({"model": id, "input": input, "background": false})).await.unwrap();
    assert!(text(&out).starts_with(&format!("Generated 1 video with fal {id}.")), "{}", text(&out));
    let got = got.lock().unwrap();
    assert_eq!(got.iter().filter(|g| g.path.contains("expand=openapi-3.0")).count(), 1);
    let initiate = got.iter().find(|g| g.path.starts_with("/storage/upload/initiate")).unwrap();
    assert_eq!(initiate.header("authorization"), Some("Key fal-key"));
    assert_eq!(initiate.json()["content_type"], "image/png");
    let submit = got.iter().find(|g| g.method == "POST" && g.path == format!("/{id}")).unwrap();
    assert_eq!(submit.json(), json!({"prompt": "the fox turns", "start_image_url": "https://v3.fal.media/files/in.png", "duration": "5"}));
}

#[tokio::test]
async fn results_are_read_from_any_shape() {
    let (base, _) = serve(|got, base| match got.path.as_str() {
        "/q/status" => json(200, json!({"status": "COMPLETED"})),
        "/q/result" => json(
            200,
            json!({"audio": {"url": format!("{base}/a.mp3"), "content_type": "audio/mpeg"}, "images": [{"url": format!("{base}/p.png")}], "timings": {"inference": 1.2}, "thumbnail_url": format!("{base}/t.png"), "log": {"url": format!("{base}/log.txt"), "content_type": "text/plain"}}),
        ),
        "/a.mp3" => fake::bytes("audio/mpeg", mp3()),
        "/p.png" => fake::bytes("image/png", png()),
        "/t.png" | "/log.txt" => panic!("previews and other files aren't downloaded"),
        _ => json(200, json!({"status_url": format!("{base}/q/status"), "response_url": format!("{base}/q/result"), "cancel_url": format!("{base}/q/cancel")})),
    })
    .await;
    let fal = Fal::new(&base, &base, &base).polling(Duration::from_millis(5));
    let call = otto_media::Call { http: reqwest::Client::new(), cancel: Default::default(), progress: Arc::new(|_| {}) };
    let out = fal.generate("k", "fal-ai/some/music", json!({"prompt": "lofi"}), &call).await.unwrap();
    let mimes: Vec<&str> = out.media.iter().map(|m| m.mime.as_str()).collect();
    assert_eq!(mimes, ["audio/mpeg", "image/png"]);
}

/// Saves Higgsfield's key, then answers with it (the setup card's path).
struct Connects(Arc<Media>, Mutex<Vec<String>>);

#[async_trait::async_trait]
impl otto_tools::Approver for Connects {
    async fn approve(&self, _: &str, _: &str, _: Value) -> bool {
        true
    }
    async fn ask(&self, _: &str, question: &str, _: &[String]) -> Option<String> {
        self.1.lock().unwrap().push(question.into());
        self.0.set_key("higgsfield", Some("id:secret".into())).unwrap();
        Some("Higgsfield".into())
    }
}

#[tokio::test]
async fn without_a_key_it_asks_then_uses_the_live_catalog() {
    let (base, got) = higgsfield().await;
    let f = fixture(&base, "http://127.0.0.1:9");
    // Before the key, only the built-in models: Genjutsu is unknown, Seedance has no modes.
    assert!(f.media.resolve_handle("genjutsu").is_none());
    let asker = Arc::new(Connects(f.media.clone(), Mutex::default()));
    let mut cx = f.cx();
    cx.approver = Some(asker.clone());
    let tool = f.media.tools().into_iter().find(|t| t.spec().name == "generate_media").unwrap();
    let input = json!({"model": "seedance-2.5", "mode": "video-extend", "input": {"prompt": "go on", "video_url": "https://x.test/v.mp4"}});
    let out = tool.call(input, cx).await.unwrap();
    assert_eq!(*asker.1.lock().unwrap(), ["Video generation needs a provider. Connect one to continue."]);
    assert!(text(&out).starts_with("Generated 1 video with Higgsfield bytedance/seedance-2.5/video-extend."), "{}", text(&out));
    assert_eq!(sent(&got, "/bytedance/seedance-2.5/video-extend"), json!({"prompt": "go on", "video_url": "https://x.test/v.mp4"}));
}

#[tokio::test]
async fn a_built_in_model_without_a_live_catalog_runs_through_its_quick_tool() {
    let (base, got) = higgsfield().await;
    let f = fixture(&base, "http://127.0.0.1:9");
    // A key, but the catalog can't be read: the built-in path, its schema the quick tool's.
    f.media.set_key("higgsfield", Some("id:secret".into())).unwrap();
    std::fs::create_dir_all(f.dir.path().join("state/media-catalog")).unwrap();
    std::fs::write(f.dir.path().join("state/media-catalog/higgsfield.json"), json!({"fetched_at": u64::MAX / 2, "endpoints": []}).to_string())
        .unwrap();
    let schema = text(&f.run("media_model_schema", json!({"model": "kling-3.0-pro"})).await.unwrap());
    assert!(schema.contains("Run: generate_media {model: \"kling-3.0-pro\", input}."), "{schema}");
    assert!(
        schema.contains("Media fields (project paths, att_ ids or https links): first_frame? (image), last_frame? (image), references? (image)"),
        "{schema}"
    );
    assert!(!schema.contains("\"provider\"") && !schema.contains("\"background\""), "{schema}");
    let error = f.run("generate_media", json!({"model": "kling-3.0-pro", "input": {"prompt": "x", "resolution": "8k"}})).await.unwrap_err();
    assert!(error.message.starts_with("Kling 3.0 Pro input: resolution: \"8k\" is not one of "), "{}", error.message);
    let out =
        f.run("generate_media", json!({"model": "kling-3.0-pro", "input": {"prompt": "a fox", "duration": 5}, "background": false})).await.unwrap();
    assert!(text(&out).starts_with("Generated 1 video with Higgsfield kling-video/v3.0/pro/text-to-video."), "{}", text(&out));
    assert_eq!(sent(&got, "/kling-video/v3.0/pro/text-to-video"), json!({"prompt": "a fox", "duration": 5}));
    // The quick tools take handles too, and route to their provider.
    let out = f.run("generate_video", json!({"prompt": "a fox", "model": "kling-3.0-pro", "background": false})).await.unwrap();
    assert!(text(&out).starts_with("Generated 1 video with Higgsfield kling-video/v3.0/pro/text-to-video."), "{}", text(&out));
}
