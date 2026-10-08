//! Cameras and screens on the stage. Each is a tile: someone else's stream, watched over WHEP
//! with its sound in the mix (screens only), or your own, previewed from the frames being sent.
//! Frames become BGRA images for GPUI; each replaced image is dropped from the window's atlas so
//! video doesn't fill memory. A tile is its own view, woken by the frames themselves, so a new
//! frame redraws that tile and nothing that merely contains it.

use super::audio::{self, Source, SourceKind};
use super::rtc::Track;
use super::voice::{Voice, Watch};
use crate::core;
use crate::core::types::*;
use futures::StreamExt;
use gpui::{App, AppContext, Context, Entity, RenderImage, Task};
use libwebrtc::audio_stream::native::NativeAudioStream;
use libwebrtc::prelude::*;
use libwebrtc::video_frame::native::VideoFrameBufferExt;
use libwebrtc::video_stream::native::NativeVideoStream;
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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
#[derive(Default)]
pub struct FrameSlot {
    frame: Mutex<Option<Arc<RenderImage>>>,
    fresh: AtomicBool,
    /// Wakes the tile showing this slot; one pending wake covers any number of frames.
    wake: Mutex<Option<async_channel::Sender<()>>>,
    /// Nobody can see this stream now (a text channel is open), so its frames aren't converted;
    /// one flag for every stream in the call.
    pub hidden: Arc<AtomicBool>,
    /// The size the tile is drawn at, in device pixels; frames are scaled down to it.
    pub want_w: AtomicU32,
    pub want_h: AtomicU32,
    pub width: AtomicU32,
    pub height: AtomicU32,
}

impl FrameSlot {
    pub fn put(&self, img: Arc<RenderImage>, w: u32, h: u32) {
        *self.frame.lock() = Some(img);
        self.width.store(w, Ordering::Relaxed);
        self.height.store(h, Ordering::Relaxed);
        self.fresh.store(true, Ordering::Release);
        if let Some(wake) = &*self.wake.lock() {
            let _ = wake.try_send(());
        }
    }

    fn take_fresh(&self) -> Option<Arc<RenderImage>> {
        self.fresh.swap(false, Ordering::Acquire).then(|| self.frame.lock().clone()).flatten()
    }
}

/// A frame as GPUI wants it: BGRA, scaled to fit `max` if given.
pub fn to_image(buffer: &dyn VideoBuffer, max: Option<(u32, u32)>) -> (Arc<RenderImage>, u32, u32) {
    let (sw, sh) = (buffer.width(), buffer.height());
    let (w, h) = match max {
        Some((mw, mh)) if mw > 0 && mh > 0 && (sw > mw || sh > mh) => {
            let s = (mw as f32 / sw as f32).min(mh as f32 / sh as f32);
            (((sw as f32 * s) as u32).max(2) & !1, ((sh as f32 * s) as u32).max(2) & !1)
        }
        _ => (sw, sh),
    };
    let mut data = vec![0u8; (w * h * 4) as usize];
    if (w, h) == (sw, sh) {
        buffer.to_argb(VideoFormatType::ARGB, &mut data, w * 4, w as i32, h as i32);
    } else {
        let mut i420 = buffer.to_i420();
        let scaled = i420.scale(w as i32, h as i32);
        scaled.to_argb(VideoFormatType::ARGB, &mut data, w * 4, w as i32, h as i32);
    }
    let img = image::RgbaImage::from_raw(w, h, data).expect("frame size");
    let frame = image::Frame::new(img);
    (Arc::new(RenderImage::new(SmallVec::from_elem(frame, 1))), w, h)
}

/// Starts feeding a watched track into a tile: video frames into its slot, sound into its source.
pub fn pump(track: Track, slot: &Arc<FrameSlot>, sound: Option<&Arc<Source>>) -> Option<tokio::task::JoinHandle<()>> {
    match track {
        Track::Video(v) => {
            let slot = slot.clone();
            Some(core::runtime().spawn(async move {
                let mut stream = NativeVideoStream::new(v);
                while let Some(frame) = stream.next().await {
                    if slot.hidden.load(Ordering::Relaxed) {
                        continue;
                    }
                    let want = (slot.want_w.load(Ordering::Relaxed), slot.want_h.load(Ordering::Relaxed));
                    let (img, w, h) = to_image(frame.buffer.as_ref(), Some(want));
                    slot.put(img, w, h);
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
    /// What is on screen now; the previous one is dropped from the atlas when replaced.
    pub image: Option<Arc<RenderImage>>,
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
                    if let Some(img) = t.slot.take_fresh() {
                        if let Some(old) = t.image.replace(img) {
                            cx.drop_image(old, None);
                        }
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
        Tile { user, mid, kind, local, slot, image: None, sound: None, watch: Watch::default(), failed: false, _redraw: redraw }
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
        if let Some(img) = self.image.take() {
            cx.drop_image(img, None);
        }
        *self.slot.wake.lock() = None;
        self._redraw = Task::ready(());
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
