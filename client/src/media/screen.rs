//! Your screen or a window, captured by WebRTC's desktop capturer, scaled to the resolution
//! picked, turned into I420 (or NV12) for the encoder, and previewed small (drawn by the GPU). Its
//! sound comes from `loopback`.

use super::loopback::{self, Loopback};
use super::rtc::VideoParams;
use super::video::{self, FrameSlot};
use super::yuv;
use gpui::{RenderImage, SurfaceFrame};
use libwebrtc::desktop_capturer::{DesktopCaptureSourceType, DesktopCapturer, DesktopCapturerOptions, DesktopFrame};
use libwebrtc::native::yuv_helper;
use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use libwebrtc::prelude::VideoBuffer;
use libwebrtc::video_frame::{I420Buffer, NV12Buffer, VideoFrame, VideoRotation};
use libwebrtc::video_source::VideoResolution;
use libwebrtc::video_source::native::NativeVideoSource;
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Screen,
    Window,
}

#[derive(Clone)]
pub struct ScreenSource {
    pub id: u64,
    pub kind: SourceKind,
    pub title: String,
    /// Known once a still of it has been taken.
    pub size: Option<(u32, u32)>,
}

/// A small picture of a source, BGRA as GPUI draws it, and the source's own size.
pub struct Still {
    pub image: Arc<RenderImage>,
    pub size: (u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    P480,
    P720,
    P1080,
    Native,
}

impl Resolution {
    pub fn from_setting(s: &str) -> Resolution {
        match s {
            "480" => Resolution::P480,
            "720" => Resolution::P720,
            "native" => Resolution::Native,
            _ => Resolution::P1080,
        }
    }

    pub fn setting(self) -> &'static str {
        match self {
            Resolution::P480 => "480",
            Resolution::P720 => "720",
            Resolution::P1080 => "1080",
            Resolution::Native => "native",
        }
    }

