//! What a video, audio or vector file is: its type from its bytes, an MP4's size and length (`tkhd`, `mvhd`), an
//! MP3's length (its frames, or a Xing/Info header), an SVG's size (`width`/`height` or `viewBox`).

/// `video/mp4`, `video/quicktime`, `audio/mpeg` or `audio/wav` from the first bytes.
pub(crate) fn sniff(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [_, _, _, _, b'f', b't', b'y', b'p', b'q', b't', b' ', b' ', ..] => Some("video/quicktime"),
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => Some("video/mp4"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'A', b'V', b'E', ..] => Some("audio/wav"),
        [b'I', b'D', b'3', ..] => Some("audio/mpeg"),
        _ if frame(bytes).is_some() => Some("audio/mpeg"),
        _ => None,
    }
}

/// An MP4's video size and length in seconds, each `None` when unreadable.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Mp4 {
    pub size: Option<(u32, u32)>,
    pub seconds: Option<f64>,
}

pub(crate) fn mp4(bytes: &[u8]) -> Mp4 {
    let mut info = Mp4::default();
    walk(bytes, &mut info);
    info
}

/// The boxes in `bytes`, into `moov`, `trak` and their kin; `mvhd` gives the length, the first `tkhd` with a size
/// (a video track: audio tracks have none) the size.
fn walk(mut bytes: &[u8], info: &mut Mp4) {
    while bytes.len() >= 8 {
        let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64;
        let kind = &bytes[4..8];
        let (head, size) = match size {
            0 => (8, bytes.len() as u64),
            1 if bytes.len() >= 16 => (16, u64::from_be_bytes(bytes[8..16].try_into().unwrap_or_default())),
            1 => return,
            n => (8, n),
        };
        let Ok(size) = usize::try_from(size) else { return };
        if size < head || size > bytes.len() {
            // A box running past the end (a cut file): read what is there of a container.
            if matches!(kind, b"moov" | b"trak") {
                walk(&bytes[head..], info);
            }
            return;
        }
        let body = &bytes[head..size];
        match kind {
            b"moov" | b"trak" | b"mdia" | b"minf" | b"edts" => walk(body, info),
            b"mvhd" => info.seconds = info.seconds.or_else(|| mvhd(body)),
            b"tkhd" => info.size = info.size.or_else(|| tkhd(body)),
            _ => {}
        }
        bytes = &bytes[size..];
    }
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// `duration / timescale`; version 1 has 64-bit times.
#[allow(clippy::cast_precision_loss)]
fn mvhd(b: &[u8]) -> Option<f64> {
    let (scale, duration) = match b.first()? {
        0 => (be32(b, 12)?, u64::from(be32(b, 16)?)),
        1 => (be32(b, 20)?, be64(b, 24)?),
        _ => return None,
    };
    (scale > 0 && duration > 0 && duration != u64::from(u32::MAX) && duration != u64::MAX).then(|| duration as f64 / f64::from(scale))
}

/// Width and height (16.16 fixed point) after the times, layer, volume and matrix.
fn tkhd(b: &[u8]) -> Option<(u32, u32)> {
    let at = match b.first()? {
        0 => 4 + 72,
        1 => 4 + 84,
        _ => return None,
    };
    let (w, h) = (be32(b, at)? >> 16, be32(b, at + 4)? >> 16);
    (w > 0 && h > 0).then_some((w, h))
}

/// One MPEG audio Layer III frame header: its length in bytes, samples, sample rate and side-info size.
#[derive(Clone, Copy, Debug)]
struct Frame {
    len: usize,
    samples: u32,
    rate: u32,
    side: usize,
}

fn frame(b: &[u8]) -> Option<Frame> {
    let [0xFF, b1, b2, b3, ..] = *b else { return None };
    if b1 & 0xE0 != 0xE0 || (b1 >> 1) & 3 != 1 {
        return None; // no sync, or not Layer III
    }
    let version = (b1 >> 3) & 3; // 3: MPEG 1, 2: MPEG 2, 0: MPEG 2.5
    const V1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let kbps = *if version == 3 { &V1 } else { &V2 }.get(usize::from(b2 >> 4))?;
    let rate = match ((b2 >> 2) & 3, version) {
        (i @ 0..=2, 3) => [44100, 48000, 32000][usize::from(i)],
        (i @ 0..=2, 2) => [22050, 24000, 16000][usize::from(i)],
        (i @ 0..=2, 0) => [11025, 12000, 8000][usize::from(i)],
        _ => return None,
    };
    if kbps == 0 {
        return None;
    }
    let (samples, mono) = (if version == 3 { 1152 } else { 576 }, b3 >> 6 == 3);
    let len = (samples / 8 * kbps * 1000 / rate) as usize + usize::from((b2 >> 1) & 1);
    let side = match (version == 3, mono) {
        (true, false) => 32,
        (true, true) | (false, false) => 17,
        (false, true) => 9,
    };
    Some(Frame { len, samples, rate, side })
}

/// An MP3's length: the Xing/Info frame count when the first frame has one, else every frame walked.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn mp3_seconds(bytes: &[u8]) -> Option<f64> {
    let mut at = 0;
    if let [b'I', b'D', b'3', _, _, flags, s0, s1, s2, s3, ..] = *bytes {
        let size = [s0, s1, s2, s3].iter().fold(0usize, |n, &b| (n << 7) | usize::from(b & 0x7F));
        at = 10 + size + if flags & 0x10 != 0 { 10 } else { 0 };
    }
    let first = frame(bytes.get(at..)?)?;
    let xing = at + 4 + first.side;
    if let Some(tag) = bytes.get(xing..xing + 4)
        && (tag == b"Xing" || tag == b"Info")
        && be32(bytes, xing + 4)? & 1 != 0
    {
        let frames = be32(bytes, xing + 8)?;
        return (frames > 0).then(|| f64::from(frames) * f64::from(first.samples) / f64::from(first.rate));
    }
    let mut seconds = 0.0;
    while let Some(f) = bytes.get(at..).and_then(frame) {
        if at + f.len > bytes.len() {
            break;
        }
        seconds += f64::from(f.samples) / f64::from(f.rate);
        at += f.len;
    }
    (seconds > 0.0).then_some(seconds)
}

