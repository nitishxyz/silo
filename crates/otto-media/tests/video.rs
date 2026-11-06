//! Video providers against a fake server: submit, poll, download; failures and cancels.
mod fake;

use fake::{Got, b64, json, mp4, png, serve};
use otto_media::providers::{Fal, Google, Higgsfield, Xai};
use otto_media::{Call, MediaErrorKind, MediaProvider, Reference, VideoRequest};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn call() -> (Call, Arc<Mutex<Vec<String>>>) {
    let said: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = said.clone();
    let call = Call { http: reqwest::Client::new(), cancel: Default::default(), progress: Arc::new(move |m| log.lock().unwrap().push(m)) };
    (call, said)
}

fn request(model: &str) -> VideoRequest {
    VideoRequest {
        prompt: "a fox running".into(),
        model: model.into(),
        first_frame: None,
        last_frame: None,
        references: vec![],
        duration: Some(20.0),
        aspect_ratio: Some("16:9".into()),
        resolution: Some("720p".into()),
        sound: Some(false),
    }
}

fn frame() -> Option<Reference> {
    Some(Reference { bytes: vec![1, 2, 3], mime: "image/png".into() })
}

/// Counts calls to a path: the `n`th status answer.
fn counter() -> impl Fn() -> usize {
    let n = Arc::new(Mutex::new(0));
    move || {
        let mut n = n.lock().unwrap();
        *n += 1;
        *n
    }
}

/// Cancels `call` after `ms`.
fn cancel_after(call: &Call, ms: u64) {
    let cancel = call.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        cancel.cancel();
    });
}

fn find(got: &Arc<Mutex<Vec<Got>>>, method: &str, path: &str) -> Option<Got> {
    got.lock().unwrap().iter().find(|g| g.method == method && g.path == path).cloned()
}

#[tokio::test]
async fn xai_submits_polls_with_progress_and_downloads() {
    let next = counter();
    let (base, got) = serve(move |got, base| match got.path.as_str() {
        "/v1/videos/generations" => json(200, json!({"request_id": "v1"})),
        "/v1/videos/v1" if next() < 2 => json(200, json!({"status": "pending", "progress": 40})),
        "/v1/videos/v1" => json(200, json!({"status": "done", "progress": 100, "model": "grok-imagine-video-1.5", "video": {"url": format!("{base}/out.mp4"), "duration": 15, "respect_moderation": true}})),
        _ => fake::bytes("application/octet-stream", mp4()),
    })
    .await;
    let mut r = request("grok-imagine-video-1.5");
    (r.first_frame, r.last_frame, r.references) = (frame(), frame(), vec![frame().unwrap()]);
    let (call, said) = call();
    let out = Xai::new(&base).polling(Duration::from_millis(5)).generate_video("x-key", r, &call).await.unwrap();
    assert_eq!((out.media.len(), out.media[0].mime.as_str()), (1, "video/mp4"), "the mime from the bytes");
    assert_eq!(out.notes, ["grok-imagine-video-1.5 makes 1–15 s videos in whole seconds: made 15 s."]);
    let sent = find(&got, "POST", "/v1/videos/generations").unwrap();
    assert_eq!(sent.header("authorization"), Some("Bearer x-key"));
    let frame = json!({"url": "data:image/png;base64,AQID"});
    assert_eq!(
        sent.json(),
        json!({"model": "grok-imagine-video-1.5", "prompt": "a fox running", "duration": 15, "aspect_ratio": "16:9", "resolution": "720p",
            "generate_audio": false, "image": frame, "last_frame": frame, "reference_images": [frame]})
    );
    assert_eq!(find(&got, "GET", "/v1/videos/v1").unwrap().header("authorization"), Some("Bearer x-key"));
    assert!(find(&got, "GET", "/out.mp4").unwrap().header("authorization").is_none(), "downloads carry no key");
    let said = said.lock().unwrap();
    assert!(said[0].starts_with("Generating with xAI… 0 s"), "{said:?}");
    assert!(said[1].starts_with("Generating with xAI… 40 %, "), "{said:?}");
}

