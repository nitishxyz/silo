//! Generated SVG (`docs/MEDIA.md` §5.3): parsed once off the main thread for its intrinsic size, and drawn
//! sharp at any zoom. gpui rasterizes an SVG image once (2 px a unit), so tiles get a raster at their size
//! and the viewer, past that, a raster of just what shows at the zoomed scale.

use std::sync::{Arc, LazyLock};

use resvg::{tiny_skia, usvg};

/// System fonts for SVG text: loaded once, by the first SVG (off the main thread).
static FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    Arc::new(db)
});

/// The longest raster side made (a viewport at 2× fits; a whole huge document does not).
const MAX_SIDE: f32 = 8192.0;

/// A region `x, y, w, h`.
pub type Rect = (f32, f32, f32, f32);

/// `bytes` read as SVG: text whose head has an `<svg` tag.
#[must_use]
pub fn sniff(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]);
    head.trim_start_matches('\u{feff}').trim_start().starts_with('<') && head.contains("<svg")
}

pub struct Svg(usvg::Tree);

impl Svg {
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let options = usvg::Options { fontdb: FONTS.clone(), ..usvg::Options::default() };
        usvg::Tree::from_data(bytes, &options).ok().map(Self)
    }

    /// Its intrinsic size in points.
    #[must_use]
    pub fn size(&self) -> (f32, f32) {
        let s = self.0.size();
        (s.width(), s.height())
    }

    /// `region` (in units of `dims`, the size it's laid out at) at `ds` pixels a unit: straight-alpha RGBA,
    /// or BGRA for gpui's [`gpui::RenderImage`].
    #[must_use]
    pub fn pixels(&self, dims: (f32, f32), region: Rect, ds: f32, bgra: bool) -> Option<(u32, u32, Vec<u8>)> {
        let (w, h) = ((region.2 * ds).ceil(), (region.3 * ds).ceil());
        if !(1.0..=MAX_SIDE).contains(&w) || !(1.0..=MAX_SIDE).contains(&h) {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let mut pixmap = tiny_skia::Pixmap::new(w as u32, h as u32)?;
        // Units of `dims` to the document's own.
        let size = self.size();
        let (sx, sy) = (ds * dims.0 / size.0.max(0.001), ds * dims.1 / size.1.max(0.001));
        let transform = tiny_skia::Transform::from_row(sx, 0.0, 0.0, sy, -region.0 * ds, -region.1 * ds);
        resvg::render(&self.0, transform, &mut pixmap.as_mut());
        let (w, h) = (pixmap.width(), pixmap.height());
        let mut data = pixmap.take();
        for px in data.as_chunks_mut::<4>().0 {
            gpui::swap_rgba_pa_to_bgra(px);
            if !bgra {
                px.swap(0, 2);
            }
        }
        Some((w, h, data))
    }

    /// `region` as a gpui image (see [`Svg::pixels`]).
    #[must_use]
    pub fn render(&self, dims: (f32, f32), region: Rect, ds: f32) -> Option<gpui::RenderImage> {
        let (w, h, data) = self.pixels(dims, region, ds, true)?;
        Some(gpui::RenderImage::new(vec![image::Frame::new(image::RgbaImage::from_raw(w, h, data)?)]))
    }

    /// The whole of it, `edge` pixels on its long side, as a PNG.
    #[must_use]
    pub fn png(&self, edge: f32) -> Option<Vec<u8>> {
        let (w, h) = self.size();
        let (w, h, data) = self.pixels((w, h), (0.0, 0.0, w, h), edge / w.max(h).max(0.001), false)?;
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_raw(w, h, data)?.write_to(&mut png, image::ImageFormat::Png).ok()?;
        Some(png.into_inner())
    }
}

/// What a view `view` big shows of an image `dims` big drawn at `origin` (view-local) and `scale`: in the image's
/// units, its edges on device pixels (`device` a point). `None` when none of it shows.
#[must_use]
pub fn visible(dims: (f32, f32), origin: (f32, f32), scale: f32, view: (f32, f32), device: f32) -> Option<Rect> {
    let axis = |o: f32, len: f32, view: f32| {
        let (a, b) = (((o.max(0.0)) * device).floor() / device, ((o + len * scale).min(view) * device).ceil() / device);
        (b > a).then(|| ((a - o) / scale, (b - a) / scale))
    };
    let ((x, w), (y, h)) = (axis(origin.0, dims.0, view.0)?, axis(origin.1, dims.1, view.1)?);
    Some((x, y, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGO: &str =
        r##"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="20" height="20" fill="#f00"/></svg>"##;

    #[test]
    fn svg_parses_and_draws_what_is_asked() {
        assert!(sniff(LOGO.as_bytes()) && !sniff(b"\x89PNG\r\n") && !sniff(b"GIF89a"));
        let svg = Svg::parse(LOGO.as_bytes()).unwrap();
        assert_eq!(svg.size(), (40.0, 20.0));
        // Its right half at 4 px a unit: 80×80, all clear (the rect is on the left).
        let (w, h, data) = svg.pixels((40.0, 20.0), (20.0, 0.0, 20.0, 20.0), 4.0, false).unwrap();
        assert_eq!((w, h), (80, 80));
        assert!(data.as_chunks::<4>().0.iter().all(|p| p[3] == 0));
        // The left half: red, straight RGBA; BGRA for gpui.
        let (_, _, data) = svg.pixels((40.0, 20.0), (0.0, 0.0, 20.0, 20.0), 1.0, false).unwrap();
        assert_eq!(&data[..4], &[255, 0, 0, 255]);
        let (_, _, data) = svg.pixels((40.0, 20.0), (0.0, 0.0, 20.0, 20.0), 1.0, true).unwrap();
        assert_eq!(&data[..4], &[0, 0, 255, 255]);
        assert!(svg.png(64.0).is_some());
        assert!(svg.pixels((40.0, 20.0), (0.0, 0.0, 40.0, 20.0), 1000.0, true).is_none(), "too big a raster");
    }

    #[test]
    fn only_what_shows_is_drawn() {
        // A 1000-unit square at 2× scale, its origin 100 pt left of and above an 800×600 view.
        let r = visible((1000.0, 1000.0), (-100.0, -100.0), 2.0, (800.0, 600.0), 2.0).unwrap();
        assert_eq!(r, (50.0, 50.0, 400.0, 300.0));
        // Fully inside: all of it.
        assert_eq!(visible((100.0, 50.0), (10.0, 20.0), 1.0, (800.0, 600.0), 2.0), Some((0.0, 0.0, 100.0, 50.0)));
        assert_eq!(visible((100.0, 50.0), (900.0, 0.0), 1.0, (800.0, 600.0), 2.0), None);
    }
}