    fn height(self) -> Option<u32> {
        match self {
            Resolution::P480 => Some(480),
            Resolution::P720 => Some(720),
            Resolution::P1080 => Some(1080),
            Resolution::Native => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    /// What the computer plays, less Harmony itself.
    System,
    /// Only the shared window's application.
    App,
    Off,
}

#[derive(Clone)]
pub struct ScreenChoice {
    pub source: ScreenSource,
    pub resolution: Resolution,
    pub fps: u32,
    pub sharp: bool,
    pub sound: Sound,
}

impl ScreenChoice {
    /// The old client's bitrate ceilings, in Mbit/s, by resolution and frame rate.
    pub fn params(&self) -> VideoParams {
        let mbps = match (self.resolution, self.fps) {
            (Resolution::P480, f) if f >= 120 => 4.,
            (Resolution::P480, f) if f >= 60 => 2.5,
            (Resolution::P480, _) => 1.5,
            (Resolution::P720, f) if f >= 120 => 8.,
            (Resolution::P720, f) if f >= 60 => 5.,
            (Resolution::P720, _) => 3.,
            (Resolution::P1080, f) if f >= 120 => 18.,
            (Resolution::P1080, f) if f >= 60 => 12.,
            (Resolution::P1080, _) => 8.,
            (Resolution::Native, f) if f >= 120 => 28.,
            (Resolution::Native, f) if f >= 60 => 20.,
            (Resolution::Native, _) => 15.,
        };
        VideoParams { max_bitrate: (mbps * 1_000_000.) as u64, max_fps: self.fps as f64, sharp: self.sharp }
    }
}

fn capturer(kind: SourceKind) -> Option<DesktopCapturer> {
    let mut opts = DesktopCapturerOptions::new(match kind {
        SourceKind::Screen => DesktopCaptureSourceType::Screen,
        SourceKind::Window => DesktopCaptureSourceType::Window,
    });
    opts.set_include_cursor(true);
    DesktopCapturer::new(opts)
}

/// Every screen or window that can be shared, at once: their stills come from `stills`.
pub fn sources(kind: SourceKind) -> Vec<ScreenSource> {
    let Some(list_capturer) = capturer(kind) else { return Vec::new() };
    let mut out = Vec::new();
    for (i, src) in list_capturer.get_source_list().into_iter().enumerate() {
        let title = match kind {
            SourceKind::Screen => trf!("Screen {}", "Tela {}", i + 1),
            SourceKind::Window => src.title(),
        };
        if kind == SourceKind::Window && (title.trim().is_empty() || title == "Harmony") {
            continue;
        }
        out.push(ScreenSource { id: src.id(), kind, title, size: None });
    }
    out
}

/// The longest a still may take. A minimized window never gives one, so it isn't waited on.
const STILL_LIMIT: Duration = Duration::from_millis(300);
const STILL_THREADS: usize = 8;

/// Takes stills of these sources a few at a time, each sent as it lands (`None` for a source
/// that gave no frame in time). Dropping the receiver stops the rest.
pub fn stills(kind: SourceKind, ids: Vec<u64>) -> async_channel::Receiver<(u64, Option<Still>)> {
    let (tx, rx) = async_channel::unbounded();
    let threads = STILL_THREADS.min(ids.len());
    let queue = Arc::new(Mutex::new(ids.into_iter()));
    for _ in 0..threads {
        let (tx, queue) = (tx.clone(), queue.clone());
        let work = move || {
            loop {
                let next = queue.lock().next();
                let Some(id) = next else { break };
                if tx.is_closed() || tx.send_blocking((id, still(kind, id))).is_err() {
                    break;
                }
            }
        };
        if let Err(e) = std::thread::Builder::new().name("harmony-still".into()).spawn(work) {
            log::warn!("no thread for screen stills: {e}");
        }
    }
    rx
}

/// One frame of a source, scaled down to 320 px wide inside the capturer's callback.
fn still(kind: SourceKind, id: u64) -> Option<Still> {
    let mut c = capturer(kind)?;
    let src = c.get_source_list().into_iter().find(|s| s.id() == id)?;
    let (tx, rx) = mpsc::channel();
    c.start_capture(Some(src), move |frame| {
        if let Ok(f) = frame {
            let (w, h) = (f.width() as u32, f.height() as u32);
            if w == 0 || h == 0 {
                return;
            }
            let _ = tx.send(Still { image: bgra_preview(f.data(), w, h, f.stride(), 320), size: (w, h) });
        }
    });
    let deadline = Instant::now() + STILL_LIMIT;
    loop {
        c.capture_frame();
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left.min(Duration::from_millis(50))) {
            Ok(still) => return Some(still),
            Err(_) if left.is_zero() => return None,
            Err(_) => {}
        }
    }
}
/// The newest captured frame, made into the encoder's frame inside the capturer's callback (no
/// copy of the BGRA), and the preview when one was asked for.
#[derive(Default)]
struct Staging {
    /// What the capture is sent at; set by the capture loop.
    resolution: Option<Resolution>,
    /// Whether the encoder takes NV12 as it is; set by the capture loop.
    nv12: bool,
    frame: Option<EncoderFrame>,
    scratch: Scratch,
    want_preview: bool,
    /// The preview's stream, for the window: its slot's id.
    preview_id: u64,
    preview_frames: video::Scratch,
    preview: Option<SurfaceFrame>,
}

/// Kept between frames so scaling doesn't allocate.
#[derive(Default)]
struct Scratch {
    bgra: Vec<u8>,
    full: Option<I420Buffer>,
}

impl Staging {
    fn take(&mut self, f: &DesktopFrame) {
        let (w, h) = (f.width() as u32, f.height() as u32);
        let (ew, eh) = (w & !1, h & !1);
        let Some(resolution) = self.resolution else { return };
        if ew == 0 || eh == 0 {
            return;
        }
        self.frame = Some(encoder_frame(f.data(), f.stride(), (ew, eh), fit(ew, eh, resolution), self.nv12, &mut self.scratch));
        if std::mem::take(&mut self.want_preview) {
            self.preview = Some(preview_frame(f.data(), w, h, f.stride(), self.preview_id, &mut self.preview_frames));
        }
    }
}

/// A frame for the encoder in the format that costs it least.
enum EncoderFrame {
    I420(I420Buffer),
    Nv12(NV12Buffer),
}

impl AsRef<dyn VideoBuffer> for EncoderFrame {
    fn as_ref(&self) -> &(dyn VideoBuffer + 'static) {
        match self {
            EncoderFrame::I420(b) => b,
            EncoderFrame::Nv12(b) => b,
        }
    }
}

/// The encoder's frame from `w` × `h` (even-sided) of a captured BGRA one, at the size sent,
/// converted once. Scaling goes first, in BGRA, except at 3/4 (1440p sent as 1080p), which libyuv
/// scales about twice as fast in I420 planes. OpenH264 works in I420. The graphics card's encoder
/// (`nv12`) works in NV12 and borrows an NV12 frame without copying it, so a frame sent at the
/// captured size is made NV12 for it, at the same cost as I420; a scaled one stays I420, since
/// libyuv scales NV12 slower than I420 plus the encoder's conversion (1.9 against 1.6 ms a frame
/// from 1440p to 1080p).
fn encoder_frame(bgra: &[u8], stride: u32, (w, h): (u32, u32), (tw, th): (u32, u32), nv12: bool, scratch: &mut Scratch) -> EncoderFrame {
    // Always a new buffer: the encoder reads it after `capture_frame` returns.
    if nv12 && (tw, th) == (w, h) {
        let mut out = NV12Buffer::new(tw, th);
        let (sy, suv) = out.strides();
        let (y, uv) = out.data_mut();
        yuv_helper::argb_to_nv12(bgra, stride, y, sy, uv, suv, w as i32, h as i32);
        return EncoderFrame::Nv12(out);
    }
    let mut out = I420Buffer::new(tw, th);
    let to_i420 = |src: &[u8], stride: u32, dst: &mut I420Buffer| {
        let (dw, dh) = (dst.width(), dst.height());
        let (sy, su, sv) = dst.strides();
        let (y, u, v) = dst.data_mut();
        yuv_helper::argb_to_i420(src, stride, y, sy, u, su, v, sv, dw as i32, dh as i32);
    };
    if (tw, th) == (w, h) {
        to_i420(bgra, stride, &mut out);
    } else if tw * 4 == w * 3 && th * 4 == h * 3 {
        let full = yuv::pooled(&mut scratch.full, w, h);
        to_i420(bgra, stride, full);
        yuv::scale(full, &mut out);
    } else {
        scratch.bgra.resize((tw * th * 4) as usize, 0);
        yuv::scale_bgra(bgra, stride, w, h, &mut scratch.bgra, tw, th);
        to_i420(&scratch.bgra, tw * 4, &mut out);
    }
    EncoderFrame::I420(out)
}

enum Control {
    Switch(ScreenChoice),
}

pub struct ScreenCapture {
    stop: Arc<AtomicBool>,
    pub source: NativeVideoSource,
    pub preview: Arc<FrameSlot>,
    pub choice: Mutex<ScreenChoice>,
    control: mpsc::Sender<Control>,
    sound: Mutex<Option<Loopback>>,
    sound_source: Mutex<Option<libwebrtc::audio_source::native::NativeAudioSource>>,
}

impl ScreenCapture {
    pub fn start(choice: ScreenChoice) -> ScreenCapture {
        let (w, h) = target_size(&choice);
        let _rt = crate::core::runtime().enter();
        let source = NativeVideoSource::new(VideoResolution { width: w, height: h }, true);
        let preview = Arc::new(FrameSlot::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (src, prev, st, c) = (source.clone(), preview.clone(), stop.clone(), choice.clone());
        std::thread::Builder::new().name("harmony-screen".into()).spawn(move || run(c, rx, src, prev, st)).ok();
        let sound = loopback_for(&choice).map(Loopback::start);
        let sound_source = sound.as_ref().map(|l| l.source.clone());
        ScreenCapture {
            stop,
            source,
            preview,
            choice: Mutex::new(choice),
            control: tx,
            sound: Mutex::new(sound),
            sound_source: Mutex::new(sound_source),
        }
    }

    /// A track for the share's sound, if it has any.
    pub fn audio_track(&self) -> Option<libwebrtc::audio_track::RtcAudioTrack> {
        self.sound_source.lock().clone().map(|s| super::rtc::factory().create_audio_track("screen-sound", s))
    }

    pub fn switch(&self, choice: ScreenChoice) {
        *self.choice.lock() = choice.clone();
        let _ = self.control.send(Control::Switch(choice));
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        *self.sound.lock() = None;
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn loopback_for(c: &ScreenChoice) -> Option<loopback::Mode> {
    let me = std::process::id();
    match (c.sound, c.source.kind) {
        (Sound::Off, _) => None,
        (Sound::App, SourceKind::Window) => loopback::window_process(c.source.id).map(loopback::Mode::Only),
        _ => Some(loopback::Mode::Excluding(me)),
    }
}

fn target_size(c: &ScreenChoice) -> (u32, u32) {
    let (sw, sh) = c.source.size.unwrap_or((1920, 1080));
    fit(sw, sh, c.resolution)
}

/// The size to send: the source scaled down to the resolution's height, never up, even sides.
fn fit(sw: u32, sh: u32, r: Resolution) -> (u32, u32) {
    let (w, h) = match r.height() {
        Some(th) if sh > th => ((sw as u64 * th as u64 / sh as u64) as u32, th),
        _ => (sw, sh),
    };
    (w.max(2) & !1, h.max(2) & !1)
}

fn run(
    mut choice: ScreenChoice,
    control: mpsc::Receiver<Control>,
    source: NativeVideoSource,
    preview: Arc<FrameSlot>,
    stop: Arc<AtomicBool>,
) {
    'outer: while !stop.load(Ordering::Relaxed) {
        let Some(mut c) = capturer(choice.source.kind) else {
            log::warn!("screen capture is unavailable");
            return;
        };
        let target = c.get_source_list().into_iter().find(|s| s.id() == choice.source.id);
        let staging: Arc<Mutex<Staging>> = Default::default();
        let s = staging.clone();
        c.start_capture(target, move |frame| {
            if let Ok(f) = frame {
                s.lock().take(&f);
            }
        });
        let started = Instant::now();
        let mut preview_at = Instant::now() - Duration::from_secs(1);
        loop {
            let tick = Instant::now();
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            if let Ok(Control::Switch(next)) = control.try_recv() {
                let restart = next.source.id != choice.source.id || next.source.kind != choice.source.kind;
                choice = next;
                if restart {
                    continue 'outer;
                }
            }
            {
                let mut st = staging.lock();
                st.resolution = Some(choice.resolution);
                st.preview_id = preview.id;
                st.nv12 = super::rtc::hardware_encoder_running();
                // Your own preview at a few frames a second is enough, and none while nobody sees it.
                if !video::window_hidden() && preview_at.elapsed() > Duration::from_millis(200) {
                    preview_at = Instant::now();
                    st.want_preview = true;
                }
            }
            c.capture_frame();
            let mut st = staging.lock();
            if let Some(buffer) = st.frame.take() {
                let mut vf = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
                vf.timestamp_us = started.elapsed().as_micros() as i64;
                source.capture_frame(&vf);
            }
            if let Some(frame) = st.preview.take() {
                preview.put(frame);
            }
            drop(st);
            let frame = Duration::from_secs_f64(1. / choice.fps.max(1) as f64);
            if let Some(rest) = frame.checked_sub(tick.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    }
}

/// `w` and `h` scaled down to `max_w` wide at most, keeping the shape.
fn preview_size(w: u32, h: u32, max_w: u32) -> (u32, u32) {
    let tw = max_w.min(w).max(1);
    (tw, (h as u64 * tw as u64 / w.max(1) as u64).max(1) as u32)
}

/// Your own share, small: 640 px wide at most, as stream `id` of the window.
fn preview_frame(data: &[u8], w: u32, h: u32, stride: u32, id: u64, frames: &mut video::Scratch) -> SurfaceFrame {
    let (tw, th) = preview_size(w, h, 640);
    let mut bgra = vec![0u8; (tw * th * 4) as usize];
    yuv::scale_bgra(data, stride, w, h, &mut bgra, tw, th);
    let [(_, sy), (_, su), (_, sv)] = yuv::i420_layout(tw, th);
    frames
        .i420(id, tw, th, |y, u, v| yuv_helper::argb_to_i420(&bgra, tw * 4, y, sy as u32, u, su as u32, v, sv as u32, tw as i32, th as i32))
}

fn bgra_preview(data: &[u8], w: u32, h: u32, stride: u32, max_w: u32) -> Arc<RenderImage> {
    let (tw, th) = preview_size(w, h, max_w);
    // GPUI wants BGRA, which is what the capturer gives.
    let mut px = vec![0u8; (tw * th * 4) as usize];
    yuv::scale_bgra(data, stride, w, h, &mut px, tw, th);
    // Captures may leave alpha unset.
    px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    let img = image::RgbaImage::from_raw(tw, th, px).expect("preview size");
    Arc::new(RenderImage::new(SmallVec::from_elem(image::Frame::new(img), 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_scale_down_but_never_up() {
        assert_eq!(fit(2560, 1440, Resolution::P1080), (1920, 1080));
        assert_eq!(fit(1280, 720, Resolution::P1080), (1280, 720));
        assert_eq!(fit(2560, 1600, Resolution::P720), (1152, 720));
        assert_eq!(fit(1366, 768, Resolution::Native), (1366, 768));
    }

    #[test]
    fn the_preview_is_small_and_has_the_colours_sent() {
        let (w, h) = (1920, 1080);
        let bgra = [40u8, 120, 200, 0].repeat((w * h) as usize);
        let frame = preview_frame(&bgra, w, h, w * 4, 5, &mut video::Scratch::default());
        assert_eq!((frame.id(), frame.width(), frame.height()), (5, 640, 360));
        let sent = encoder_frame(&bgra, w * 4, (w, h), (640, 360), &mut Scratch::default());
        let (y, u, v) = sent.data();
        for (plane, want) in [(0, y[0]), (1, u[0]), (2, v[0])] {
            let (bytes, _) = frame.plane(plane);
            assert!(bytes.iter().all(|p| p.abs_diff(want) <= 1), "plane {plane}");
        }
    }

    #[test]
    fn every_scaling_path_keeps_the_picture() {
        // Each of the three ways to the size sent: as it is, at 3/4 in I420, and in BGRA; and the
        // first as NV12 for the hardware encoder.
        let mut scratch = Scratch::default();
        for nv12 in [false, true] {
            for ((w, h), r) in [((1280, 720), Resolution::P720), ((1280, 960), Resolution::P720), ((1920, 1080), Resolution::P720)] {
                let bgra = [40u8, 120, 200, 0].repeat((w * h) as usize);
                let (tw, th) = fit(w, h, r);
                for _ in 0..2 {
                    match encoder_frame(&bgra, w * 4, (w, h), (tw, th), nv12, &mut scratch) {
                        EncoderFrame::I420(out) => {
                            assert!(!nv12 || (tw, th) != (w, h));
                            assert_eq!((out.width(), out.height()), (tw, th));
                            let (y, u, v) = out.data();
                            let (y0, u0, v0) = (y[0], u[0], v[0]);
                            assert!(y.iter().all(|&p| p == y0) && u.iter().all(|&p| p == u0) && v.iter().all(|&p| p == v0), "{w}x{h}");
                        }
                        EncoderFrame::Nv12(out) => {
                            assert!(nv12 && (tw, th) == (w, h));
                            assert_eq!((out.width(), out.height()), (tw, th));
                            let (y, uv) = out.data();
                            assert!(y.iter().all(|&p| p == y[0]) && uv.chunks(2).all(|p| p == &uv[..2]), "{w}x{h} NV12");
                        }
                    }
                }
            }
        }
    }
}