#[tokio::test]
async fn xai_failures_and_cancels_end_the_wait() {
    let (base, _) = serve(|got, _| match got.path.as_str() {
        "/v1/videos/generations" => json(200, json!({"request_id": "v2"})),
        _ => json(200, json!({"status": "failed", "error": {"code": "invalid_argument", "message": "Prompt cannot be empty."}})),
    })
    .await;
    let xai = Xai::new(&base).polling(Duration::from_millis(5));
    let error = xai.generate_video("k", request("grok-imagine-video-1.5"), &call().0).await.unwrap_err();
    assert_eq!(error.message, "xAI: Prompt cannot be empty.");
    let (base, _) = serve(|got, _| match got.path.as_str() {
        "/v1/videos/generations" => json(200, json!({"request_id": "v3"})),
        _ => json(200, json!({"status": "pending"})),
    })
    .await;
    let (call, _) = call();
    cancel_after(&call, 80);
    let error = Xai::new(&base).polling(Duration::from_millis(20)).generate_video("k", request("m"), &call).await.unwrap_err();
    assert_eq!(error.kind, MediaErrorKind::Cancelled);
}

#[tokio::test]
async fn google_runs_a_background_interaction_and_polls_it() {
    let next = counter();
    let (base, got) = serve(move |got, _| match (got.method.as_str(), got.path.as_str()) {
        ("POST", "/v1beta/interactions") => json(200, json!({"id": "i1", "status": "in_progress"})),
        ("GET", "/v1beta/interactions/i1") if next() < 2 => json(200, json!({"id": "i1", "status": "in_progress"})),
        _ => json(
            200,
            json!({"id": "i1", "status": "completed", "steps": [
            {"type": "thought", "content": [{"type": "thought", "text": "…"}]},
            {"type": "model_output", "content": [{"type": "video", "mime_type": "video/mp4", "data": b64(&mp4())}]}]}),
        ),
    })
    .await;
    let mut r = request("gemini-omni-1.1-flash");
    (r.first_frame, r.aspect_ratio, r.resolution) = (frame(), Some("1:1".into()), Some("1080p".into()));
    let out = Google::new(format!("{base}/v1beta")).polling(Duration::from_millis(5)).generate_video("g-key", r, &call().0).await.unwrap();
    assert_eq!((out.media.len(), out.media[0].mime.as_str(), out.media[0].bytes.len()), (1, "video/mp4", mp4().len()));
    assert_eq!(
        out.notes,
        [
            "Gemini takes the length from the prompt; it may differ.",
            "Gemini makes 16:9 or 9:16 videos, not 1:1: made 16:9.",
            "Gemini always adds sound; ask for silence in the prompt."
        ]
    );
    let sent = find(&got, "POST", "/v1beta/interactions").unwrap();
    assert_eq!(sent.header("x-goog-api-key"), Some("g-key"));
    assert_eq!(
        sent.json(),
        json!({"model": "gemini-omni-1.1-flash", "background": true,
            "input": [{"type": "image", "data": "AQID", "mime_type": "image/png"}, {"type": "text", "text": "a fox running\n\nThe video is 20 seconds long."}],
            "response_format": {"type": "video", "resolution": "1080p"}, "generation_config": {"video_config": {"task": "image_to_video"}}})
    );
    assert_eq!(got.lock().unwrap().iter().filter(|g| g.path == "/v1beta/interactions/i1").count(), 2);
}

