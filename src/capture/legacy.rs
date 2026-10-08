//! CoreGraphics capture for macOS 10.13, built without ScreenCaptureKit.

use super::*;
use anyhow::{anyhow, Context};
use std::collections::VecDeque;
use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayCreateImage(display: u32) -> *const c_void;
    fn CGImageGetWidth(image: *const c_void) -> usize;
    fn CGImageGetHeight(image: *const c_void) -> usize;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        colorspace: *const c_void,
        bitmap_info: u32,
    ) -> *const c_void;
    fn CGContextDrawImage(ctx: *const c_void, rect: CGRect, image: *const c_void);
    fn CGColorSpaceCreateDeviceRGB() -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: *const c_void);
}

/// Quartz draws into a caller-owned, tightly packed BGRA buffer. Its 10.13
/// display screenshot API works without SCK and lets us scale to the client
/// size in the same pass. A valid GUI display is still required by WindowServer.
fn capture_frame(
    display: u32,
    width: u16,
    height: u16,
    preserve_aspect: bool,
) -> Result<(Vec<u8>, bool)> {
    let stride = usize::from(width) * 4;
    let mut frame = vec![0u8; stride * usize::from(height)];
    let letterbox;
    unsafe {
        let image = CGDisplayCreateImage(display);
        if image.is_null() {
            return Err(anyhow!(
                "CGDisplayCreateImage({display}) failed; the current user needs an active GUI display"
            ));
        }
        let source_width = CGImageGetWidth(image);
        let source_height = CGImageGetHeight(image);
        if source_width == 0 || source_height == 0 {
            CFRelease(image);
            return Err(anyhow!("CGDisplayCreateImage returned an empty image"));
        }
        let colorspace = CGColorSpaceCreateDeviceRGB();
        if colorspace.is_null() {
            CFRelease(image);
            return Err(anyhow!("CGColorSpaceCreateDeviceRGB failed"));
        }
        // kCGBitmapByteOrder32Little | kCGImageAlphaPremultipliedFirst.
        // On little-endian Intel this stores bytes in B,G,R,A order.
        let context = CGBitmapContextCreate(
            frame.as_mut_ptr().cast(),
            usize::from(width),
            usize::from(height),
            8,
            stride,
            colorspace,
            0x2000 | 2,
        );
        if context.is_null() {
            CFRelease(colorspace);
            CFRelease(image);
            return Err(anyhow!("CGBitmapContextCreate failed"));
        }
        // CGBitmapContext's first memory row maps to the Quartz bottom row.
        // CGDisplayCreateImage already has the display's row orientation here;
        // flipping the CTM makes the RDP bitmap appear upside down.
        let source_aspect = source_width as f64 / source_height as f64;
        let target_aspect = f64::from(width) / f64::from(height);
        letterbox = preserve_aspect && (source_aspect - target_aspect).abs() > 0.001;
        let (draw_width, draw_height) = if letterbox && source_aspect > target_aspect {
            (f64::from(width), f64::from(width) / source_aspect)
        } else if letterbox {
            (f64::from(height) * source_aspect, f64::from(height))
        } else {
            (f64::from(width), f64::from(height))
        };
        CGContextDrawImage(
            context,
            CGRect {
                origin: CGPoint {
                    x: (f64::from(width) - draw_width) / 2.0,
                    y: (f64::from(height) - draw_height) / 2.0,
                },
                size: CGSize {
                    width: draw_width,
                    height: draw_height,
                },
            },
            image,
        );
        CFRelease(context);
        CFRelease(colorspace);
        CFRelease(image);
    }
    Ok((frame, letterbox))
}

pub struct LegacyCaptureUpdates {
    display: u32,
    width: u16,
    height: u16,
    screen_size_pts: (f64, f64),
    cursor_scale: f64,
    preserve_aspect: bool,
    interval: tokio::time::Interval,
    pending: VecDeque<DisplayUpdate>,
    last_frame: Vec<u8>,
    cursor: CursorState,
    pending_resize: PendingResize,
    desktop_size: SharedDesktopSize,
    suppress_next_adopt: Arc<AtomicBool>,
    display_suppressed: Option<Arc<AtomicBool>>,
}

