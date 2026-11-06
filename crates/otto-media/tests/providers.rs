//! Provider requests and responses against a fake server.
mod fake;

use fake::{Got, b64, json, png, serve};
use otto_media::providers::{Fal, Google, Higgsfield, OpenAi, Xai};
use otto_media::{Call, ImageRequest, MediaProvider, Reference};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn call() -> (Call, Arc<Mutex<Vec<String>>>) {
    let said: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = said.clone();
    let call = Call { http: reqwest::Client::new(), cancel: Default::default(), progress: Arc::new(move |m| log.lock().unwrap().push(m)) };
    (call, said)
}

fn request(model: &str) -> ImageRequest {
    ImageRequest {
        prompt: "a red dot".into(),
        model: model.into(),
        references: vec![],
        aspect_ratio: Some("16:9".into()),
        quality: Some("high".into()),
        transparent: true,
        n: 2,
        complexity: None,
        format: None,
        frame_count: None,
        frame_duration_ms: None,
        markup: None,
    }
}

fn only(got: &Arc<Mutex<Vec<Got>>>) -> Got {
    let got = got.lock().unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    got[0].clone()
}

#[tokio::test]
async fn openai_generations_and_edits() {
    let (base, got) =
        serve(|_, _| json(200, json!({"data": [{"b64_json": b64(&png()), "revised_prompt": "a red dot, centered"}, {"b64_json": b64(&png())}]})))
            .await;
    let openai = OpenAi::new(&base);
    let out = openai.generate_image("sk-test-key-123", request("gpt-image-2.5-flare"), &call().0).await.unwrap();
    assert_eq!((out.media.len(), out.media[0].mime.as_str(), out.revised_prompt.as_deref()), (2, "image/png", Some("a red dot, centered")));
    let sent = only(&got);
    assert_eq!((sent.method.as_str(), sent.path.as_str()), ("POST", "/v1/images/generations"));
    assert_eq!(sent.header("authorization"), Some("Bearer sk-test-key-123"));
    assert_eq!(
        sent.json(),
        json!({"model": "gpt-image-2.5-flare", "prompt": "a red dot", "n": 2, "size": "1536x1024", "quality": "high", "background": "transparent"})
    );
    got.lock().unwrap().clear();
    let mut edit = request("gpt-image-2.5-flare");
    edit.references = vec![Reference { bytes: png(), mime: "image/png".into() }; 2];
    openai.generate_image("sk-test-key-123", edit, &call().0).await.unwrap();
    let sent = only(&got);
    assert_eq!(sent.path, "/v1/images/edits");
    assert!(sent.header("content-type").unwrap().starts_with("multipart/form-data"));
    let text = sent.text();
    assert_eq!(text.matches("name=\"image[]\"").count(), 2, "{text}");
    assert!(text.contains("name=\"background\"\r\n\r\ntransparent"));
}

#[tokio::test]
async fn openai_errors_name_the_provider_and_never_the_key() {
    let (base, _) = serve(|_, _| json(401, json!({"error": {"message": "Incorrect API key provided: sk-secret-abcdef"}}))).await;
    let error = OpenAi::new(&base).generate_image("sk-secret-abcdef", request("m"), &call().0).await.unwrap_err();
    assert_eq!(error.message, "OpenAI refused the API key: Incorrect API key provided: [key]");
    let (base, _) = serve(|_, _| json(400, json!({"error": {"message": "Your request was rejected by the content policy"}}))).await;
    let error = OpenAi::new(&base).generate_image("k", request("m"), &call().0).await.unwrap_err();
    assert_eq!(error.message, "OpenAI: Your request was rejected by the content policy (HTTP 400)");
}

#[tokio::test]
async fn google_one_interaction_per_image() {
    let body = json!({"status": "completed", "steps": [
        {"type": "thought", "summary": [{"type": "image", "data": b64(&png()), "mime_type": "image/png"}]},
        {"type": "model_output", "content": [{"type": "text", "text": "Here"}, {"type": "image", "data": b64(&png()), "mime_type": "image/png"}]}
    ]});
    let (base, got) = serve(move |_, _| json(200, body.clone())).await;
    let mut r = request("gemini-nano-banana-2.1");
    r.references = vec![Reference { bytes: vec![1, 2, 3], mime: "image/jpeg".into() }];
    let out = Google::new(format!("{base}/v1beta")).generate_image("g-key", r, &call().0).await.unwrap();
    assert_eq!(out.media.len(), 2, "one model_output image each");
    assert_eq!(out.notes, ["Gemini can't make transparent backgrounds."]);
    let got = got.lock().unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!((got[0].path.as_str(), got[0].header("x-goog-api-key")), ("/v1beta/interactions", Some("g-key")));
    assert_eq!(
        got[0].json(),
        json!({"model": "gemini-nano-banana-2.1", "input": [{"type": "text", "text": "a red dot"}, {"type": "image", "data": "AQID", "mime_type": "image/jpeg"}],
            "response_format": {"type": "image", "aspect_ratio": "16:9", "image_size": "2K"}})
    );
}

