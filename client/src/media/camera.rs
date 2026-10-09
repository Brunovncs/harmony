//! Your camera: frames from the device, centre-cropped and scaled to the size sent in I420 (by
//! libyuv, in one pass where it can), the background replaced if you asked for it (the one step
//! that works in RGBA), then into a WebRTC video source and into your own preview, which the GPU
//! draws mirrored.

use super::background::{Backdrop, Compositor, Segmenter};
use super::video::{self, FrameSlot};
use super::yuv::{self, Packed};
use crate::core::settings::Background;
use gpui::SurfaceFrame;
use libwebrtc::native::yuv_helper;
use libwebrtc::prelude::VideoBuffer;
use libwebrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use libwebrtc::video_source::VideoResolution;
use libwebrtc::video_source::native::NativeVideoSource;
use nokhwa::Camera;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{ApiBackend, CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType};
use parking_lot::Mutex;
use std::cmp::Reverse;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// What a camera sends: small, at the frame rate other call apps use.
pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 360;
pub const FPS: u32 = 30;
/// Room for 360p at 30 fps to stay clean in motion; WebRTC's own ceiling at this size is 1.7 Mbps.
pub const BITRATE: u64 = 800_000;
/// Bad frames in a row before the camera counts as gone.
const MAX_BAD_FRAMES: u32 = 30;
/// The person-finding model at most this often (12 times a second): a run takes about 20 ms of a
/// core, and the mask is reused and smoothed in between.
const MASK_EVERY: Duration = Duration::from_millis(80);

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    /// Stable enough to save: the device's own name with its index as a fallback.
    pub id: String,
    pub name: String,
}

pub fn devices() -> Vec<Device> {
    nokhwa::query(ApiBackend::MediaFoundation)
        .unwrap_or_default()
        .into_iter()
        .map(|c| Device { id: c.human_name(), name: c.human_name() })
        .collect()
}

/// The pictures that ship with Harmony, by the name settings store.
pub const BUILTIN: [(&str, &str, &[u8]); 6] = [
    ("aurora", "Aurora", include_bytes!("../../assets/backgrounds/aurora.jpg")),
    ("dusk", "Dusk", include_bytes!("../../assets/backgrounds/dusk.jpg")),
    ("studio", "Studio", include_bytes!("../../assets/backgrounds/studio.jpg")),
    ("ocean", "Ocean", include_bytes!("../../assets/backgrounds/ocean.jpg")),
    ("bokeh", "Bokeh", include_bytes!("../../assets/backgrounds/bokeh.jpg")),
    ("pixels", "Pixels", include_bytes!("../../assets/backgrounds/pixels.jpg")),
];

fn segmenter() -> Option<&'static Segmenter> {
    static S: OnceLock<Option<Segmenter>> = OnceLock::new();
    S.get_or_init(|| match Segmenter::load() {
        Ok(s) => Some(s),
        Err(e) => {
            log::warn!("background replacement is unavailable: {e}");
            None
        }
    })
    .as_ref()
}

/// A background picture, scaled to cover the frame, as RGBA.
fn picture(bg: &Background, w: u32, h: u32) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = match bg {
        Background::Builtin { name } => BUILTIN.iter().find(|b| b.0 == name)?.2.to_vec(),
        Background::Image { path } => std::fs::read(path).ok()?,
        _ => return None,
    };
    let img = image::load_from_memory(&bytes).ok()?;
    let img = img.resize_to_fill(w, h, image::imageops::FilterType::Triangle).to_rgba8();
    Some(img.into_raw())
}

pub struct Capture {
    stop: Arc<AtomicBool>,
    pub source: NativeVideoSource,
    pub preview: Arc<FrameSlot>,
    background: Arc<Mutex<Background>>,
    pub error: Arc<Mutex<Option<String>>>,
}

