use otto_tools::{
    OutputPart, Tool, ToolCx, ToolErrorKind, attachments,
    builtin::{copy_attachment::CopyAttachment, read::Read},
    images,
    policy::*,
};
use serde_json::json;
use std::path::Path;

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn noise(w: u32, h: u32) -> Vec<u8> {
    let mut state = 0x2545_f491_u32;
    let img = image::RgbImage::from_fn(w, h, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let [a, b, c, _] = state.to_le_bytes();
        image::Rgb([a, b, c])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// A project folder and an attachments root holding `session`'s files.
fn cx(project: &Path, root: &Path) -> ToolCx {
    let mut cx = ToolCx::new("call", project);
    cx.attachments = Some(root.to_path_buf());
    cx.sessions = vec!["child".into(), "s1".into()];
    cx
}

#[test]
fn attachments_are_stored_with_ids_and_noted() {
    let d = tempfile::tempdir().unwrap();
    let dir = attachments::dir(d.path(), "s1");
    let a = attachments::put(&dir, "../shots/screenshot.png", "image/png", &vec![7; 812 * 1024]).unwrap();
    assert!(attachments::is_id(&a.id), "{}", a.id);
    assert_eq!(a.name, "screenshot.png");
    assert_eq!(a.note(), format!("[attachment {}: screenshot.png, image/png, 812 KB — copy_attachment can save it into the project]", a.id));
    let (got, path) = attachments::get(&dir, &a.id).unwrap();
    assert_eq!(got, a);
    assert_eq!(path, dir.join(format!("{}-screenshot.png", a.id)));
    assert_eq!(std::fs::read(&path).unwrap().len(), 812 * 1024);
    // Chunks append; the total is capped.
    let b = attachments::put(&dir, "notes.txt", "", b"one ").unwrap();
    assert_eq!(b.mime, "application/octet-stream");
    assert_eq!(attachments::append(&dir, &b.id, b"two", 100).unwrap().size, 7);
    assert_eq!(std::fs::read(attachments::get(&dir, &b.id).unwrap().1).unwrap(), b"one two");
    assert!(attachments::append(&dir, &b.id, &[0; 100], 100).is_err());
    assert_eq!(attachments::list(&dir).iter().map(|a| a.id.clone()).collect::<Vec<_>>(), [a.id.clone(), b.id.clone()]);
    // Ids are checked before touching the disk; ancestors' attachments are found.
    assert!(attachments::get(&dir, "att_../../x").is_err());
    assert!(attachments::get(&dir, "att_00000000").is_err());
    assert_eq!(attachments::find(d.path(), &["child".into(), "s1".into()], &a.id).unwrap().0, a);
    assert!(attachments::find(d.path(), &["other".into()], &a.id).is_err());
    assert_eq!(attachments::size(512), "512 B");
    assert_eq!(attachments::size(3 * 1024 * 1024 + 400 * 1024), "3.4 MB");
    assert_eq!(attachments::clean_name(""), "file");
    assert_eq!(attachments::clean_name("a/.."), "file");
}

#[tokio::test]
async fn copy_attachment_copies_originals_and_refuses_to_overwrite() {
    let project = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let original = noise(64, 48);
    let a = attachments::put(&attachments::dir(root.path(), "s1"), "shot.png", "image/png", &original).unwrap();
    let tool = CopyAttachment::default();
    assert!(tool.spec().effects.write);
    // Loaded on demand through load_tools; read stays eager.
    assert_eq!(tool.spec().loading, otto_tools::Loading::Deferred);
    assert_eq!(Read::default().spec().loading, otto_tools::Loading::Eager);
    let out = tool.call(json!({"attachment_id":a.id,"path":"assets/img/shot.png"}), cx(project.path(), root.path())).await.unwrap();
    let dest = project.path().join("assets/img/shot.png");
    assert_eq!(std::fs::read(&dest).unwrap(), original);
    assert_eq!(out.metadata["bytes"], original.len());
    assert_eq!(out.metadata["name"], "shot.png");
    assert!(out.joined_text().starts_with("Copied shot.png to "));
    // An existing file stays unless overwrite is set.
    std::fs::write(&dest, b"mine").unwrap();
    let refused = tool.call(json!({"attachment_id":a.id,"path":"assets/img/shot.png"}), cx(project.path(), root.path())).await.unwrap_err();
    assert_eq!(refused.kind, ToolErrorKind::InvalidInput);
    assert_eq!(std::fs::read(&dest).unwrap(), b"mine");
    tool.call(json!({"attachment_id":a.id,"path":"assets/img/shot.png","overwrite":true}), cx(project.path(), root.path())).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), original);
    let unknown = tool.call(json!({"attachment_id":"att_0badf00d","path":"x.png"}), cx(project.path(), root.path())).await.unwrap_err();
    assert_eq!(unknown.kind, ToolErrorKind::NotFound);
}

