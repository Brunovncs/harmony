//! Your camera: frames from the device (as RGBA), the background replaced if you asked for it,
//! then into a WebRTC video source (I420, by libyuv) and, mirrored, into your own preview.

use super::background::{Backdrop, Compositor, Segmenter};
use super::video::FrameSlot;
use crate::core::settings::Background;
use gpui::RenderImage;
use libwebrtc::native::yuv_helper;
use libwebrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use libwebrtc::video_source::VideoResolution;
use libwebrtc::video_source::native::NativeVideoSource;
use nokhwa::Camera;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{ApiBackend, CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType};
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::cmp::Reverse;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
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
/// The person-finding model at most this often (15 to 20 times a second); the mask is smoothed
/// in between.
const MASK_EVERY: Duration = Duration::from_millis(50);

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
/// that still covers the frame sent (or else the largest), then the format cheapest to carry
/// and decode.
fn best_format(formats: &[CameraFormat]) -> Option<CameraFormat> {
    let covers = |f: &CameraFormat| f.width() >= WIDTH && f.height() >= HEIGHT;
    let format = |f: &CameraFormat| match f.format() {
        FrameFormat::MJPEG => 0,
        FrameFormat::NV12 => 1,
        FrameFormat::YUYV => 2,
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

/// The frame as tightly packed RGBA.
fn decode(frame: &nokhwa::Buffer) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let res = frame.resolution();
    let (w, h) = (res.width(), res.height());
    let data = frame.buffer();
    let rgb = |rgb: &[u8]| rgb.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect::<Vec<u8>>();
    let rgba = match frame.source_frame_format() {
        FrameFormat::MJPEG => image::load_from_memory_with_format(data, image::ImageFormat::Jpeg)?.to_rgba8().into_raw(),
        FrameFormat::NV12 => {
            let (y, uv) = data.split_at_checked((w * h) as usize).ok_or_else(|| anyhow::anyhow!("short frame"))?;
            anyhow::ensure!(uv.len() >= (w * h / 2) as usize, "short frame");
            let mut out = vec![0u8; (w * h * 4) as usize];
            // libyuv's ABGR is R, G, B, A in memory.
            yuv_helper::nv12_to_abgr(y, w, uv, w, &mut out, w * 4, w as i32, h as i32);
            out
        }
        FrameFormat::YUYV => rgb(&nokhwa::utils::yuyv422_to_rgb(data, false)?),
        FrameFormat::RAWRGB => rgb(data),
        FrameFormat::RAWBGR => data.chunks_exact(3).flat_map(|p| [p[2], p[1], p[0], 255]).collect(),
        FrameFormat::GRAY => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
    };
    anyhow::ensure!(rgba.len() == (w * h * 4) as usize, "bad frame");
    Ok((rgba, w, h))
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

/// To 640 × 360: centre-cropped to 16:9, then scaled (by libyuv, through I420) if need be.
fn to_size(rgba: Vec<u8>, w: u32, h: u32) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(rgba.len() == (w * h * 4) as usize, "bad frame");
    if (w, h) == (WIDTH, HEIGHT) {
        return Ok(rgba);
    }
    let (x, y, cw, ch) = crop_rect(w, h);
    let (row, x, cw4) = ((w * 4) as usize, (x * 4) as usize, (cw * 4) as usize);
    let mut crop = Vec::with_capacity(cw4 * ch as usize);
    for line in rgba.chunks_exact(row).skip(y as usize).take(ch as usize) {
        crop.extend_from_slice(&line[x..x + cw4]);
    }
    if (cw, ch) == (WIDTH, HEIGHT) {
        return Ok(crop);
    }
    let scaled = rgba_to_i420(&crop, cw, ch).scale(WIDTH as i32, HEIGHT as i32);
    let (sy, su, sv) = scaled.strides();
    let (py, pu, pv) = scaled.data();
    let mut out = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    yuv_helper::i420_to_abgr(py, sy, pu, su, pv, sv, &mut out, WIDTH * 4, WIDTH as i32, HEIGHT as i32);
    Ok(out)
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
    mask: Arc<Mutex<Option<Timed>>>,
}

/// A mask and how long the model took for it.
type Timed = (Vec<f32>, Duration);

impl Masker {
    fn start(seg: &'static Segmenter) -> std::io::Result<Masker> {
        // No room for a frame: one is only taken while the model is waiting for it.
        let (frames, rx) = sync_channel::<Vec<u8>>(0);
        let mask = Arc::new(Mutex::new(None));
        let out = mask.clone();
        std::thread::Builder::new().name("harmony-segment".into()).spawn(move || {
            for rgba in rx {
                let t = Instant::now();
                match seg.mask(&rgba, WIDTH as usize, HEIGHT as usize) {
                    Ok(m) => *out.lock() = Some((m, t.elapsed())),
                    Err(e) => log::debug!("segmentation: {e}"),
                }
            }
        })?;
        Ok(Masker { frames, mask })
    }

    /// Hands the model a frame unless it is still busy with the last one.
    fn offer(&self, rgba: &[u8]) -> bool {
        self.frames.try_send(rgba.to_vec()).is_ok()
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

const STAGES: [&str; 4] = ["decode", "segment", "composite", "convert"];

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
    let mut compositor = Compositor::new();
    let mut masker: Option<Masker> = None;
    let mut offered = Instant::now() - MASK_EVERY;
    let mut picture_for: Option<(Background, Vec<u8>)> = None;
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
        let mut rgba = match meter.time(0, || decode(&frame).and_then(|(px, w, h)| to_size(px, w, h))) {
            Ok(px) => px,
            Err(e) => {
                strike(&mut bad, e)?;
                continue;
            }
        };
        bad = 0;

        let bg = background.lock().clone();
        let seg = segmenter().filter(|_| !matches!(bg, Background::None));
        if let Some(seg) = seg {
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
            }
        } else if compositor.has_mask() {
            // A mask kept from before would show the wrong outline when a background comes back.
            compositor = Compositor::new();
            masker = None;
        }

        let mut vf = VideoFrame::new(VideoRotation::VideoRotation0, meter.time(3, || rgba_to_i420(&rgba, WIDTH, HEIGHT)));
        vf.timestamp_us = started.elapsed().as_micros() as i64;
        source.capture_frame(&vf);
        meter.sent += 1;
        preview.put(mirrored_bgra(&rgba, WIDTH, HEIGHT), WIDTH, HEIGHT);
    }
    Ok(())
}