impl Capture {
    /// Opens the camera (by id, or the first one) and starts sending frames.
    pub fn start(device: &str, background: Background) -> Capture {
        // The source keeps itself alive with a task on the network runtime.
        let _rt = crate::core::runtime().enter();
        let source = NativeVideoSource::new(VideoResolution { width: WIDTH, height: HEIGHT }, false);
        let preview = Arc::new(FrameSlot::default());
        let stop = Arc::new(AtomicBool::new(false));
        let background = Arc::new(Mutex::new(background));
        let error = Arc::new(Mutex::new(None));
        let (src, prev, st, bg, err, device) =
            (source.clone(), preview.clone(), stop.clone(), background.clone(), error.clone(), device.to_string());
        std::thread::Builder::new()
            .name("harmony-camera".into())
            .spawn(move || {
                if let Err(e) = run(&device, src, prev, st, bg) {
                    log::warn!("camera: {e}");
                    *err.lock() = Some(e.to_string());
                }
            })
            .ok();
        Capture { stop, source, preview, background, error }
    }

    pub fn set_background(&self, bg: Background) {
        *self.background.lock() = bg;
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The camera's mode to use: its full frame rate (up to `FPS`) first, then the smallest picture
/// that still covers the frame sent (or else the largest), then the format cheapest to turn into
/// it. NV12 and YUY2 crop and convert in one libyuv pass; MJPEG needs a JPEG decode first, so it
/// wins only when it is the one way to get the size or the rate.
fn best_format(formats: &[CameraFormat]) -> Option<CameraFormat> {
    let covers = |f: &CameraFormat| f.width() >= WIDTH && f.height() >= HEIGHT;
    let format = |f: &CameraFormat| match f.format() {
        FrameFormat::NV12 => 0,
        FrameFormat::YUYV => 1,
        FrameFormat::MJPEG => 2,
        _ => 3,
    };
    formats
        .iter()
        .min_by_key(|f| {
            let area = f.width() * f.height();
            (Reverse(f.frame_rate().min(FPS)), !covers(f), if covers(f) { area } else { u32::MAX - area }, format(f))
        })
        .copied()
}

fn open(device: &str) -> anyhow::Result<Camera> {
    let cams = nokhwa::query(ApiBackend::MediaFoundation)?;
    let info = cams
        .iter()
        .find(|c| c.human_name() == device)
        .or_else(|| cams.first())
        .ok_or_else(|| anyhow::anyhow!(tr!("No camera found.", "Nenhuma câmera encontrada.")))?;
    // nokhwa's own "closest" match gives up when the camera lacks the exact size asked for, and
    // its fallback is the biggest picture there is; the mode is picked here instead.
    let mut cam = Camera::new(info.index().clone(), RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate))?;
    let mut mode = cam.camera_format();
    if let Some(best) = cam.compatible_camera_formats().ok().and_then(|f| best_format(&f)) {
        match cam.set_camera_requset(RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(best))) {
            Ok(_) => mode = best,
            Err(e) => log::info!("camera: {best} refused: {e}"),
        }
    }
    log::info!("camera: {}x{} {:?} at {} fps", mode.width(), mode.height(), mode.format(), mode.frame_rate());
    cam.open_stream()?;
    Ok(cam)
}

/// The centre of a w × h picture in the shape of the frame sent, as (x, y, width, height), on
/// even pixels.
fn crop_rect(w: u32, h: u32) -> (u32, u32, u32, u32) {
    if w * HEIGHT > h * WIDTH {
        let cw = (h * WIDTH / HEIGHT) & !1;
        (((w - cw) / 2) & !1, 0, cw, h)
    } else {
        let ch = (w * HEIGHT / WIDTH) & !1;
        (0, ((h - ch) / 2) & !1, w, ch)
    }
}

/// Turns the device's frames into the frame sent, keeping its buffers from frame to frame.
#[derive(Default)]
struct Converter {
    /// A whole MJPEG frame: libyuv decodes JPEG only whole, so it is cropped after.
    full: Option<I420Buffer>,
    /// The crop of an uncompressed frame, when it still has to be scaled.
    crop: Option<I420Buffer>,
    /// A JPEG's crop at the size sent, still in JPEG's full range.
    wide: Option<I420Buffer>,
}