impl LegacyCaptureUpdates {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: u16,
        height: u16,
        fps: u32,
        display_id: Option<u32>,
        screen_size_pts: (f64, f64),
        cursor_scale: f64,
        preserve_aspect: bool,
        pending_resize: PendingResize,
        desktop_size: SharedDesktopSize,
        suppress_next_adopt: Arc<AtomicBool>,
        display_suppressed: Option<Arc<AtomicBool>>,
    ) -> Result<Self> {
        let mut count = 0;
        let result = unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) };
        if result != 0 || count == 0 {
            return Err(anyhow!(
                "macOS reports no active displays for this GUI session; macOS 10.13 cannot create a CGVirtualDisplay"
            ));
        }
        let display = display_id.unwrap_or_else(|| unsafe { CGMainDisplayID() });
        if display == 0 {
            return Err(anyhow!("CGMainDisplayID returned zero"));
        }
        // Fail at connection setup instead of leaving a black, inert RDP session.
        let (_, letterbox) = capture_frame(display, width, height, preserve_aspect)
            .context("initial legacy display capture")?;
        desktop_size.set_letterbox(letterbox);
        let mut interval =
            tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(fps.max(1))));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        if let Some(flag) = &display_suppressed {
            flag.store(false, Ordering::Relaxed);
        }
        Ok(Self {
            display,
            width,
            height,
            screen_size_pts,
            cursor_scale,
            preserve_aspect,
            interval,
            pending: VecDeque::new(),
            last_frame: Vec::new(),
            cursor: CursorState::new(width, height, screen_size_pts, cursor_scale)?,
            pending_resize,
            desktop_size,
            suppress_next_adopt,
            display_suppressed,
        })
    }

    fn enqueue_frame(&mut self, frame: Vec<u8>) {
        if frame == self.last_frame {
            return;
        }
        let stride = usize::from(self.width) * 4;
        let mut rects = Vec::new();
        if self.last_frame.len() != frame.len() {
            rects.extend(split_strips(0, 0, self.width, self.height));
        } else {
            // Quartz provides a full screenshot, so compare small tiles against
            // the preceding frame before asking the RDP bitmap encoder to work.
            // Adjacent changed tiles in a row share one update.
            const TILE: usize = 64;
            let width = usize::from(self.width);
            let height = usize::from(self.height);
            let mut changed_pixels = 0usize;
            for y in (0..height).step_by(TILE) {
                let tile_height = TILE.min(height - y);
                let mut run_start = None;
                for x in (0..width).step_by(TILE) {
                    let tile_width = TILE.min(width - x);
                    let changed = (y..y + tile_height).any(|row| {
                        let offset = row * stride + x * 4;
                        let end = offset + tile_width * 4;
                        frame[offset..end] != self.last_frame[offset..end]
                    });
                    if changed {
                        changed_pixels += tile_width * tile_height;
                        run_start.get_or_insert(x);
                    } else if let Some(start) = run_start.take() {
                        rects.push((
                            start as u16,
                            y as u16,
                            (x - start) as u16,
                            tile_height as u16,
                        ));
                    }
                }
                if let Some(start) = run_start {
                    rects.push((
                        start as u16,
                        y as u16,
                        (width - start) as u16,
                        tile_height as u16,
                    ));
                }
            }
            // A busy full-screen app is cheaper to encode as a few large
            // strips than as hundreds of small tile updates.
            if changed_pixels * 2 >= width * height {
                rects.clear();
                rects.extend(split_strips(0, 0, self.width, self.height));
            }
        }
        for (x, y, w, h) in rects {
            let mut data = Vec::with_capacity(usize::from(w) * usize::from(h) * 4);
            for row in y..y + h {
                let offset = usize::from(row) * stride + usize::from(x) * 4;
                data.extend_from_slice(&frame[offset..offset + usize::from(w) * 4]);
            }
            self.pending.push_back(DisplayUpdate::Bitmap(BitmapUpdate {
                x,
                y,
                width: NonZeroU16::new(w).unwrap(),
                height: NonZeroU16::new(h).unwrap(),
                format: PixelFormat::BgrA32,
                data: Bytes::from(data),
                stride: NonZeroUsize::new(usize::from(w) * 4).unwrap(),
            }));
        }
        self.last_frame = frame;
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for LegacyCaptureUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        loop {
            if let Some((width, height)) = self.pending_resize.take_legacy_ready() {
                self.width = width;
                self.height = height;
                self.last_frame.clear();
                self.pending.clear();
                self.cursor =
                    CursorState::new(width, height, self.screen_size_pts, self.cursor_scale)?;
                self.desktop_size.set(width, height);
                self.suppress_next_adopt.store(true, Ordering::Relaxed);
                return Ok(Some(DisplayUpdate::Resize(DesktopSize { width, height })));
            }
            if let Some(update) = self.pending.pop_front() {
                return Ok(Some(update));
            }
            self.interval.tick().await;
            if let Some(update) = self.cursor.poll().into_iter().next() {
                return Ok(Some(update));
            }
            if self
                .display_suppressed
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed))
                && !self.last_frame.is_empty()
            {
                continue;
            }
            let display = self.display;
            let width = self.width;
            let height = self.height;
            let preserve_aspect = self.preserve_aspect;
            let (frame, letterbox) = tokio::task::spawn_blocking(move || {
                capture_frame(display, width, height, preserve_aspect)
            })
            .await
            .context("legacy capture worker panicked")??;
            self.desktop_size.set_letterbox(letterbox);
            self.enqueue_frame(frame);
        }
    }
}