#[tokio::test]
async fn google_failures_and_cancels() {
    let (base, _) = serve(|got, _| match got.method.as_str() {
        "POST" => json(200, json!({"id": "i2", "status": "in_progress"})),
        _ => json(200, json!({"id": "i2", "status": "failed", "error": {"message": "The prompt was blocked."}})),
    })
    .await;
    let google = Google::new(&base).polling(Duration::from_millis(5));
    let error = google.generate_video("k", request("gemini-omni-1.1-flash"), &call().0).await.unwrap_err();
    assert_eq!(error.message, "Gemini: The prompt was blocked.");
    let (base, got) = serve(|got, _| match got.path.as_str() {
        "/interactions/i3/cancel" => json(200, json!({})),
        _ => json(200, json!({"id": "i3", "status": "in_progress"})),
    })
    .await;
    let (call, _) = call();
    cancel_after(&call, 80);
    let error = Google::new(&base).polling(Duration::from_millis(20)).generate_video("k", request("m"), &call).await.unwrap_err();
    assert_eq!(error.kind, MediaErrorKind::Cancelled);
    assert!(find(&got, "POST", "/interactions/i3/cancel").is_some(), "a cancel cancels the interaction");
}

#[tokio::test]
async fn fal_queues_polls_then_reads_the_response() {
    let next = counter();
    let (base, got) = serve(move |got, base| match (got.method.as_str(), got.path.as_str()) {
        ("POST", "/kling/v4/image-to-video") => json(
            200,
            json!({"request_id": "q1", "queue_position": 2,
            "status_url": format!("{base}/kling/v4/requests/q1/status"), "response_url": format!("{base}/kling/v4/requests/q1"),
            "cancel_url": format!("{base}/kling/v4/requests/q1/cancel")}),
        ),
        ("GET", "/kling/v4/requests/q1/status") if next() < 2 => json(200, json!({"status": "IN_PROGRESS", "request_id": "q1"})),
        ("GET", "/kling/v4/requests/q1/status") => json(200, json!({"status": "COMPLETED", "request_id": "q1"})),
        ("GET", "/kling/v4/requests/q1") => json(200, json!({"video": {"url": format!("{base}/files/out.mp4"), "content_type": "video/mp4"}})),
        _ => fake::bytes("video/mp4", mp4()),
    })
    .await;
    let mut r = request("kling/v4/text-to-video");
    (r.first_frame, r.duration, r.resolution) = (frame(), Some(8.0), None);
    let (call, said) = call();
    let out = Fal::new(&base, &base, &base).polling(Duration::from_millis(5)).generate_video("fal-key", r, &call).await.unwrap();
    assert_eq!((out.model.as_str(), out.media[0].mime.as_str()), ("kling/v4/image-to-video", "video/mp4"));
    assert_eq!(out.notes, ["Kling 4.0 on fal sets its own sound."]);
    let sent = find(&got, "POST", "/kling/v4/image-to-video").unwrap();
    assert_eq!(sent.header("authorization"), Some("Key fal-key"));
    assert_eq!(
        sent.json(),
        json!({"prompt": "a fox running", "duration": 8, "aspect_ratio": "16:9", "start_image_url": "data:image/png;base64,AQID"})
    );
    assert_eq!(find(&got, "GET", "/kling/v4/requests/q1").unwrap().header("authorization"), Some("Key fal-key"));
    let said = said.lock().unwrap();
    assert!(said[0].starts_with("Generating with fal… queued, 2 ahead, "), "{said:?}");
    assert!(said[1].starts_with("Generating with fal… in progress, "), "{said:?}");
}

#[tokio::test]
async fn fal_failures_and_cancels() {
    let submitted = |base: &str| {
        json(
            200,
            json!({"request_id": "q", "status_url": format!("{base}/m/requests/q/status"), "response_url": format!("{base}/m/requests/q"),
            "cancel_url": format!("{base}/m/requests/q/cancel")}),
        )
    };
    let (base, _) = serve(move |got, base| match got.path.as_str() {
        "/m/text-to-video" => submitted(base),
        _ => json(200, json!({"status": "COMPLETED", "error": "Out of memory", "error_type": "runner_error"})),
    })
    .await;
    let fal = Fal::new(&base, &base, &base).polling(Duration::from_millis(5));
    let error = fal.generate_video("k", request("m/text-to-video"), &call().0).await.unwrap_err();
    assert_eq!(error.message, "fal: Out of memory");
    let (base, got) = serve(move |got, base| match got.path.as_str() {
        "/m/text-to-video" => submitted(base),
        "/m/requests/q/cancel" => json(200, json!({"status": "OK"})),
        _ => json(200, json!({"status": "IN_QUEUE", "queue_position": 0})),
    })
    .await;
    let (call, _) = call();
    cancel_after(&call, 80);
    let error =
        Fal::new(&base, &base, &base).polling(Duration::from_millis(20)).generate_video("k", request("m/text-to-video"), &call).await.unwrap_err();
    assert_eq!(error.kind, MediaErrorKind::Cancelled);
    assert!(find(&got, "PUT", "/m/requests/q/cancel").is_some(), "a cancel withdraws the queued request");
}