impl Converter {
    /// The frame centre-cropped to 16:9 and scaled to `WIDTH` × `HEIGHT`, as I420.
    fn convert(&mut self, frame: &nokhwa::Buffer) -> anyhow::Result<I420Buffer> {
        let res = frame.resolution();
        let (w, h) = (res.width(), res.height());
        let rect = crop_rect(w, h);
        let (_, _, cw, ch) = rect;
        anyhow::ensure!(cw >= 2 && ch >= 2, "{w}x{h} frame");
        let data = frame.buffer();
        // A new buffer every frame: the encoder reads it after it is sent.
        let mut out = I420Buffer::new(WIDTH, HEIGHT);
        let packed = match frame.source_frame_format() {
            FrameFormat::MJPEG => {
                let full = yuv::pooled(&mut self.full, w, h);
                if !decode_jpeg(data, full)? {
                    yuv::scale_rect(full, rect, &mut out);
                    return Ok(out);
                }
                let wide = if (w, h) == (WIDTH, HEIGHT) {
                    full
                } else {
                    let wide = yuv::pooled(&mut self.wide, WIDTH, HEIGHT);
                    yuv::scale_rect(full, rect, wide);
                    wide
                };
                yuv::narrow_range(wide, &mut out);
                return Ok(out);
            }
            FrameFormat::NV12 => Packed::Nv12,
            FrameFormat::YUYV => Packed::Yuy2,
            FrameFormat::RAWRGB => Packed::Raw,
            FrameFormat::RAWBGR => Packed::Rgb24,
            FrameFormat::GRAY => Packed::Gray,
        };
        if (cw, ch) == (WIDTH, HEIGHT) {
            yuv::convert(data, packed, w, h, rect, &mut out)?;
        } else {
            let crop = yuv::pooled(&mut self.crop, cw, ch);
            yuv::convert(data, packed, w, h, rect, crop)?;
            yuv::scale(crop, &mut out);
        }
        Ok(out)
    }
}

/// A whole MJPEG frame into `dst`, its size; whether it came out in JPEG's full range.
fn decode_jpeg(data: &[u8], dst: &mut I420Buffer) -> anyhow::Result<bool> {
    let (w, h) = (dst.width(), dst.height());
    match yuv::mjpeg(data, w, h, dst) {
        Ok(()) => Ok(true),
        Err(e) => {
            // The slower decoder takes a few JPEGs libjpeg-turbo turns down.
            log::debug!("camera: {e}; decoding it with image");
            let rgba = image::load_from_memory_with_format(data, image::ImageFormat::Jpeg)?.to_rgba8();
            anyhow::ensure!(rgba.dimensions() == (w, h), "bad frame");
            rgba_into(&rgba, dst);
            Ok(false)
        }
    }
}

/// Counts a bad frame: fine now and then, the end of the camera when they keep coming.
fn strike(bad: &mut u32, e: anyhow::Error) -> anyhow::Result<()> {
    *bad += 1;
    if *bad >= MAX_BAD_FRAMES {
        return Err(e.context(format!("{MAX_BAD_FRAMES} bad frames in a row")));
    }
    log::debug!("camera: skipped a bad frame: {e}");
    Ok(())
}

/// Lets frames through at no more than `FPS` a second, evenly: a camera at 60 sends every other
/// frame, one at 30 or less sends them all, however unevenly they arrive.
struct Pacer {
    interval: Duration,
    next: Option<Duration>,
}

impl Pacer {
    fn new(fps: u32) -> Pacer {
        Pacer { interval: Duration::from_secs(1) / fps, next: None }
    }

    /// Whether a frame that arrived at `t` goes out.
    fn admit(&mut self, t: Duration) -> bool {
        // A frame a little early still counts, so jitter doesn't cost frames.
        let slack = self.interval / 4;
        match self.next {
            Some(next) if t + slack < next => false,
            Some(next) if t < next + self.interval => {
                self.next = Some(next + self.interval);
                true
            }
            _ => {
                self.next = Some(t + self.interval);
                true
            }
        }
    }
}

/// The person-finding model on a thread of its own, so its time never holds up the camera: it
/// is handed a frame when it is free, and leaves the newest mask behind with the time it took.
struct Masker {
    frames: SyncSender<Vec<u8>>,
    /// The model is waiting for a frame, so one offered now is taken.
    idle: Arc<AtomicBool>,
    /// The last frame the model was handed, back for the next to be copied into.
    spare: Arc<Mutex<Option<Vec<u8>>>>,
    mask: Arc<Mutex<Option<Timed>>>,
}

/// A mask and how long the model took for it.
type Timed = (Vec<f32>, Duration);