#[tokio::test]
async fn google_without_an_image_says_what_the_model_said() {
    let (base, _) =
        serve(|_, _| json(200, json!({"steps": [{"type": "model_output", "content": [{"type": "text", "text": "I can't draw that."}]}]}))).await;
    let mut r = request("m");
    r.n = 1;
    let error = Google::new(&base).generate_image("k", r, &call().0).await.unwrap_err();
    assert_eq!(error.message, "Gemini returned no image: I can't draw that.");
}

#[tokio::test]
async fn xai_generations_and_edits() {
    let (base, got) = serve(|_, _| json(200, json!({"data": [{"b64_json": b64(&png())}]}))).await;
    let xai = Xai::new(&base);
    xai.generate_image("x-key", request("grok-imagine-image-2.0"), &call().0).await.unwrap();
    let sent = only(&got);
    assert_eq!((sent.path.as_str(), sent.header("authorization")), ("/v1/images/generations", Some("Bearer x-key")));
    assert_eq!(
        sent.json(),
        json!({"model": "grok-imagine-image-2.0", "prompt": "a red dot", "n": 2, "response_format": "b64_json", "aspect_ratio": "16:9", "quality": "medium", "resolution": "2k"})
    );
    got.lock().unwrap().clear();
    let mut edit = request("grok-imagine-image-2.0");
    edit.references = vec![Reference { bytes: vec![1, 2, 3], mime: "image/png".into() }];
    xai.generate_image("x-key", edit, &call().0).await.unwrap();
    let sent = only(&got);
    assert_eq!(sent.path, "/v1/images/edits");
    assert_eq!(sent.json()["image"], json!({"url": "data:image/png;base64,AQID", "type": "image_url"}));
}

#[tokio::test]
async fn fal_runs_then_downloads_and_edits_with_the_edit_endpoint() {
    let (base, got) = serve(|got, base| {
        if got.path == "/files/out.png" {
            fake::bytes("image/png", png())
        } else {
            json(200, json!({"images": [{"url": format!("{base}/files/out.png"), "width": 2, "height": 1, "content_type": "image/png"}]}))
        }
    })
    .await;
    let fal = Fal::new(&base, &base, &base);
    let mut r = request("blackforestlabs/flux-3/text-to-image");
    r.references = vec![Reference { bytes: vec![1, 2, 3], mime: "image/png".into() }];
    let out = fal.generate_image("fal-key", r, &call().0).await.unwrap();
    assert_eq!((out.media.len(), out.model.as_str()), (2, "blackforestlabs/flux-3/edit-image"));
    let got = got.lock().unwrap();
    let runs: Vec<&Got> = got.iter().filter(|g| g.method == "POST").collect();
    assert_eq!(runs.len(), 2);
    assert_eq!((runs[0].path.as_str(), runs[0].header("authorization")), ("/blackforestlabs/flux-3/edit-image", Some("Key fal-key")));
    assert_eq!(
        runs[0].json(),
        json!({"prompt": "a red dot", "output_format": "png", "aspect_ratio": "16:9", "image_urls": ["data:image/png;base64,AQID"]})
    );
    assert!(got.iter().filter(|g| g.method == "GET").all(|g| g.header("authorization").is_none()), "downloads carry no key");
}

