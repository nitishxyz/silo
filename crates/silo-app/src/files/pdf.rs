//! PDF pages for the viewer, drawn with CoreGraphics (macOS): the pages'
//! sizes, and a page as a bitmap of a given width. Elsewhere a PDF shows as
//! a binary file.

use std::sync::Arc;

/// The size of each page in points (as shown: its crop box, rotated).
#[cfg(target_os = "macos")]
#[must_use]
pub fn page_sizes(bytes: &[u8]) -> Option<Vec<(f32, f32)>> {
    let doc = imp::document(bytes)?;
    let count = objc2_core_graphics::CGPDFDocument::number_of_pages(Some(&doc));
    (count > 0).then(|| (1..=count).map(|n| imp::page(&doc, n).map_or((612.0, 792.0), |p| imp::size(&p))).collect())
}

/// Pages (index from 0, width in pixels) as bitmaps, white behind them.
#[cfg(target_os = "macos")]
#[must_use]
pub fn render(bytes: &[u8], pages: &[(usize, u32)]) -> Vec<(usize, u32, Arc<gpui::RenderImage>)> {
    let Some(doc) = imp::document(bytes) else { return Vec::new() };
    pages.iter().filter_map(|&(i, w)| Some((i, w, Arc::new(imp::render(&doc, i, w)?)))).collect()
}

#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn page_sizes(_: &[u8]) -> Option<Vec<(f32, f32)>> {
    None
}

#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn render(_: &[u8], _: &[(usize, u32)]) -> Vec<(usize, u32, Arc<gpui::RenderImage>)> {
    Vec::new()
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2_core_foundation::{CFData, CFRetained, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGColorSpace, CGContext, CGDataProvider, CGImageAlphaInfo, CGImageByteOrderInfo, CGPDFBox, CGPDFDocument, CGPDFPage,
    };

    pub fn document(bytes: &[u8]) -> Option<CFRetained<CGPDFDocument>> {
        let data = CFData::from_bytes(bytes);
        let provider = CGDataProvider::with_cf_data(Some(&data))?;
        CGPDFDocument::with_provider(Some(&provider))
    }

    pub fn page(doc: &CGPDFDocument, number: usize) -> Option<CFRetained<CGPDFPage>> {
        CGPDFDocument::page(Some(doc), number)
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn size(page: &CGPDFPage) -> (f32, f32) {
        let r = CGPDFPage::box_rect(Some(page), CGPDFBox::CropBox);
        let (w, h) = (r.size.width.abs() as f32, r.size.height.abs() as f32);
        if CGPDFPage::rotation_angle(Some(page)).rem_euclid(180) == 90 { (h, w) } else { (w, h) }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    pub fn render(doc: &CGPDFDocument, index: usize, width: u32) -> Option<gpui::RenderImage> {
        let page = page(doc, index + 1)?;
        let (w, h) = size(&page);
        let scale = f64::from(width) / f64::from(w.max(1.0));
        let height = (f64::from(h) * scale).round().max(1.0) as u32;
        let (pw, ph) = (width as usize, height as usize);
        let mut pixels = vec![0u8; pw * ph * 4];
        let space = CGColorSpace::new_device_rgb()?;
        // BGRA in memory, as gpui's images are.
        let info = CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0;
        // SAFETY: `pixels` holds `ph` rows of `pw * 4` bytes and outlives the context.
        let cx = unsafe { CGBitmapContextCreate(pixels.as_mut_ptr().cast(), pw, ph, 8, pw * 4, Some(&space), info) }?;
        let all = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: f64::from(width), height: f64::from(height) } };
        CGContext::set_rgb_fill_color(Some(&cx), 1.0, 1.0, 1.0, 1.0);
        CGContext::fill_rect(Some(&cx), all);
        CGContext::scale_ctm(Some(&cx), scale, scale);
        let shown = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: f64::from(w), height: f64::from(h) } };
        let transform = CGPDFPage::drawing_transform(Some(&page), CGPDFBox::CropBox, shown, 0, true);
        CGContext::concat_ctm(Some(&cx), transform);
        CGContext::draw_pdf_page(Some(&cx), Some(&page));
        drop(cx);
        let buffer = image::RgbaImage::from_raw(width, height, pixels)?;
        Some(gpui::RenderImage::new(vec![image::Frame::new(buffer)]))
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// One 200×100 pt page, its left half filled black.
    const PDF: &str = "%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
        3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]/Contents 4 0 R>>endobj\n\
        4 0 obj<</Length 17>>stream\n0 0 100 100 re f\nendstream endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n";

    #[test]
    fn pages_measure_and_draw_at_the_width_asked() {
        assert_eq!(super::page_sizes(PDF.as_bytes()), Some(vec![(200.0, 100.0)]));
        assert_eq!(super::page_sizes(b"not a pdf"), None);
        let drawn = super::render(PDF.as_bytes(), &[(0, 400), (5, 400)]);
        assert_eq!(drawn.len(), 1, "a page that isn't there is skipped");
        let (index, width, image) = &drawn[0];
        assert_eq!((*index, *width), (0, 400));
        let size = image.size(0);
        assert_eq!((i32::from(size.width), i32::from(size.height)), (400, 200));
        let bytes = image.as_bytes(0).unwrap();
        let pixel = |x: usize, y: usize| &bytes[(y * 400 + x) * 4..(y * 400 + x) * 4 + 4];
        assert_eq!(pixel(10, 100), [0, 0, 0, 255], "the filled half (BGRA)");
        assert_eq!(pixel(390, 100), [255, 255, 255, 255], "white behind the page");
    }
}