impl Masker {
    fn start(seg: &'static Segmenter) -> std::io::Result<Masker> {
        // No room for a frame: one is only taken while the model is waiting for it.
        let (frames, rx) = sync_channel::<Vec<u8>>(0);
        let idle = Arc::new(AtomicBool::new(false));
        let spare = Arc::new(Mutex::new(None));
        let mask = Arc::new(Mutex::new(None));
        let (out, waiting, returned) = (mask.clone(), idle.clone(), spare.clone());
        std::thread::Builder::new().name("harmony-segment".into()).spawn(move || {
            loop {
                waiting.store(true, Ordering::Release);
                let Ok(rgba) = rx.recv() else { break };
                waiting.store(false, Ordering::Relaxed);
                let t = Instant::now();
                match seg.mask(&rgba, WIDTH as usize, HEIGHT as usize) {
                    Ok(m) => *out.lock() = Some((m, t.elapsed())),
                    Err(e) => log::debug!("segmentation: {e}"),
                }
                *returned.lock() = Some(rgba);
            }
        })?;
        Ok(Masker { frames, idle, spare, mask })
    }

    /// Hands the model a frame unless it is still busy with the last one, copying it only then.
    fn offer(&self, rgba: &[u8]) -> bool {
        if !self.idle.load(Ordering::Acquire) {
            return false;
        }
        let mut frame = self.spare.lock().take().unwrap_or_default();
        frame.clear();
        frame.extend_from_slice(rgba);
        match self.frames.try_send(frame) {
            Ok(()) => true,
            // Not quite back at its wait yet.
            Err(TrySendError::Full(frame) | TrySendError::Disconnected(frame)) => {
                *self.spare.lock() = Some(frame);
                false
            }
        }
    }

    fn take(&self) -> Option<Timed> {
        self.mask.lock().take()
    }
}

/// With `HARMONY_VIDEO_STATS` set: frames from the camera and frames sent, and where the time
/// per frame goes, logged every few seconds.
struct Meter {
    since: Instant,
    got: u32,
    sent: u32,
    stages: [(Duration, u32); STAGES.len()],
}

const STAGES: [&str; 5] = ["decode", "segment", "composite", "convert", "preview"];

impl Meter {
    fn new() -> Meter {
        Meter { since: Instant::now(), got: 0, sent: 0, stages: Default::default() }
    }

    fn add(&mut self, stage: usize, d: Duration) {
        self.stages[stage].0 += d;
        self.stages[stage].1 += 1;
    }

    fn time<T>(&mut self, stage: usize, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let out = f();
        self.add(stage, t.elapsed());
        out
    }

    fn tick(&mut self) {
        let secs = self.since.elapsed().as_secs_f64();
        if secs < 5. {
            return;
        }
        if super::rtc::video_stats() {
            let stages: Vec<String> = STAGES
                .iter()
                .zip(&self.stages)
                .filter(|(_, (_, n))| *n > 0)
                .map(|(name, (d, n))| format!("{name} {:.1} ms ×{n}", d.as_secs_f64() * 1000. / *n as f64))
                .collect();
            log::info!("camera: {:.1} fps in, {:.1} fps sent; {}", self.got as f64 / secs, self.sent as f64 / secs, stages.join(", "));
        }
        *self = Meter::new();
    }
}

/// Held by the capture that has a camera open. Media Foundation fails or blocks when a device is
/// opened twice, so a new capture waits here for the last one to let go: a preview handing over to
/// the call, or a switch to another camera.
static DEVICE: Mutex<()> = parking_lot::const_mutex(());

