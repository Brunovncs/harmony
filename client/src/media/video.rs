//! Cameras and screens on the stage. Each is a tile: someone else's stream, watched over WHEP
//! with its sound in the mix (screens only), or your own, previewed from the frames being sent.
//! Frames stay YUV, as decoders give them, and the window's renderer uploads, converts and scales
//! them on the GPU, keeping each stream's textures from frame to frame. A tile is its own view,
//! woken by the frames themselves, so a new frame redraws that tile and nothing that merely
//! contains it.

use super::audio::{self, Source, SourceKind};
use super::rtc::Track;
use super::voice::{Voice, Watch};
use super::yuv;
use crate::core;
use crate::core::types::*;
use futures::StreamExt;
use gpui::{App, AppContext, Context, Entity, SurfaceFormat, SurfaceFrame, SurfacePlane, Task};
use libwebrtc::audio_stream::native::NativeAudioStream;
use libwebrtc::prelude::*;
use libwebrtc::video_stream::native::NativeVideoStream;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TileKind {
    Camera,
    Screen,
}

impl TileKind {
    pub fn path(self) -> &'static str {
        match self {
            TileKind::Camera => "c",
            TileKind::Screen => "s",
        }
    }
}

/// The newest frame, handed from the decoding thread to the window.
pub struct FrameSlot {
    /// Names the stream to the renderer, which keeps its textures by it.
    pub id: u64,
    frame: Mutex<Option<SurfaceFrame>>,
    fresh: AtomicBool,
    /// Wakes the tile showing this slot; one pending wake covers any number of frames.
    wake: Mutex<Option<async_channel::Sender<()>>>,
    /// Nobody can see this stream now (a text channel is open), so its frames aren't converted;
    /// one flag for every stream in the call.
    pub hidden: Arc<AtomicBool>,
    /// The size the tile is drawn at, in device pixels; frames much bigger are scaled down to it.
    pub want_w: AtomicU32,
    pub want_h: AtomicU32,
}

impl Default for FrameSlot {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        FrameSlot {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            frame: Mutex::default(),
            fresh: AtomicBool::default(),
            wake: Mutex::default(),
            hidden: Arc::default(),
            want_w: AtomicU32::default(),
            want_h: AtomicU32::default(),
        }
    }
}

impl FrameSlot {
    pub fn put(&self, frame: SurfaceFrame) {
        *self.frame.lock() = Some(frame);
        self.fresh.store(true, Ordering::Release);
        if let Some(wake) = &*self.wake.lock() {
            let _ = wake.try_send(());
        }
    }

    fn take_fresh(&self) -> Option<SurfaceFrame> {
        self.fresh.swap(false, Ordering::Acquire).then(|| self.frame.lock().clone()).flatten()
    }
}

static WINDOW_HIDDEN: AtomicBool = AtomicBool::new(false);

/// The window is minimised or hidden to the tray: nobody can see any stream or preview.
pub fn set_window_hidden(hidden: bool) {
    WINDOW_HIDDEN.store(hidden, Ordering::Relaxed);
}

pub fn window_hidden() -> bool {
    WINDOW_HIDDEN.load(Ordering::Relaxed)
}

/// Frames a stream can have out at once (in its slot, its tile and the window's last two scenes)
/// whose buffers are kept for reuse; any more are allocated and let go.
const POOL: usize = 6;

/// What one stream keeps between frames: the buffers of frames the window is done with, so a new
/// frame doesn't allocate.
#[derive(Default)]
pub struct Scratch {
    pool: Vec<Arc<[u8]>>,
}