#[tokio::test]
async fn higgsfield_kling_with_frames_uploads_them_and_maps_settings() {
    let next = counter();
    let (base, got) = serve(move |got, base| match got.path.as_str() {
        "/files/generate-upload-url" => {
            json(200, json!({"public_url": format!("https://cdn.example/f{}.png", next()), "upload_url": format!("{base}/upload")}))
        }
        "/upload" => fake::bytes("text/plain", vec![]),
        "/kling-video/v3.0/pro/image-to-video" => {
            json(200, json!({"status": "queued", "request_id": "h1", "status_url": format!("{base}/requests/h1/status")}))
        }
        "/requests/h1/status" => json(200, json!({"status": "completed", "video": {"url": format!("{base}/out.mp4")}})),
        _ => fake::bytes("video/mp4", mp4()),
    })
    .await;
    let mut r = request("kling-video/v3.0/pro/text-to-video");
    (r.first_frame, r.last_frame, r.references) = (frame(), frame(), vec![Reference { bytes: png(), mime: "image/png".into() }]);
    let out = Higgsfield::new(&base, Duration::from_millis(5)).generate_video("id:secret", r, &call().0).await.unwrap();
    assert_eq!(out.model, "kling-video/v3.0/pro/image-to-video");
    assert_eq!(
        out.notes,
        [
            "Kling 3.0 makes 3–15 s videos in whole seconds: made 15 s.",
            "Kling 3.0 follows the first frame's shape.",
            "Kling 3.0 picks its own resolution.",
            "kling-video/v3.0/pro/image-to-video takes no reference images; they weren't sent."
        ]
    );
    let sent = find(&got, "POST", "/kling-video/v3.0/pro/image-to-video").unwrap();
    assert_eq!(
        sent.json(),
        json!({"prompt": "a fox running", "duration": 15, "sound": "off", "image_url": "https://cdn.example/f1.png", "last_image_url": "https://cdn.example/f2.png"})
    );
    assert_eq!(got.lock().unwrap().iter().filter(|g| g.path == "/upload").count(), 2);
}

#[tokio::test]
async fn higgsfield_video_failures_and_cancels() {
    let (base, _) = serve(|got, _| match got.path.as_str() {
        "/bytedance/seedance-2.5/text-to-video" => json(200, json!({"status": "queued", "request_id": "h2"})),
        _ => json(200, json!({"status": "failed", "error": "Generation failed"})),
    })
    .await;
    let higgsfield = Higgsfield::new(&base, Duration::from_millis(5));
    let error = higgsfield.generate_video("id:secret", request("bytedance/seedance-2.5/text-to-video"), &call().0).await.unwrap_err();
    assert_eq!(error.message, "Higgsfield: Generation failed");
    let (base, got) = serve(|got, _| match got.path.as_str() {
        "/minimax/h3/text-to-video" => json(200, json!({"status": "queued", "request_id": "h3"})),
        "/requests/h3/cancel" => json(200, json!({})),
        _ => json(200, json!({"status": "in_progress"})),
    })
    .await;
    let (call, _) = call();
    cancel_after(&call, 80);
    let error =
        Higgsfield::new(&base, Duration::from_millis(20)).generate_video("id:secret", request("minimax/h3/text-to-video"), &call).await.unwrap_err();
    assert_eq!(error.kind, MediaErrorKind::Cancelled);
    assert!(find(&got, "POST", "/requests/h3/cancel").is_some());
}