#[test]
fn copy_attachment_goes_through_the_path_policy() {
    let project = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let cx = cx(project.path(), root.path());
    let tool = CopyAttachment::default();
    let ask = Policy { mode: ApprovalMode::AskOnEffects, rules: vec![] };
    let prepared = ask.prepare(tool.spec(), &json!({"attachment_id":"att_00000000","path":"assets/a.png"}), &cx).unwrap();
    assert!(matches!(prepared.decision, Decision::Ask { .. }));
    assert_eq!(prepared.resolved[0].location, Location::Project);
    assert!(prepared.input["path"].as_str().unwrap().ends_with("/assets/a.png"));
    let outside =
        Policy { mode: ApprovalMode::Auto, rules: vec![] }.prepare(tool.spec(), &json!({"attachment_id":"x","path":"../a.png"}), &cx).unwrap();
    assert_eq!(outside.resolved[0].location, Location::Outside);
    let deny = Policy {
        mode: ApprovalMode::Auto,
        rules: vec![Rule { tool: "copy_attachment".into(), pattern: Some("**/secret/**".into()), decision: RuleDecision::Deny }],
    };
    assert!(matches!(deny.decide(tool.spec(), &json!({"attachment_id":"x","path":"secret/a.png"}), &cx), Decision::Deny { .. }));
    let plan = Policy { mode: ApprovalMode::Plan, rules: vec![] };
    assert!(matches!(plan.decide(tool.spec(), &json!({"attachment_id":"x","path":"a.png"}), &cx), Decision::Deny { .. }));
}

#[test]
fn images_downscale_under_the_byte_cap_and_never_upscale() {
    let big = images::prepare(&png(3024, 1964), images::MAX_EDGE, images::QUALITY).unwrap();
    assert_eq!((big.original, big.sent), ((3024, 1964), (1568, 1018)));
    assert_eq!(big.dimensions(), "3024×1964 → 1568×1018");
    assert!(big.bytes.len() <= images::MAX_BYTES);
    let decoded = image::load_from_memory(&big.bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1568, 1018));
    // Small files pass unchanged, whatever max_edge asks.
    let small = png(100, 50);
    let same = images::prepare(&small, 4000, images::QUALITY).unwrap();
    assert_eq!((same.sent, same.bytes == small, same.mime), ((100, 50), true, "image/png"));
    assert_eq!(same.dimensions(), "100×50");
    // Noise compresses badly: recompressed (JPEG) to fit the cap.
    let noisy = images::prepare(&noise(2400, 2400), images::MAX_EDGE, images::QUALITY).unwrap();
    assert!(noisy.bytes.len() <= images::MAX_BYTES, "{}", noisy.bytes.len());
    assert!(noisy.sent.0 <= images::MAX_EDGE && noisy.sent.0 == noisy.sent.1);
    assert!(images::prepare(b"\x89PNG\r\n\x1a\nnot really", images::MAX_EDGE, images::QUALITY).is_err());
}

#[tokio::test]
async fn read_sends_images_downscaled_pdfs_as_documents_and_attachments() {
    let project = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("shot.png"), png(3024, 1964)).unwrap();
    let out = Read::default().call(json!({"path":"shot.png"}), cx(project.path(), root.path())).await.unwrap();
    assert_eq!(out.joined_text(), "shot.png 3024×1964 → 1568×1018");
    assert!(matches!(&out.content[1], OutputPart::Image { .. }));
    assert_eq!(out.metadata["sent"], json!([1568, 1018]));
    let out = Read::default().call(json!({"path":"shot.png","max_edge":500}), cx(project.path(), root.path())).await.unwrap();
    assert_eq!(out.metadata["sent"], json!([500, 325]));
    // Absolute paths work (screenshots live outside the project).
    let outside = tempfile::tempdir().unwrap();
    let small = outside.path().join("small.png");
    std::fs::write(&small, png(40, 30)).unwrap();
    let out = Read::default().call(json!({"path":small,"max_edge":4000}), cx(project.path(), root.path())).await.unwrap();
    assert_eq!(out.joined_text(), "small.png 40×30");
    std::fs::write(project.path().join("doc.pdf"), b"%PDF-1.4\n%fake\n").unwrap();
    let out = Read::default().call(json!({"path":"doc.pdf"}), cx(project.path(), root.path())).await.unwrap();
    assert!(matches!(&out.content[1], OutputPart::Document { mime, name, .. } if mime == "application/pdf" && name.as_deref() == Some("doc.pdf")));
    let a = attachments::put(&attachments::dir(root.path(), "s1"), "notes.txt", "text/plain", b"hello\nworld\n").unwrap();
    let out = Read::default().call(json!({"attachment_id":a.id}), cx(project.path(), root.path())).await.unwrap();
    assert_eq!(out.joined_text(), "1: hello\n2: world\n");
    assert_eq!(out.metadata["name"], "notes.txt", "an attachment read is named for the transcript");
    let shot = attachments::put(&attachments::dir(root.path(), "s1"), "screenshot.png", "image/png", &png(40, 30)).unwrap();
    let out = Read::default().call(json!({"attachment_id":shot.id}), cx(project.path(), root.path())).await.unwrap();
    assert_eq!((out.metadata["name"].as_str(), out.metadata["sent"].clone()), (Some("screenshot.png"), json!([40, 30])));
    // A model without vision gets a description, not the image.
    let mut blind = cx(project.path(), root.path());
    blind.vision = false;
    let out = Read::default().call(json!({"attachment_id":shot.id}), blind).await.unwrap();
    assert_eq!(out.content.len(), 1);
    let size = attachments::size(png(40, 30).len() as u64);
    assert_eq!(out.joined_text(), format!("screenshot.png: 40×30 PNG, {size}; this model can't view images"));
    assert_eq!(out.metadata["name"], "screenshot.png");
    // The policy leaves an attachment read without a path.
    let prepared = Policy::default().prepare(Read::default().spec(), &json!({"attachment_id":a.id}), &cx(project.path(), root.path())).unwrap();
    assert!(prepared.input.get("path").is_none());
}