impl Scratch {
    /// `len` bytes written by `fill`, in one of the pool's buffers that nothing holds any more if
    /// there is one.
    fn buffer(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> Arc<[u8]> {
        self.pool.retain(|b| b.len() == len);
        if let Some(i) = self.pool.iter_mut().position(|b| Arc::get_mut(b).is_some()) {
            fill(Arc::get_mut(&mut self.pool[i]).expect("unshared"));
            return self.pool[i].clone();
        }
        let mut b: Arc<[u8]> = std::iter::repeat_n(0, len).collect();
        fill(Arc::get_mut(&mut b).expect("new"));
        if self.pool.len() < POOL {
            self.pool.push(b.clone());
        }
        b
    }

    /// A `w` × `h` I420 frame of stream `id` in one buffer, its Y, U and V planes written by `fill`.
    pub fn i420(&mut self, id: u64, w: u32, h: u32, fill: impl FnOnce(&mut [u8], &mut [u8], &mut [u8])) -> SurfaceFrame {
        let data = self.buffer(yuv::i420_len(w, h), |buf| {
            let (y, u, v) = yuv::i420_planes(buf, w, h);
            fill(y, u, v)
        });
        let planes = yuv::i420_layout(w, h).map(|(offset, stride)| SurfacePlane { offset, stride });
        SurfaceFrame::new(id, SurfaceFormat::I420, w, h, data, &planes)
    }

    /// A frame of stream `id` from planes (bytes, stride) copied as they lie, one after another.
    fn copied(&mut self, id: u64, format: SurfaceFormat, w: u32, h: u32, planes: &[(&[u8], u32)]) -> SurfaceFrame {
        let mut layout = Vec::with_capacity(planes.len());
        let mut len = 0;
        for (bytes, stride) in planes {
            layout.push(SurfacePlane { offset: len, stride: *stride as usize });
            len += bytes.len();
        }
        let data = self.buffer(len, |buf| {
            for ((bytes, _), at) in planes.iter().zip(&layout) {
                buf[at.offset..at.offset + bytes.len()].copy_from_slice(bytes);
            }
        });
        SurfaceFrame::new(id, format, w, h, data, &layout)
    }

    /// An I420 picture as a frame of stream `id`.
    pub fn of_i420(&mut self, id: u64, b: &I420Buffer) -> SurfaceFrame {
        let (y, u, v) = b.data();
        let (sy, su, sv) = b.strides();
        self.copied(id, SurfaceFormat::I420, b.width(), b.height(), &[(y, sy), (u, su), (v, sv)])
    }
}

/// The size to scale a `sw` × `sh` frame down to before it is uploaded, when it has more than half
/// again the pixels of what fits in `max`: they would cost upload bandwidth only for the GPU to
/// throw them away. Smaller ones are uploaded whole and scaled by the GPU as it draws.
fn shrink_to(sw: u32, sh: u32, (mw, mh): (u32, u32)) -> Option<(u32, u32)> {
    if mw == 0 || mh == 0 || sw == 0 || sh == 0 {
        return None;
    }
    let s = (mw as f64 / sw as f64).min(mh as f64 / sh as f64);
    let side = |n: u32| ((n as f64 * s).round() as u32).max(2) & !1;
    (s * s * 1.5 < 1.).then(|| (side(sw), side(sh)))
}

/// A decoded frame as a frame of stream `id` for the window: its planes as the decoder left
/// them, or scaled down on the CPU when it is much bigger than `max` (see `shrink_to`).
pub fn surface_frame(buffer: &dyn VideoBuffer, id: u64, max: (u32, u32), scratch: &mut Scratch) -> SurfaceFrame {
    let (sw, sh) = (buffer.width(), buffer.height());
    if let Some((w, h)) = shrink_to(sw, sh, max) {
        // Decoders hand over I420, which is scaled from where it lies; `to_i420` would copy it first.
        let converted;
        let src = match buffer.as_i420() {
            Some(b) => b,
            None => {
                converted = buffer.to_i420();
                &converted
            }
        };
        return scratch.i420(id, w, h, |y, u, v| yuv::scale_into(src, y, u, v, w, h));
    }
    if let Some(b) = buffer.as_i420() {
        return scratch.of_i420(id, b);
    }
    if let Some(b) = buffer.as_nv12() {
        let (y, uv) = b.data();
        let (sy, suv) = b.strides();
        return scratch.copied(id, SurfaceFormat::Nv12, sw, sh, &[(y, sy), (uv, suv)]);
    }
    scratch.of_i420(id, &buffer.to_i420())
}

/// Starts feeding a watched track into a tile: video frames into its slot, sound into its source.
pub fn pump(track: Track, slot: &Arc<FrameSlot>, sound: Option<&Arc<Source>>) -> Option<tokio::task::JoinHandle<()>> {
    match track {
        Track::Video(v) => {
            let slot = slot.clone();
            Some(core::runtime().spawn(async move {
                let mut stream = NativeVideoStream::new(v);
                let mut scratch = Scratch::default();
                while let Some(frame) = stream.next().await {
                    if slot.hidden.load(Ordering::Relaxed) || window_hidden() {
                        continue;
                    }
                    let want = (slot.want_w.load(Ordering::Relaxed), slot.want_h.load(Ordering::Relaxed));
                    let (into, mut kept) = (slot.clone(), std::mem::take(&mut scratch));
                    // Off the network runtime's workers. The stream holds only the newest frame
                    // meanwhile, so a slow copy or scale skips frames rather than queueing them.
                    let converted = tokio::task::spawn_blocking(move || {
                        into.put(surface_frame(frame.buffer.as_ref(), into.id, want, &mut kept));
                        kept
                    });
                    match converted.await {
                        Ok(s) => scratch = s,
                        Err(e) => {
                            log::warn!("video frame conversion failed: {e}");
                            break;
                        }
                    }
                }
            }))
        }
        Track::Audio(a) => {
            let src = sound?.clone();
            Some(core::runtime().spawn(async move {
                let mut stream = NativeAudioStream::new(a, audio::RATE as i32, 2);
                while let Some(frame) = stream.next().await {
                    src.push_i16(&frame.data, frame.num_channels as usize);
                }
            }))
        }
    }
}

pub struct Tile {
    pub user: UserId,
    pub mid: i64,
    pub kind: TileKind,
    pub local: bool,
    pub slot: Arc<FrameSlot>,
    /// What is on screen now.
    pub frame: Option<SurfaceFrame>,
    /// The screen's sound, for someone else's screen.
    pub sound: Option<Arc<Source>>,
    /// Someone else's stream: the subscription, which `Voice` keeps alive.
    pub watch: Watch,
    /// Several attempts in a row have failed (it keeps trying).
    pub failed: bool,
    _redraw: Task<()>,
}

impl Tile {
    fn new(user: UserId, mid: i64, kind: TileKind, local: bool, slot: Arc<FrameSlot>, cx: &mut Context<Self>) -> Tile {
        let (wake, woken) = async_channel::bounded(1);
        *slot.wake.lock() = Some(wake);
        let redraw = cx.spawn(async move |this, cx| {
            loop {
                let Ok(()) = this.update(cx, |t, cx| {
                    if let Some(frame) = t.slot.take_fresh() {
                        t.frame = Some(frame);
                        cx.notify();
                    }
                }) else {
                    break;
                };
                if woken.recv().await.is_err() {
                    break;
                }
            }
        });
        Tile { user, mid, kind, local, slot, frame: None, sound: None, watch: Watch::default(), failed: false, _redraw: redraw }
    }

