//! Accelerated-paint IOSurface → CVPixelBuffer (what gpui's `window.paint_surface` takes).

use std::ffi::c_void;

use anyhow::{Result, bail};
use core_foundation::base::{TCFType, kCFAllocatorDefault};
use core_video::pixel_buffer::{CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferRef, kCVPixelBufferLock_ReadOnly};
use core_video::r#return::kCVReturnSuccess;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferCreateWithIOSurface(allocator: *const c_void, surface: *mut c_void, attributes: *const c_void, out: *mut CVPixelBufferRef)
    -> i32;
    fn CVPixelBufferGetIOSurface(buffer: CVPixelBufferRef) -> *mut c_void;
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetID(surface: *mut c_void) -> u32;
}

/// `'BGRA'`, the only format CEF's accelerated paint produces on macOS.
pub const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// The latest painted view, retaining CEF's IOSurface.
#[derive(Clone)]
pub struct Frame {
    pub buffer: CVPixelBuffer,
    pub width: u32,
    pub height: u32,
}

// SAFETY: CVPixelBuffer/IOSurface are CoreFoundation objects with atomic retain/release.
unsafe impl Send for Frame {}

impl Frame {
    /// Wraps (and retains) `surface`. Call only inside `on_accelerated_paint`.
    pub(crate) fn from_io_surface(surface: *mut c_void) -> Result<Frame> {
        if surface.is_null() {
            bail!("accelerated paint without an IOSurface");
        }
        let mut raw: CVPixelBufferRef = std::ptr::null_mut();
        let status = unsafe { CVPixelBufferCreateWithIOSurface(kCFAllocatorDefault.cast(), surface, std::ptr::null(), &mut raw) };
        if status != kCVReturnSuccess || raw.is_null() {
            bail!("CVPixelBufferCreateWithIOSurface failed: {status}");
        }
        let buffer = unsafe { CVPixelBuffer::wrap_under_create_rule(raw) };
        let (width, height) = (buffer.get_width() as u32, buffer.get_height() as u32);
        if buffer.get_pixel_format() != BGRA {
            bail!("unexpected pixel format {:#x}", buffer.get_pixel_format());
        }
        Ok(Frame { buffer, width, height })
    }
}

impl Frame {
    /// The IOSurface's system-wide id: CEF paints into a small pool of
    /// surfaces in turn, so ids repeat (for diagnostics).
    pub fn surface_id(&self) -> u32 {
        let surface = unsafe { CVPixelBufferGetIOSurface(self.buffer.as_concrete_TypeRef()) };
        if surface.is_null() { 0 } else { unsafe { IOSurfaceGetID(surface) } }
    }

    /// Reads one BGRA pixel back from the surface (CPU lock; for tests and diagnostics).
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        if self.buffer.lock_base_address(kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess {
            return None;
        }
        let stride = self.buffer.get_bytes_per_row();
        let base = unsafe { CVPixelBufferGetBaseAddress(self.buffer.as_concrete_TypeRef()) };
        let pixel = (!base.is_null()).then(|| unsafe {
            let at = base.cast::<u8>().add(y as usize * stride + x as usize * 4);
            [*at, *at.add(1), *at.add(2), *at.add(3)]
        });
        self.buffer.unlock_base_address(kCVPixelBufferLock_ReadOnly);
        pixel
    }
}
