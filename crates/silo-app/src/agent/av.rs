//! AVFoundation for generated video and audio (`docs/MEDIA.md` §6.4), macOS: a file's length and a video's
//! poster frame (off the main thread), an `AVPlayer` for audio rows, and the viewer's video: an `AVPlayer`
//! whose item feeds an `AVPlayerItemVideoOutput` of IOSurface-backed BGRA pixel buffers, drawn by gpui's
//! `surface` element (no native view).

pub use imp::{Frame, Player, Video, copy_file, hide_cursor, probe, still};

/// What AVFoundation read from a cache file.
pub struct Probe {
    /// A video's first frame (PNG) and its size in pixels.
    pub poster: Option<(Vec<u8>, (u32, u32))>,
    pub ms: Option<u64>,
}

#[cfg(target_os = "macos")]
mod imp {
    use std::{cell::RefCell, path::Path};

    use core_foundation::{
        base::{CFType, TCFType},
        boolean::CFBoolean,
        dictionary::CFDictionary,
        number::CFNumber,
        string::CFString,
    };
    use core_video::pixel_buffer::{CVPixelBuffer, CVPixelBufferKeys, kCVPixelBufferLock_ReadOnly, kCVPixelFormatType_32BGRA};
    use objc2::{
        AnyThread as _, MainThreadMarker,
        rc::Retained,
        runtime::{AnyObject, ProtocolObject},
    };
    use objc2_app_kit::{NSCursor, NSPasteboard};
    use objc2_av_foundation::{AVAsset, AVAssetImageGenerator, AVPlayer, AVPlayerItem, AVPlayerItemVideoOutput};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo};
    use objc2_core_media::CMTime;
    use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};

    fn url(path: &Path) -> Retained<NSURL> {
        NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
    }

    fn seconds(t: CMTime) -> Option<f64> {
        // SAFETY: a plain value conversion.
        let s = unsafe { t.seconds() };
        (s.is_finite() && s >= 0.0).then_some(s)
    }

    fn at(secs: f64) -> CMTime {
        // SAFETY: a plain value conversion.
        unsafe { CMTime::with_seconds(secs.max(0.0), 600) }
    }

    /// The file's length and, for a video, its first frame at most `edge` pixels on its long side. Blocks.
    #[must_use]
    pub fn probe(path: &Path, video: bool, edge: u32) -> super::Probe {
        // SAFETY: an asset on a local file URL; AVAsset may be used off the main thread.
        let asset = unsafe { AVAsset::assetWithURL(&url(path)) };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ms = seconds(unsafe { asset.duration() }).filter(|s| *s > 0.0).map(|s| (s * 1000.0).round() as u64);
        super::Probe { poster: if video { poster(&asset, edge) } else { None }, ms }
    }

    #[allow(clippy::cast_precision_loss)]
    fn poster(asset: &AVAsset, edge: u32) -> Option<(Vec<u8>, (u32, u32))> {
        // SAFETY: a generator on a live asset, configured before use; the synchronous copy runs off the main
        // thread (where AVFoundation allows it to block); `actualTime` may be null.
        let image = unsafe {
            let generator = AVAssetImageGenerator::assetImageGeneratorWithAsset(asset);
            generator.setAppliesPreferredTrackTransform(true);
            generator.setMaximumSize(CGSize { width: f64::from(edge), height: f64::from(edge) });
            #[allow(deprecated)]
            generator.copyCGImageAtTime_actualTime_error(at(0.0), std::ptr::null_mut()).ok()?
        };
        let (w, h) = (CGImage::width(Some(&image)), CGImage::height(Some(&image)));
        if w == 0 || h == 0 {
            return None;
        }
        let mut pixels = vec![0u8; w * h * 4];
        let space = CGColorSpace::new_device_rgb()?;
        // RGBA in memory; frames are opaque, so premultiplied changes nothing.
        let info = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
        // SAFETY: `pixels` holds `h` rows of `w * 4` bytes and outlives the context.
        let cx = unsafe { CGBitmapContextCreate(pixels.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&space), info) }?;
        let all = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: w as f64, height: h as f64 } };
        CGContext::draw_image(Some(&cx), all, Some(&image));
        drop(cx);
        let (w, h) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_raw(w, h, pixels)?.write_to(&mut png, image::ImageFormat::Png).ok()?;
        Some((png.into_inner(), (w, h)))
    }

    /// The file at `path` on the clipboard (Finder pastes the file).
    pub fn copy_file(path: &Path) -> bool {
        let board = NSPasteboard::generalPasteboard();
        board.clearContents();
        board.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(url(path))]))
    }

    #[link(name = "QuartzCore", kind = "framework")]
    unsafe extern "C" {
        /// The host clock (`mach_absolute_time` in seconds) item times map from.
        fn CACurrentMediaTime() -> f64;
    }

    /// The pointer hidden until it next moves (the player's chrome hid).
    pub fn hide_cursor() {
        NSCursor::setHiddenUntilMouseMoves(true);
    }

    /// An `AVPlayer` on a file, without a view; paused when dropped.
    pub struct Player(Retained<AVPlayer>);

    impl Player {
        #[must_use]
        pub fn new(path: &Path) -> Option<Self> {
            let mtm = MainThreadMarker::new()?;
            // SAFETY: a player on a local file URL, on the main thread.
            Some(Self(unsafe { AVPlayer::playerWithURL(&url(path), mtm) }))
        }

        /// Play; from the start when it played to the end.
        pub fn play(&self) {
            if self.ended() {
                self.seek(0.0);
            }
            // SAFETY: plain player calls on the main thread (as for the rest).
            unsafe { self.0.play() }
        }

        pub fn pause(&self) {
            unsafe { self.0.pause() }
        }

        #[must_use]
        pub fn playing(&self) -> bool {
            unsafe { self.0.rate() > 0.0 }
        }

        /// Stopped at its end.
        #[must_use]
        pub fn ended(&self) -> bool {
            !self.playing() && self.duration().is_some_and(|d| self.time() >= d - 0.05)
        }

        /// Where it is, in seconds.
        #[must_use]
        pub fn time(&self) -> f64 {
            seconds(unsafe { self.0.currentTime() }).unwrap_or(0.0)
        }

        /// Its length in seconds, once loaded.
        #[must_use]
        pub fn duration(&self) -> Option<f64> {
            let item = unsafe { self.0.currentItem() }?;
            seconds(unsafe { item.duration() }).filter(|d| *d > 0.0)
        }

        /// To `secs` exactly (short clips: no snapping to keyframes); a newer seek cancels an older one.
        pub fn seek(&self, secs: f64) {
            unsafe { self.0.seekToTime_toleranceBefore_toleranceAfter(at(secs), at(0.0), at(0.0)) }
        }

        pub fn set_volume(&self, volume: f32) {
            unsafe { self.0.setVolume(volume.clamp(0.0, 1.0)) }
        }

        pub fn set_muted(&self, muted: bool) {
            unsafe { self.0.setMuted(muted) }
        }
    }

    impl Drop for Player {
        fn drop(&mut self) {
            self.pause();
        }
    }

    /// A video frame: what gpui's `surface` element draws.
    pub type Frame = CVPixelBuffer;

    /// `frame` copied into a gpui image (BGRA, as gpui keeps them): what a closing viewer fades out, as a
    /// surface draws without opacity.
    #[must_use]
    pub fn still(frame: &Frame) -> Option<gpui::RenderImage> {
        let (w, h, row) = (frame.get_width(), frame.get_height(), frame.get_bytes_per_row());
        if w == 0 || h == 0 || row < w * 4 || frame.lock_base_address(kCVPixelBufferLock_ReadOnly) != 0 {
            return None;
        }
        // SAFETY: locked for reading: `h` rows of `row` bytes from the base address, each `w` BGRA pixels.
        let data = unsafe {
            let base = frame.get_base_address().cast::<u8>();
            (0..h).flat_map(|y| std::slice::from_raw_parts(base.add(y * row), w * 4).iter().copied()).collect::<Vec<u8>>()
        };
        frame.unlock_base_address(kCVPixelBufferLock_ReadOnly);
        let image = image::RgbaImage::from_raw(u32::try_from(w).ok()?, u32::try_from(h).ok()?, data)?;
        Some(gpui::RenderImage::new(vec![image::Frame::new(image)]))
    }

    /// The viewer's video: a player whose item hands out each frame as an IOSurface-backed BGRA
    /// `CVPixelBuffer` (what gpui's Metal renderer samples directly), polled on the main thread.
    pub struct Video {
        pub player: Player,
        output: Retained<AVPlayerItemVideoOutput>,
        /// The newest frame taken; held while it shows.
        frame: RefCell<Option<CVPixelBuffer>>,
    }

    impl Video {
        #[must_use]
        pub fn new(path: &Path) -> Option<Self> {
            let mtm = MainThreadMarker::new()?;
            let key = |k: CVPixelBufferKeys| CFString::from(k);
            #[allow(clippy::cast_possible_wrap)]
            let attrs = CFDictionary::<CFString, CFType>::from_CFType_pairs(&[
                (key(CVPixelBufferKeys::PixelFormatType), CFNumber::from(kCVPixelFormatType_32BGRA as i32).as_CFType()),
                (key(CVPixelBufferKeys::IOSurfaceProperties), CFDictionary::<CFString, CFType>::from_CFType_pairs(&[]).as_CFType()),
                (key(CVPixelBufferKeys::MetalCompatibility), CFBoolean::true_value().as_CFType()),
            ]);
            // SAFETY: CFDictionary is toll-free bridged to NSDictionary (CFString keys to NSString), alive for
            // the call; the item, output and player are made and wired on the main thread.
            unsafe {
                let attrs = &*attrs.as_concrete_TypeRef().cast::<NSDictionary<NSString, AnyObject>>();
                let output = AVPlayerItemVideoOutput::initWithPixelBufferAttributes(AVPlayerItemVideoOutput::alloc(), Some(attrs));
                let item = AVPlayerItem::playerItemWithURL(&url(path), mtm);
                item.addOutput(&output);
                Some(Self { player: Player(AVPlayer::playerWithPlayerItem(Some(&item), mtm)), output, frame: RefCell::default() })
            }
        }

        /// The item time on screen now.
        fn now(&self) -> CMTime {
            // SAFETY: a plain query on the main thread.
            unsafe { self.output.itemTimeForHostTime(CACurrentMediaTime()) }
        }

        /// A frame newer than the one taken is due (playing, or a seek landed).
        #[must_use]
        pub fn fresh(&self) -> bool {
            unsafe { self.output.hasNewPixelBufferForItemTime(self.now()) }
        }

        /// The frame for now: a newer one when due, else the last taken (none before the first decodes).
        #[must_use]
        pub fn frame(&self) -> Option<CVPixelBuffer> {
            let t = self.now();
            // SAFETY: the copy comes retained (+1, `copy` family); its ownership moves to the wrapper.
            if unsafe { self.output.hasNewPixelBufferForItemTime(t) }
                && let Some(buffer) = unsafe { self.output.copyPixelBufferForItemTime_itemTimeForDisplay(t, std::ptr::null_mut()) }
            {
                let buffer = unsafe { CVPixelBuffer::wrap_under_create_rule(Retained::into_raw(buffer).cast()) };
                *self.frame.borrow_mut() = Some(buffer);
            }
            self.frame.borrow().clone()
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::path::Path;

    #[must_use]
    pub fn probe(_: &Path, _: bool, _: u32) -> super::Probe {
        super::Probe { poster: None, ms: None }
    }

    pub fn copy_file(_: &Path) -> bool {
        false
    }

    pub fn hide_cursor() {}

    pub struct Player;

    impl Player {
        #[must_use]
        pub fn new(_: &Path) -> Option<Self> {
            None
        }
        pub fn play(&self) {}
        pub fn pause(&self) {}
        #[must_use]
        pub fn playing(&self) -> bool {
            false
        }
        #[must_use]
        pub fn ended(&self) -> bool {
            false
        }
        #[must_use]
        pub fn time(&self) -> f64 {
            0.0
        }
        #[must_use]
        pub fn duration(&self) -> Option<f64> {
            None
        }
        pub fn seek(&self, _: f64) {}
        pub fn set_volume(&self, _: f32) {}
        pub fn set_muted(&self, _: bool) {}
    }

    /// No frames off macOS.
    #[derive(Clone)]
    pub enum Frame {}

    #[must_use]
    pub fn still(frame: &Frame) -> Option<gpui::RenderImage> {
        match *frame {}
    }

    pub struct Video {
        pub player: Player,
    }

    impl Video {
        #[must_use]
        pub fn new(_: &Path) -> Option<Self> {
            None
        }
        #[must_use]
        pub fn fresh(&self) -> bool {
            false
        }
        #[must_use]
        pub fn frame(&self) -> Option<Frame> {
            None
        }
    }
}