/// RGBA to I420 (BT.601, limited range), as encoders expect.
pub fn rgba_to_i420(rgba: &[u8], w: u32, h: u32) -> I420Buffer {
    let mut buf = I420Buffer::new(w, h);
    let (sy, su, sv) = buf.strides();
    let (y, u, v) = buf.data_mut();
    // libyuv's ABGR is R, G, B, A in memory.
    yuv_helper::abgr_to_i420(rgba, w * 4, y, sy, u, su, v, sv, w as i32, h as i32);
    buf
}

/// Your own view, mirrored as a mirror is, in GPUI's BGRA.
fn mirrored_bgra(rgba: &[u8], w: u32, h: u32) -> Arc<RenderImage> {
    let row = w as usize * 4;
    let mut out = vec![0u8; rgba.len()];
    for (src, dst) in rgba.chunks_exact(row).zip(out.chunks_exact_mut(row)) {
        for (s, d) in src.chunks_exact(4).rev().zip(dst.chunks_exact_mut(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], 255]);
        }
    }
    let img = image::RgbaImage::from_raw(w, h, out).expect("frame size");
    Arc::new(RenderImage::new(SmallVec::from_elem(image::Frame::new(img), 1)))
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
        assert_eq!(best_format(&modes), Some(mode(FrameFormat::MJPEG, 640, 480, 30)));
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
        let (w, h) = (640u32, 480u32);
        let rgba: Vec<u8> = (0..h).flat_map(|y| (0..w).flat_map(move |_| [y as u8, 0, 0, 255])).collect();
        let out = to_size(rgba, w, h).unwrap();
        assert_eq!(out.len(), (WIDTH * HEIGHT * 4) as usize);
        assert_eq!(out[0], 60);
        assert_eq!(out[out.len() - 4], (60 + HEIGHT - 1) as u8);
    }

    #[test]
    fn a_big_frame_is_scaled_down_with_its_colour() {
        let (w, h) = (1280u32, 720u32);
        let rgba = [200u8, 120, 40, 255].repeat((w * h) as usize);
        let out = to_size(rgba, w, h).unwrap();
        assert_eq!(out.len(), (WIDTH * HEIGHT * 4) as usize);
        let p = &out[(WIDTH * 100 + 300) as usize * 4..][..4];
        assert!(p[0].abs_diff(200) <= 3 && p[1].abs_diff(120) <= 3 && p[2].abs_diff(40) <= 3 && p[3] == 255, "{p:?}");
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
