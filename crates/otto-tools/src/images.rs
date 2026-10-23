//! Images for a model: downscaled to `MAX_EDGE` on the long edge and
//! recompressed under `MAX_BYTES`, never upscaled.
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat, imageops::FilterType};

/// Long edge sent by default (Anthropic's recommended size).
pub const MAX_EDGE: u32 = 1568;
/// Encoded bytes per image; base64 keeps it under the 5 MB providers allow.
pub const MAX_BYTES: usize = 3_700_000;
pub const QUALITY: u8 = 85;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    pub mime: &'static str,
    pub bytes: Vec<u8>,
    pub original: (u32, u32),
    pub sent: (u32, u32),
}

impl Prepared {
    /// `3024×1964 → 1568×1018`, or `800×600` when sent as is.
    #[must_use]
    pub fn dimensions(&self) -> String {
        let (w, h) = self.original;
        if self.original == self.sent { format!("{w}×{h}") } else { format!("{w}×{h} → {}×{}", self.sent.0, self.sent.1) }
    }
}

/// The mime of an image a model takes as is.
#[must_use]
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

/// Whether `bytes` decode as an image here (PNG, JPEG, GIF, WebP, TIFF, BMP).
#[must_use]
pub fn decodable(bytes: &[u8]) -> bool {
    image::guess_format(bytes).is_ok_and(|f| {
        matches!(f, ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP | ImageFormat::Tiff | ImageFormat::Bmp)
    })
}

/// `bytes` fit for a model: at most `max_edge` on the long edge and
/// `MAX_BYTES` encoded. Small PNG/JPEG/GIF/WebP files pass unchanged;
/// others become PNG (transparency, or a PNG source that fits) or JPEG at `quality`.
///
/// # Errors
/// Not a decodable image, or it can't be made small enough.
pub fn prepare(bytes: &[u8], max_edge: u32, quality: u8) -> Result<Prepared, String> {
    let format = image::guess_format(bytes).map_err(|e| e.to_string())?;
    let mut decoder = image::ImageReader::with_format(std::io::Cursor::new(bytes), format).into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    let upright = orientation == image::metadata::Orientation::NoTransforms;
    img.apply_orientation(orientation);
    let original = img.dimensions();
    let max_edge = max_edge.max(1);
    let fits = original.0.max(original.1) <= max_edge;
    if let Some(mime) = sniff(bytes).filter(|_| fits && bytes.len() <= MAX_BYTES && upright) {
        return Ok(Prepared { mime, bytes: bytes.to_vec(), original, sent: original });
    }
    let alpha = img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p.0[3] < 255);
    let png_first = alpha || matches!(format, ImageFormat::Png | ImageFormat::Gif | ImageFormat::Bmp | ImageFormat::Tiff);
    let mut edge = original.0.max(original.1).min(max_edge);
    let mut quality = quality.clamp(1, 100);
    loop {
        let scaled = if edge >= original.0.max(original.1) { img.clone() } else { resize(&img, original, edge) };
        let sent = scaled.dimensions();
        if png_first && let Some(png) = encode(&scaled, ImageFormat::Png, quality).filter(|b| b.len() <= MAX_BYTES) {
            return Ok(Prepared { mime: "image/png", bytes: png, original, sent });
        }
        if !alpha && let Some(jpeg) = encode(&scaled, ImageFormat::Jpeg, quality).filter(|b| b.len() <= MAX_BYTES) {
            return Ok(Prepared { mime: "image/jpeg", bytes: jpeg, original, sent });
        }
        if edge <= 64 {
            return Err("image can't be made small enough".into());
        }
        edge = edge * 3 / 4;
        quality = quality.saturating_sub(10).max(50);
    }
}

fn resize(img: &DynamicImage, (w, h): (u32, u32), edge: u32) -> DynamicImage {
    let scale = f64::from(edge) / f64::from(w.max(h));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (nw, nh) = (((f64::from(w) * scale).round() as u32).max(1), ((f64::from(h) * scale).round() as u32).max(1));
    img.resize_exact(nw, nh, FilterType::Lanczos3)
}

fn encode(img: &DynamicImage, format: ImageFormat, quality: u8) -> Option<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    match format {
        ImageFormat::Jpeg => {
            let rgb = DynamicImage::ImageRgb8(img.to_rgb8());
            rgb.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)).ok()?;
        }
        _ => {
            let img = if img.color().has_alpha() { DynamicImage::ImageRgba8(img.to_rgba8()) } else { DynamicImage::ImageRgb8(img.to_rgb8()) };
            img.write_to(&mut out, ImageFormat::Png).ok()?;
        }
    }
    Some(out.into_inner())
}