fn run(
    device: &str,
    source: NativeVideoSource,
    preview: Arc<FrameSlot>,
    stop: Arc<AtomicBool>,
    background: Arc<Mutex<Background>>,
) -> anyhow::Result<()> {
    let Some(_device) = DEVICE.try_lock_for(Duration::from_secs(5)) else {
        anyhow::bail!(tr!("The camera is still in use.", "A câmera ainda está em uso."));
    };
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let mut cam = open(device)?;
    let mut converter = Converter::default();
    let mut compositor = Compositor::new();
    let mut masker: Option<Masker> = None;
    let mut offered = Instant::now() - MASK_EVERY;
    let mut picture_for: Option<(Background, Vec<u8>)> = None;
    // Only filled while a background is replaced.
    let mut rgba = Vec::new();
    let mut frames = video::Scratch::default();
    let mut pacer = Pacer::new(FPS);
    let mut meter = Meter::new();
    let started = Instant::now();
    let mut bad = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let frame = match cam.frame() {
            Ok(f) => f,
            Err(e) => {
                strike(&mut bad, e.into())?;
                std::thread::sleep(pacer.interval);
                continue;
            }
        };
        meter.got += 1;
        meter.tick();
        if !pacer.admit(started.elapsed()) {
            continue;
        }
        let mut i420 = match meter.time(0, || converter.convert(&frame)) {
            Ok(b) => b,
            Err(e) => {
                strike(&mut bad, e)?;
                continue;
            }
        };
        bad = 0;

        let bg = background.lock().clone();
        let seg = segmenter().filter(|_| !matches!(bg, Background::None));
        if let Some(seg) = seg {
            // The model and the blend work in RGBA; the frame goes there and back only now.
            meter.time(3, || i420_to_rgba(&i420, &mut rgba));
            let (mw, mh) = seg.input_size();
            if !compositor.has_mask() {
                // The first frame waits for its mask, so the room never shows unreplaced.
                if let Ok(mask) = meter.time(1, || seg.mask(&rgba, WIDTH as usize, HEIGHT as usize)) {
                    compositor.update_mask(&mask, mw, mh);
                }
            } else if let Some((mask, took)) = masker.as_ref().and_then(Masker::take) {
                meter.add(1, took);
                compositor.update_mask(&mask, mw, mh);
            }
            if offered.elapsed() >= MASK_EVERY {
                if masker.is_none() {
                    masker = Some(Masker::start(seg)?);
                }
                if masker.as_ref().is_some_and(|m| m.offer(&rgba)) {
                    offered = Instant::now();
                }
            }
            let backdrop = match &bg {
                Background::Blur { strength } => Some(Backdrop::Blur((*strength).clamp(2, 30))),
                other => {
                    if picture_for.as_ref().is_none_or(|p| &p.0 != other) {
                        picture_for = picture(other, WIDTH, HEIGHT).map(|p| (other.clone(), p));
                    }
                    picture_for.as_ref().map(|p| Backdrop::Image(&p.1))
                }
            };
            if let Some(b) = backdrop {
                meter.time(2, || compositor.apply(&mut rgba, WIDTH as usize, HEIGHT as usize, &b));
                // Not sent yet, so it is still this loop's to write.
                meter.time(3, || rgba_into(&rgba, &mut i420));
            }
        } else if compositor.has_mask() {
            // A mask kept from before would show the wrong outline when a background comes back.
            compositor = Compositor::new();
            masker = None;
            rgba = Vec::new();
        }

        let mut vf = VideoFrame::new(VideoRotation::VideoRotation0, i420);
        vf.timestamp_us = started.elapsed().as_micros() as i64;
        source.capture_frame(&vf);
        meter.sent += 1;
        // The encoder only reads the frame, so the preview can be made from it meanwhile.
        if !video::window_hidden() {
            let frame = meter.time(4, || preview_frame(&vf.buffer, preview.id, &mut frames));
            preview.put(frame);
        }
    }
    Ok(())
}

/// RGBA of `dst`'s size into it, as I420 (BT.601, limited range), as encoders expect.
fn rgba_into(rgba: &[u8], dst: &mut I420Buffer) {
    let (w, h) = (dst.width(), dst.height());
    let (sy, su, sv) = dst.strides();
    let (y, u, v) = dst.data_mut();
    // libyuv's ABGR is R, G, B, A in memory.
    yuv_helper::abgr_to_i420(rgba, w * 4, y, sy, u, su, v, sv, w as i32, h as i32);
}

fn i420_to_rgba(src: &I420Buffer, rgba: &mut Vec<u8>) {
    let (w, h) = (src.width(), src.height());
    rgba.resize((w * h * 4) as usize, 0);
    let (sy, su, sv) = src.strides();
    let (y, u, v) = src.data();
    yuv_helper::i420_to_abgr(y, sy, u, su, v, sv, rgba, w * 4, w as i32, h as i32);
}

/// Your own view, as stream `id` of the window, which draws it mirrored as a mirror is.
fn preview_frame(src: &I420Buffer, id: u64, frames: &mut video::Scratch) -> SurfaceFrame {
    frames.of_i420(id, src).mirrored(true)
}