    /// A tile showing your own camera or screen, from the frames being published.
    pub fn local(user: UserId, mid: i64, kind: TileKind, slot: Arc<FrameSlot>, cx: &mut App) -> Entity<Tile> {
        cx.new(|cx| Tile::new(user, mid, kind, true, slot, cx))
    }

    /// Someone else's camera or screen, not yet connected: `Voice` subscribes and retries.
    pub fn remote(user: UserId, mid: i64, kind: TileKind, hidden: Arc<AtomicBool>, cx: &mut App) -> Entity<Tile> {
        cx.new(|cx| {
            let mut tile = Tile::new(user, mid, kind, false, Arc::new(FrameSlot { hidden, ..Default::default() }), cx);
            tile.sound = (kind == TileKind::Screen).then(|| {
                let s = Source::new(SourceKind::Stream, 1., false);
                audio::audio().mixer.add(s.clone());
                s
            });
            tile
        })
    }

    pub fn set_gain(&mut self, gain: f32, cx: &mut Context<Self>) {
        if let Some(s) = &self.sound {
            s.gain.set(gain.clamp(0., audio::MAX_GAIN));
        }
        cx.notify();
    }

    pub fn gain(&self) -> f32 {
        self.sound.as_ref().map(|s| s.gain.get()).unwrap_or(1.)
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.watch.hang_up();
        if let Some(s) = self.sound.take() {
            audio::audio().mixer.remove(&s);
        }
        self.frame = None;
        *self.slot.wake.lock() = None;
        self._redraw = Task::ready(());
        cx.notify();
    }
}

/// Tiles the person closed, by (user, kind); they stay closed until that stream ends.
#[derive(Default)]
pub struct Closed(pub HashSet<(UserId, TileKind)>);

/// Opens a tile for every camera and screen others publish, and closes the ones that ended.
pub fn sync_tiles(voice: &mut Voice, roster: &[Member], me: UserId, cx: &mut Context<Voice>) {
    let live: Vec<(UserId, i64, TileKind)> = roster
        .iter()
        .filter(|m| m.user_id != me)
        .flat_map(|m| {
            let mut v = Vec::new();
            if m.publishes("c") {
                v.push((m.user_id, m.mid, TileKind::Camera));
            }
            if m.publishes("s") {
                v.push((m.user_id, m.mid, TileKind::Screen));
            }
            v
        })
        .collect();
    let mut keep = Vec::new();
    for tile in voice.tiles.drain(..) {
        let (user, mid, kind, local) = {
            let t = tile.read(cx);
            (t.user, t.mid, t.kind, t.local)
        };
        if local || live.iter().any(|(u, m, k)| *u == user && *m == mid && *k == kind) {
            keep.push(tile);
        } else {
            tile.update(cx, |t, cx| t.close(cx));
        }
    }
    voice.tiles = keep;
    let closed: HashSet<(UserId, TileKind)> = voice.closed.0.clone();
    voice.closed.0.retain(|(u, k)| live.iter().any(|(lu, _, lk)| lu == u && lk == k));
    for (user, mid, kind) in live {
        let exists = voice.tiles.iter().any(|t| {
            let t = t.read(cx);
            t.user == user && t.kind == kind && !t.local
        });
        if exists || closed.contains(&(user, kind)) {
            continue;
        }
        voice.tiles.push(Tile::remote(user, mid, kind, voice.streams_hidden.clone(), cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_frames_much_bigger_than_drawn_are_scaled_on_the_cpu() {
        // Four times the pixels of the tile: scaled to fit it.
        assert_eq!(shrink_to(1920, 1080, (960, 540)), Some((960, 540)));
        // Letterboxed in a square tile: fitted to its width.
        assert_eq!(shrink_to(1920, 1080, (640, 640)), Some((640, 360)));
        // Under half again as many pixels, or fewer: uploaded whole, for the GPU to scale.
        assert_eq!(shrink_to(1920, 1080, (1600, 900)), None);
        assert_eq!(shrink_to(1280, 720, (1920, 1080)), None);
        // A tile not laid out yet asks for nothing.
        assert_eq!(shrink_to(1920, 1080, (0, 0)), None);
    }

    /// Whether every pixel of one of the frame's planes is `value`.
    fn plane_is(frame: &SurfaceFrame, plane: usize, value: u8) -> bool {
        let (bytes, stride) = frame.plane(plane);
        let (w, rows) = frame.plane_size(plane);
        (0..rows).all(|r| bytes[r * stride..r * stride + w].iter().all(|&p| p == value))
    }

    #[test]
    fn frames_keep_their_planes_and_reuse_buffers_the_window_let_go() {
        let mut src = I420Buffer::new(64, 36);
        let (y, u, v) = src.data_mut();
        y.fill(100);
        u.fill(90);
        v.fill(160);
        let mut scratch = Scratch::default();
        let a = surface_frame(&src, 3, (64, 36), &mut scratch);
        assert_eq!((a.id(), a.width(), a.height(), a.format()), (3, 64, 36, SurfaceFormat::I420));
        assert!(plane_is(&a, 0, 100) && plane_is(&a, 1, 90) && plane_is(&a, 2, 160));
        let first = a.plane(0).0.as_ptr();
        drop(a);
        let b = surface_frame(&src, 3, (64, 36), &mut scratch);
        assert_eq!(b.plane(0).0.as_ptr(), first, "the first frame's buffer, free again");
        let c = surface_frame(&src, 3, (64, 36), &mut scratch);
        assert_ne!(c.plane(0).0.as_ptr(), first, "still held by the second frame");
        assert_ne!(b.serial(), c.serial());

        // Drawn at a quarter of the pixels: scaled down first, colours kept.
        let d = surface_frame(&src, 3, (32, 18), &mut scratch);
        assert_eq!((d.width(), d.height()), (32, 18));
        assert!(plane_is(&d, 0, 100) && plane_is(&d, 1, 90) && plane_is(&d, 2, 160));
    }
}