#[tokio::test]
async fn higgsfield_submits_polls_and_downloads() {
    let polls = Arc::new(Mutex::new(0));
    let seen = polls.clone();
    let (base, got) = serve(move |got, base| match got.path.as_str() {
        "/higgsfield-ai/soul/v2/standard" => json(200, json!({"status": "queued", "request_id": "r1", "status_url": format!("{base}/requests/r1/status"), "cancel_url": format!("{base}/requests/r1/cancel")})),
        "/requests/r1/status" => {
            let mut n = seen.lock().unwrap();
            *n += 1;
            if *n < 2 { json(200, json!({"status": "in_progress", "request_id": "r1"})) } else { json(200, json!({"status": "completed", "images": [{"url": format!("{base}/out.png")}]})) }
        }
        _ => fake::bytes("image/png", png()),
    })
    .await;
    let higgsfield = Higgsfield::new(&base, Duration::from_millis(10));
    let mut r = request("higgsfield-ai/soul/v2/standard");
    r.n = 1;
    let (call, said) = call();
    let out = higgsfield.generate_image("id:secret", r, &call).await.unwrap();
    assert_eq!(out.media.len(), 1);
    let got = got.lock().unwrap();
    assert_eq!(got[0].header("authorization"), Some("Key id:secret"));
    assert!(got[0].header("idempotency-key").is_some());
    assert_eq!(got[0].json(), json!({"prompt": "a red dot", "aspect_ratio": "16:9", "resolution": "1080p"}));
    assert_eq!(got.iter().filter(|g| g.path == "/requests/r1/status").count(), 2);
    let said = said.lock().unwrap();
    assert!(said[0].starts_with("Generating with Higgsfield… queued, "), "{said:?}");
    assert!(said[1].starts_with("Generating with Higgsfield… in progress, "), "{said:?}");
}

#[tokio::test]
async fn higgsfield_uploads_references_and_withdraws_on_cancel() {
    let (base, got) = serve(|got, base| match got.path.as_str() {
        "/files/generate-upload-url" => json(200, json!({"public_url": "https://cdn.example/ref.png", "upload_url": format!("{base}/upload"), "upload_headers": {"Content-Type": "image/png", "x-amz-tagging": "retention=temporary"}})),
        "/upload" => fake::bytes("text/plain", vec![]),
        "/xai/grok-imagine-image-2.0" => json(200, json!({"status": "queued", "request_id": "r2", "status_url": format!("{base}/requests/r2/status"), "cancel_url": format!("{base}/requests/r2/cancel")})),
        "/requests/r2/cancel" => fake::bytes("text/plain", vec![]),
        _ => json(200, json!({"status": "queued", "request_id": "r2"})),
    })
    .await;
    let higgsfield = Higgsfield::new(&base, Duration::from_millis(20));
    let mut r = request("higgsfield-ai/soul/v2/standard");
    (r.n, r.quality) = (1, Some("low".into()));
    r.references = vec![Reference { bytes: vec![1, 2, 3], mime: "image/png".into() }];
    let (call, _) = call();
    let cancel = call.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
    });
    let error = higgsfield.generate_image("id:secret", r, &call).await.unwrap_err();
    assert_eq!(error.kind, otto_media::MediaErrorKind::Cancelled);
    let got = got.lock().unwrap();
    let upload = got.iter().find(|g| g.path == "/upload").unwrap();
    assert_eq!(
        (upload.method.as_str(), upload.body.as_slice(), upload.header("x-amz-tagging")),
        ("PUT", &[1u8, 2, 3][..], Some("retention=temporary"))
    );
    assert!(upload.header("authorization").is_none(), "no credentials to storage");
    let submit = got.iter().find(|g| g.path == "/xai/grok-imagine-image-2.0").expect("references go to Grok Image 2.0");
    assert_eq!(
        submit.json(),
        json!({"prompt": "a red dot", "aspect_ratio": "16:9", "quality": "low", "image_urls": ["https://cdn.example/ref.png"]})
    );
    assert!(got.iter().any(|g| g.path == "/requests/r2/cancel" && g.method == "POST"));
}

#[tokio::test]
async fn higgsfield_failures_end_the_wait() {
    for (status, message) in [("failed", "Higgsfield: Generation failed"), ("nsfw", "Higgsfield: rejected by content moderation")] {
        let (base, _) = serve(move |got, _| match got.path.as_str() {
            "/z-image/turbo" => json(200, json!({"status": "queued", "request_id": "r"})),
            _ => json(200, json!({"status": status, "error": "Generation failed"})),
        })
        .await;
        let mut r = request("z-image/turbo");
        r.n = 1;
        let error = Higgsfield::new(&base, Duration::from_millis(5)).generate_image("id:secret", r, &call().0).await.unwrap_err();
        assert_eq!(error.message, message);
    }
}