/// A still of a background, for the picker.
pub fn thumbnail(bg: &Background) -> Option<Arc<gpui::Image>> {
    let bytes = match bg {
        Background::Builtin { name } => BUILTIN.iter().find(|b| b.0 == name)?.2.to_vec(),
        Background::Image { path } => std::fs::read(path).ok()?,
        _ => return None,
    };
    let fmt = if bytes.starts_with(&[0x89, b'P']) { gpui::ImageFormat::Png } else { gpui::ImageFormat::Jpeg };
    Some(Arc::new(gpui::Image::from_bytes(fmt, bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_to_i420(rgba: &[u8], w: u32, h: u32) -> I420Buffer {
        let mut buf = I420Buffer::new(w, h);
        rgba_into(rgba, &mut buf);
        buf
    }

    #[test]
    fn grey_converts_to_grey() {
        let rgba = vec![128u8; 4 * 2 * 4];
        let buf = rgba_to_i420(&rgba, 4, 2);
        let (y, u, v) = buf.data();
        assert!(y.iter().all(|&p| (125..=127).contains(&p)), "{y:?}");
        assert!(u.iter().chain(v).all(|&p| p == 128));
    }

    #[test]
    fn every_builtin_background_decodes() {
        for (name, _, _) in BUILTIN {
            assert!(picture(&Background::Builtin { name: name.into() }, 64, 36).is_some_and(|p| p.len() == 64 * 36 * 4), "{name}");
        }
    }

    fn mode(format: FrameFormat, w: u32, h: u32, fps: u32) -> CameraFormat {
        CameraFormat::new(nokhwa::utils::Resolution::new(w, h), format, fps)
    }

    #[test]
    fn the_smallest_full_rate_mode_that_covers_the_frame_wins() {
        // What a cheap USB camera lists: nothing at 640 × 360, 25 and 30 fps everywhere.
        let mut modes = Vec::new();
        for format in [FrameFormat::MJPEG, FrameFormat::NV12] {
            for (w, h) in [(640, 320), (640, 480), (1280, 720), (1920, 1080)] {
                for fps in [25, 30] {
                    modes.push(mode(format, w, h, fps));
                }
            }
        }
        assert_eq!(best_format(&modes), Some(mode(FrameFormat::NV12, 640, 480, 30)));
        // The frame rate counts before the size: 720p at 30 beats 480p at 15.
        let modes = [mode(FrameFormat::YUYV, 640, 480, 15), mode(FrameFormat::MJPEG, 1280, 720, 30)];
        assert_eq!(best_format(&modes), Some(mode(FrameFormat::MJPEG, 1280, 720, 30)));
        // Faster than sent buys nothing, so the smaller picture wins.
        let modes = [mode(FrameFormat::MJPEG, 1280, 720, 60), mode(FrameFormat::MJPEG, 640, 360, 30)];
        assert_eq!(best_format(&modes), Some(mode(FrameFormat::MJPEG, 640, 360, 30)));
        // Nothing big enough: the biggest there is.
        let modes = [mode(FrameFormat::MJPEG, 320, 240, 30), mode(FrameFormat::MJPEG, 424, 240, 30)];
        assert_eq!(best_format(&modes), Some(mode(FrameFormat::MJPEG, 424, 240, 30)));
        assert_eq!(best_format(&[]), None);
    }

    #[test]
    fn frames_are_cropped_to_the_middle_in_16_by_9() {
        assert_eq!(crop_rect(640, 480), (0, 60, 640, 360));
        assert_eq!(crop_rect(1920, 1080), (0, 0, 1920, 1080));
        assert_eq!(crop_rect(1280, 1024), (0, 152, 1280, 720));
        assert_eq!(crop_rect(1000, 360), (180, 0, 640, 360));
    }

    #[test]
    fn a_4_by_3_frame_keeps_its_middle_rows() {
        // NV12 whose every row is lit by its own number: the frame sent starts 60 rows down.
        let (w, h) = (640u32, 480u32);
        let mut nv12: Vec<u8> = (0..h).flat_map(|y| std::iter::repeat_n(y as u8, w as usize)).collect();
        nv12.resize(Packed::Nv12.size(w, h), 128);
        let out = Converter::default().convert(&frame(w, h, &nv12, FrameFormat::NV12)).unwrap();
        assert_eq!((out.width(), out.height()), (WIDTH, HEIGHT));
        let (y, _, _) = out.data();
        assert_eq!(y[0], 60);
        assert_eq!(y[(HEIGHT - 1) as usize * out.strides().0 as usize], (60 + HEIGHT - 1) as u8);
    }

    fn frame(w: u32, h: u32, data: &[u8], format: FrameFormat) -> nokhwa::Buffer {
        nokhwa::Buffer::new(nokhwa::utils::Resolution::new(w, h), data, format)
    }

    #[test]
    fn every_format_is_cropped_and_scaled_with_its_colour() {
        let (w, h) = (1280u32, 960u32);
        let colour = [200u8, 120, 40];
        let i420 = rgba_to_i420(&[colour[0], colour[1], colour[2], 255].repeat((w * h) as usize), w, h);
        let (y, u, v) = (i420.data().0[0], i420.data().1[0], i420.data().2[0]);
        let mut nv12 = vec![y; (w * h) as usize];
        nv12.extend([u, v].repeat((w * h / 4) as usize));
        let rgb = colour.repeat((w * h) as usize);
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95).encode(&rgb, w, h, image::ExtendedColorType::Rgb8).unwrap();
        let frames = [
            (FrameFormat::NV12, nv12),
            (FrameFormat::YUYV, [y, u, y, v].repeat((w * h / 2) as usize)),
            (FrameFormat::RAWRGB, rgb.clone()),
            (FrameFormat::RAWBGR, [colour[2], colour[1], colour[0]].repeat((w * h) as usize)),
            (FrameFormat::MJPEG, jpeg),
        ];
        let mut converter = Converter::default();
        for (format, data) in frames {
            // Twice, so the second runs on kept buffers.
            for _ in 0..2 {
                let out = converter.convert(&frame(w, h, &data, format)).unwrap();
                assert_eq!((out.width(), out.height()), (WIDTH, HEIGHT), "{format:?}");
                let mut px = Vec::new();
                i420_to_rgba(&out, &mut px);
                let p = &px[(WIDTH * 100 + 300) as usize * 4..][..4];
                let near = p.iter().zip(colour).all(|(a, b)| a.abs_diff(b) <= 4) && p[3] == 255;
                assert!(near, "{format:?}: {p:?}");
            }
        }
        let short = frame(w, h, &[0; 100], FrameFormat::NV12);
        assert!(converter.convert(&short).is_err());
    }

    #[test]
    fn the_preview_is_the_frame_sent_drawn_mirrored() {
        // White on the left of the frame sent: the preview holds it as sent, for the GPU to flip.
        let rgba: Vec<u8> = (0..WIDTH * HEIGHT).flat_map(|i| if i % WIDTH < WIDTH / 2 { [255; 4] } else { [0, 0, 0, 255] }).collect();
        let sent = rgba_to_i420(&rgba, WIDTH, HEIGHT);
        let frame = preview_frame(&sent, 7, &mut video::Scratch::default());
        assert!(frame.is_mirrored());
        assert_eq!((frame.id(), frame.width(), frame.height()), (7, WIDTH, HEIGHT));
        let (y, stride) = frame.plane(0);
        let at = |x: usize| y[stride * 10 + x];
        assert!(at(10) > 225 && at(WIDTH as usize - 10) < 30, "{} {}", at(10), at(WIDTH as usize - 10));
    }

    /// Frames at `fps` with a few milliseconds of jitter, through a pacer at `FPS`, for 10 s.
    fn paced(fps: f64) -> u32 {
        let mut pacer = Pacer::new(FPS);
        (0..(fps * 10.) as u32)
            .filter(|&i| {
                let jitter = [0., 4., -3., 6., -5., 2.][i as usize % 6] / 1000.;
                pacer.admit(Duration::from_secs_f64((i as f64 / fps + jitter).max(0.)))
            })
            .count() as u32
    }

    #[test]
    fn the_pacer_keeps_every_frame_up_to_its_rate_and_halves_double() {
        assert_eq!(paced(30.), 300);
        assert_eq!(paced(16.7), 167);
        assert!((295..=305).contains(&paced(60.)), "{}", paced(60.));
    }
}
