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
use nokhwa::utils::{ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution};
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// What a camera sends, as the old client did: small, and steady rather than sharp.
pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 360;
pub const FPS: u32 = 24;
pub const BITRATE: u64 = 400_000;
/// Bad frames in a row before the camera counts as gone.
const MAX_BAD_FRAMES: u32 = 30;

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

fn open(device: &str) -> anyhow::Result<Camera> {
    let cams = nokhwa::query(ApiBackend::MediaFoundation)?;
    let info = cams
        .iter()
        .find(|c| c.human_name() == device)
        .or_else(|| cams.first())
        .ok_or_else(|| anyhow::anyhow!(tr!("No camera found.", "Nenhuma câmera encontrada.")))?;
    let index = info.index().clone();
    let want =
        |fmt| RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(CameraFormat::new(Resolution::new(WIDTH, HEIGHT), fmt, 30)));
    let mut cam = Camera::new(index.clone(), want(FrameFormat::MJPEG))
        .or_else(|_| Camera::new(index.clone(), want(FrameFormat::NV12)))
        .or_else(|_| Camera::new(index, RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate)))?;
    cam.open_stream()?;
    let _ = CameraIndex::Index(0);
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

/// To 640 × 360: centre-cropped to 16:9, then scaled.
fn to_size(rgba: Vec<u8>, w: u32, h: u32) -> anyhow::Result<Vec<u8>> {
    let img = image::RgbaImage::from_raw(w, h, rgba).ok_or_else(|| anyhow::anyhow!("bad frame"))?;
    Ok(if (w, h) == (WIDTH, HEIGHT) {
        img.into_raw()
    } else {
        image::DynamicImage::ImageRgba8(img).resize_to_fill(WIDTH, HEIGHT, image::imageops::FilterType::Triangle).to_rgba8().into_raw()
    })
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
    let mut picture_for: Option<(Background, u32, u32, Vec<u8>)> = None;
    let started = Instant::now();
    let frame_time = Duration::from_secs_f64(1. / FPS as f64);
    let mut last = Instant::now() - frame_time;
    let mut n = 0u64;
    let mut bad = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let frame = match cam.frame() {
            Ok(f) => f,
            Err(e) => {
                strike(&mut bad, e.into())?;
                std::thread::sleep(frame_time);
                continue;
            }
        };
        if last.elapsed() < frame_time.mul_f32(0.85) {
            continue;
        }
        last = Instant::now();
        let mut rgba = match decode(&frame).and_then(|(px, w, h)| to_size(px, w, h)) {
            Ok(px) => px,
            Err(e) => {
                strike(&mut bad, e)?;
                continue;
            }
        };
        bad = 0;

        let bg = background.lock().clone();
        if !matches!(bg, Background::None)
            && let Some(seg) = segmenter()
        {
            // The model every other frame is plenty; the mask is smoothed in between.
            if n.is_multiple_of(2)
                && let Ok(mask) = seg.mask(&rgba, WIDTH as usize, HEIGHT as usize)
            {
                let (mw, mh) = seg.input_size();
                compositor.update_mask(&mask, mw, mh);
            }
            let backdrop = match &bg {
                Background::Blur { strength } => Some(Backdrop::Blur((*strength).clamp(2, 30))),
                other => {
                    if picture_for.as_ref().is_none_or(|p| &p.0 != other) {
                        picture_for = picture(other, WIDTH, HEIGHT).map(|p| (other.clone(), WIDTH, HEIGHT, p));
                    }
                    picture_for.as_ref().map(|p| Backdrop::Image(&p.3))
                }
            };
            if let Some(b) = backdrop {
                compositor.apply(&mut rgba, WIDTH as usize, HEIGHT as usize, &b);
            }
        }

        let mut vf = VideoFrame::new(VideoRotation::VideoRotation0, rgba_to_i420(&rgba, WIDTH, HEIGHT));
        vf.timestamp_us = started.elapsed().as_micros() as i64;
        source.capture_frame(&vf);
        // Your own view at half the frame rate is plenty, and halves the pictures handed to the
        // window.
        if n.is_multiple_of(2) {
            preview.put(mirrored_bgra(&rgba, WIDTH, HEIGHT), WIDTH, HEIGHT);
        }
        n += 1;
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
}