/// An SVG's size: its root's `width` and `height` in plain numbers or `px`, else its `viewBox`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn svg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(bytes.get(..bytes.len().min(64 * 1024))?).ok()?;
    let start = text.find("<svg")?;
    let tag = &text[start..start + text[start..].find('>')?];
    let attr = |name: &str| {
        let at = tag.match_indices(name).find(|(i, _)| tag[..*i].ends_with(char::is_whitespace))?.0 + name.len();
        let rest = tag[at..].trim_start().strip_prefix('=')?.trim_start();
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        rest[1..].split(quote).next()
    };
    let number = |v: &str| v.trim().trim_end_matches("px").parse::<f64>().ok().filter(|n| *n > 0.0 && n.is_finite());
    let size = |w: f64, h: f64| Some((w.round() as u32, h.round() as u32)).filter(|(w, h)| *w > 0 && *h > 0);
    if let (Some(w), Some(h)) = (attr("width").and_then(number), attr("height").and_then(number)) {
        return size(w, h);
    }
    let view: Vec<f64> =
        attr("viewBox")?.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect();
    match view[..] {
        [_, _, w, h] if w > 0.0 && h > 0.0 => size(w, h),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(body.len() + 8).unwrap().to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn tkhd(version: u8, w: u32, h: u32) -> Vec<u8> {
        let mut body = vec![version, 0, 0, 0];
        body.resize(4 + if version == 0 { 72 } else { 84 }, 0);
        body.extend((w << 16).to_be_bytes());
        body.extend((h << 16).to_be_bytes());
        boxed(b"tkhd", &body)
    }

    fn mvhd_v0(scale: u32, duration: u32) -> Vec<u8> {
        let mut body = vec![0; 12];
        body.extend(scale.to_be_bytes());
        body.extend(duration.to_be_bytes());
        body.resize(100, 0);
        boxed(b"mvhd", &body)
    }

    #[test]
    fn mp4_size_from_the_video_track_and_length_from_mvhd() {
        let audio = boxed(b"trak", &tkhd(0, 0, 0));
        let video = boxed(b"trak", &[tkhd(0, 1280, 720), boxed(b"mdia", &[])].concat());
        let moov = boxed(b"moov", &[mvhd_v0(1000, 8000), audio, video].concat());
        let file = [boxed(b"ftyp", b"isom\0\0\0\0isom"), boxed(b"mdat", &[0; 64]), moov].concat();
        assert_eq!(sniff(&file), Some("video/mp4"));
        assert_eq!(mp4(&file), Mp4 { size: Some((1280, 720)), seconds: Some(8.0) });
    }

    #[test]
    fn mp4_version_1_boxes_and_unreadable_files() {
        let mut mvhd = vec![1, 0, 0, 0];
        mvhd.extend([0; 16]);
        mvhd.extend(600u32.to_be_bytes());
        mvhd.extend(3000u64.to_be_bytes());
        mvhd.resize(112, 0);
        let moov = boxed(b"moov", &[boxed(b"mvhd", &mvhd), boxed(b"trak", &tkhd(1, 1080, 1920))].concat());
        assert_eq!(mp4(&moov), Mp4 { size: Some((1080, 1920)), seconds: Some(5.0) });
        assert_eq!(mp4(b"not a video at all"), Mp4::default());
        assert_eq!(mp4(&boxed(b"moov", &[0xFF; 3])), Mp4::default());
    }

    #[test]
    fn svg_size_from_width_and_height_or_the_view_box() {
        assert_eq!(svg_size(br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="512" height="256px">"#), Some((512, 256)));
        assert_eq!(svg_size(br#"<svg stroke-width="2" viewBox="0 0 1024 768"><path/></svg>"#), Some((1024, 768)));
        assert_eq!(svg_size(br#"<svg width="100%" height="100%" viewBox="0,0,64,32">"#), Some((64, 32)));
        assert_eq!(svg_size(br#"<svg width="10em" height="5em">"#), None);
        assert_eq!(svg_size(b"not svg"), None);
    }

    /// An MPEG 1 Layer III frame, 128 kbps, 44.1 kHz, stereo: 417 bytes.
    fn mp3_frame(fill: &[u8]) -> Vec<u8> {
        let mut f = vec![0xFF, 0xFB, 0x90, 0x00];
        f.extend_from_slice(fill);
        f.resize(417, 0);
        f
    }

    #[test]
    fn mp3_length_from_its_frames_after_an_id3_tag() {
        let mut file = b"ID3\x04\x00\x00\x00\x00\x00\x05hello".to_vec();
        for _ in 0..100 {
            file.extend(mp3_frame(&[]));
        }
        assert_eq!(sniff(&file), Some("audio/mpeg"));
        let seconds = mp3_seconds(&file).unwrap();
        assert!((seconds - 100.0 * 1152.0 / 44100.0).abs() < 1e-9, "{seconds}");
        assert_eq!(mp3_seconds(b"ID3\x04\x00\x00\x00\x00\x00\x00"), None);
        assert_eq!(mp3_seconds(&[0; 64]), None);
    }

    #[test]
    fn mp3_length_from_a_xing_header() {
        let mut fill = vec![0; 32];
        fill.extend(b"Xing");
        fill.extend(1u32.to_be_bytes());
        fill.extend(1000u32.to_be_bytes());
        let file = [mp3_frame(&fill), mp3_frame(&[])].concat();
        let seconds = mp3_seconds(&file).unwrap();
        assert!((seconds - 1000.0 * 1152.0 / 44100.0).abs() < 1e-9, "{seconds}");
    }
}
