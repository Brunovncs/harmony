//! The media side of a call, and the one reconciler that keeps it right: your microphone
//! published to your slot's `v` path, one audio-only subscription to everyone else's (each
//! playing through its own source in the mix at that person's volume), and a tile for every
//! camera and screen. Every async setup carries a generation, so a stale completion hangs up
//! instead of attaching; failures back off 1, 2, 4, 8 s; a subscription that goes quiet is
//! replaced. What you publish is announced only once its connection is up, and announced again
//! whenever the roster shows the server lost it (a socket reconnect clears it there).

use super::audio::{self, Cue, MAX_GAIN, MicGuard, Source, SourceKind};
use super::rtc::{self, Link, Track};
use super::video::{Tile, TileKind};
use crate::core::api::ApiError;
use crate::core::types::*;
use crate::core::{self};
use crate::prefs::prefs;
use crate::session::Session;
use futures::StreamExt;
use gpui::{App, Context, Entity, Task, WeakEntity};
use libwebrtc::audio_source::AudioSourceOptions;
use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_stream::native::NativeAudioStream;
use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Per-person volumes (0..3.5), by user so they survive changing slots and rejoining. Kept for as
/// long as the app runs, as in the old client.
fn volumes() -> &'static Mutex<HashMap<UserId, f32>> {
    static V: OnceLock<Mutex<HashMap<UserId, f32>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

pub fn volume_of(user: UserId) -> f32 {
    volumes().lock().get(&user).copied().unwrap_or(1.)
}

/// How long a new connection gets to come up (ICE and DTLS) before it counts as failed.
pub const CONNECT_LIMIT: Duration = Duration::from_secs(10);
/// How long a subscription may go without a packet before it is replaced.
const STALL: Duration = Duration::from_secs(8);
/// A roster that hasn't caught up with an announcement yet doesn't repeat it before this.
const ANNOUNCE_AGAIN: Duration = Duration::from_secs(3);
/// Several failures in a row before a tile says it can't connect (it keeps trying).
const TILE_FAILED_AFTER: u32 = 3;

/// A new generation for an async setup; a completion carrying an older one is stale.
pub fn next_generation() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

pub fn not_connected() -> ApiError {
    ApiError {
        status: 0,
        code: ErrorCode::MediaError,
        message: tr!("The connection to the media server never came up.", "A conexão com o servidor de mídia não se estabeleceu.").into(),
        body: serde_json::Value::Null,
    }
}

/// Spacing between attempts after failures: 1, 2, 4, then 8 s.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Backoff {
    pub failures: u32,
    last: Option<Instant>,
}

impl Backoff {
    pub fn delay(failures: u32) -> Duration {
        match failures {
            0 => Duration::ZERO,
            n => Duration::from_secs(1 << (n - 1).min(3)),
        }
    }

    pub fn ready(&self, now: Instant) -> bool {
        self.last.is_none_or(|t| now >= t + Backoff::delay(self.failures))
    }

    pub fn fail(&mut self, now: Instant) {
        self.failures += 1;
        self.last = Some(now);
    }

    pub fn reset(&mut self) {
        *self = Backoff::default();
    }
}

/// How an attempt to watch ended.
pub enum Outcome {
    /// A newer attempt (or a hang-up) came first; this one was hung up.
    Stale,
    Attached,
    Failed(ApiError),
}

/// One WHEP subscription's life, the same for a voice and for a camera or screen: the attempt
/// in flight, the backoff after failures, and the packet count that tells a live stream from a
/// stalled one.
#[derive(Default)]
pub struct Watch {
    pub link: Option<Arc<Link>>,
    generation: u64,
    connecting: bool,
    pub backoff: Backoff,
    meter: Meter,
    pumps: Vec<tokio::task::JoinHandle<()>>,
}

/// Packets seen so far, and since when no new ones came.
#[derive(Default)]
struct Meter {
    packets: u64,
    quiet_since: Option<Instant>,
}

impl Meter {
    /// True once the count has stood still for too long.
    fn stalled(&mut self, packets: u64, now: Instant) -> bool {
        if packets > self.packets {
            self.packets = packets;
            self.quiet_since = None;
            return false;
        }
        now.duration_since(*self.quiet_since.get_or_insert(now)) >= STALL
    }
}

impl Watch {
    /// Not connected, nothing in flight, and the backoff has passed.
    pub fn due(&self, now: Instant) -> bool {
        self.link.is_none() && !self.connecting && self.backoff.ready(now)
    }

    /// Starts an attempt, hanging up whatever was there; returns its generation.
    pub fn begin(&mut self) -> u64 {
        self.hang_up();
        self.connecting = true;
        self.generation = next_generation();
        self.generation
    }

    pub fn finish(
        &mut self,
        generation: u64,
        result: Result<Link, ApiError>,
        pumps: Vec<tokio::task::JoinHandle<()>>,
        now: Instant,
    ) -> Outcome {
        if generation != self.generation || !self.connecting {
            // Dropping the link hangs it up.
            pumps.iter().for_each(|p| p.abort());
            return Outcome::Stale;
        }
        self.connecting = false;
        match result {
            Ok(link) => {
                self.link = Some(Arc::new(link));
                self.pumps = pumps;
                self.backoff.reset();
                self.meter = Meter::default();
                Outcome::Attached
            }
            Err(e) => {
                pumps.iter().for_each(|p| p.abort());
                self.backoff.fail(now);
                Outcome::Failed(e)
            }
        }
    }

    /// Feeds the packet count from stats taken for generation `generation`; true once it has been quiet
    /// for too long.
    pub fn stalled(&mut self, generation: u64, packets: u64, now: Instant) -> bool {
        generation == self.generation && self.link.is_some() && self.meter.stalled(packets, now)
    }

    pub fn hang_up(&mut self) {
        self.pumps.drain(..).for_each(|p| p.abort());
        if let Some(l) = self.link.take() {
            l.close();
        }
        self.connecting = false;
        self.meter = Meter::default();
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.hang_up();
    }
}

/// Which of `v`, `c`, `s` the server has wrong about you, as what to tell it.
pub fn publishing_fixes(live: [(&'static str, bool); 3], server: &[String]) -> Vec<(&'static str, bool)> {
    live.into_iter().filter(|(kind, on)| server.iter().any(|s| s == kind) != *on).collect()
}

struct Peer {
    user: UserId,
    source: Arc<Source>,
    watch: Watch,
}

/// A connection whose stats were asked for, to match the answer back.
enum Watched {
    Peer(i64),
    Tile(WeakEntity<Tile>),
}

pub struct Voice {
    session: Entity<Session>,
    pub channel: ChannelId,
    tokens: VoiceTokens,
    mid: Option<i64>,
    muted: bool,
    deafened: bool,
    /// An admin holds you muted; the server refuses the microphone until they let go.
    force_muted: bool,
    mic: Option<Arc<Link>>,
    /// The microphone publish in flight, by generation.
    mic_attempt: Option<u64>,
    mic_backoff: Backoff,
    _mic_guard: Option<MicGuard>,
    peers: HashMap<i64, Peer>,
    /// What was last told to the server about each path, and when.
    announced: HashMap<&'static str, (bool, Instant)>,
    speaking: HashSet<UserId>,
    envelopes: HashMap<UserId, (f32, Instant)>,
    pub ping_ms: Option<u32>,
    pub route: Option<rtc::Route>,
    pub tiles: Vec<Entity<Tile>>,
    /// Shared by others' tiles: set while the stage is out of sight.
    pub(super) streams_hidden: Arc<std::sync::atomic::AtomicBool>,
    pub closed: super::video::Closed,
    pub shares: super::share::Shares,
    roster: Vec<Member>,
    _ticker: Task<()>,
    _slow: Task<()>,
}

const SPEAK_ON: f32 = 0.0075;
const SPEAK_OFF: f32 = 0.0035;

impl Voice {
    pub fn new(
        session: Entity<Session>,
        channel: ChannelId,
        tokens: VoiceTokens,
        mid: Option<i64>,
        muted: bool,
        deafened: bool,
        cx: &mut Context<Self>,
    ) -> Voice {
        let a = audio::audio();
        apply_mic_prefs(cx);
        a.mixer.deafened.store(deafened, std::sync::atomic::Ordering::Relaxed);
        a.mic.muted.store(muted, std::sync::atomic::Ordering::Relaxed);
        // Speaking rings, ten times a second.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                if this.update(cx, |v, cx| v.tick(cx)).is_err() {
                    break;
                }
            }
        });
        // Retries every second (backoff decides who is due); stalls and ping every four.
        let slow = cx.spawn(async move |this, cx| {
            for n in 1u64.. {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let ok = this.update(cx, |v, cx| {
                    v.retry(cx);
                    if n.is_multiple_of(4) {
                        v.check(cx);
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        });
        let mut v = Voice {
            session,
            channel,
            tokens,
            mid,
            muted,
            deafened,
            force_muted: false,
            mic: None,
            mic_attempt: None,
            mic_backoff: Backoff::default(),
            _mic_guard: Some(a.acquire_mic()),
            peers: HashMap::new(),
            announced: HashMap::new(),
            speaking: HashSet::new(),
            envelopes: HashMap::new(),
            ping_ms: None,
            route: None,
            tiles: Vec::new(),
            streams_hidden: Default::default(),
            closed: Default::default(),
            shares: Default::default(),
            roster: Vec::new(),
            _ticker: ticker,
            _slow: slow,
        };
        v.reconcile(cx);
        v
    }

    pub fn set_tokens(&mut self, tokens: VoiceTokens, _: &mut Context<Self>) {
        self.tokens = tokens;
    }

    /// After a reconnect. Subscriptions to others stay (their paths and sessions didn't change),
    /// and so does what you publish if the slot is the same; the server forgot it was live, which
    /// the next roster shows and the reconciler puts right. A new slot means new paths: the old
    /// ones are hung up and everything is published again.
    pub fn restart(&mut self, tokens: VoiceTokens, mid: Option<i64>, cx: &mut Context<Self>) {
        self.tokens = tokens;
        if mid != self.mid {
            log::info!("slot {:?} became {mid:?}; publishing again", self.mid);
            self.mid = mid;
            self.drop_mic();
            self.mic_backoff.reset();
            self.repath_shares(cx);
        }
        self.reconcile(cx);
    }

    fn ice(&self, cx: &App) -> Vec<IceServer> {
        self.session.read(cx).ice_servers.clone()
    }

    fn start_mic(&mut self, cx: &mut Context<Self>) {
        let generation = next_generation();
        self.mic_attempt = Some(generation);
        let source = NativeAudioSource::new(AudioSourceOptions::default(), audio::RATE, 1, 0);
        let track = rtc::factory().create_audio_track("mic", source.clone());
        let api = self.session.read(cx).api.clone();
        let url = self.tokens.publish.voice.clone();
        let ice = self.ice(cx);
        let mid = self.mid;
        cx.spawn(async move |this, cx| {
            let link = core::run(async move {
                let link = rtc::publish(&api, &url, &ice, Some((track, 32_000)), None).await?;
                if link.connected(CONNECT_LIMIT).await { Ok(link) } else { Err(not_connected()) }
            })
            .await;
            let _ = this.update(cx, |v, cx| {
                if v.mic_attempt != Some(generation) {
                    // Superseded or dropped meanwhile; the link hangs up as it goes.
                    return;
                }
                v.mic_attempt = None;
                match link {
                    Ok(link) => {
                        log::info!("microphone published to slot {mid:?}");
                        // Only now does the microphone feed this source, so an attempt that
                        // lost the race can't leave the live link without sound.
                        audio::audio().set_mic_sink(Some(Box::new(move |pcm: &[i16]| {
                            let frame = libwebrtc::audio_frame::AudioFrame {
                                data: pcm.to_vec().into(),
                                sample_rate: audio::RATE,
                                num_channels: 1,
                                samples_per_channel: pcm.len() as u32,
                            };
                            let _ = futures::executor::block_on(source.capture_frame(&frame));
                        })));
                        v.mic = Some(Arc::new(link));
                        v.mic_backoff.reset();
                        v.announce(cx);
                    }
                    Err(e) => {
                        v.mic_backoff.fail(Instant::now());
                        log::warn!(
                            "microphone publish failed ({} in a row): {} ({}, {})",
                            v.mic_backoff.failures,
                            e.message,
                            e.code,
                            e.status
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn drop_mic(&mut self) {
        audio::audio().set_mic_sink(None);
        self.mic_attempt = None;
        if let Some(link) = self.mic.take() {
            link.close();
        }
    }

    pub fn mic_failed(&self) -> bool {
        self.mic.is_none() && !self.force_muted && self.mic_backoff.failures > 0
    }

    pub fn set_muted(&mut self, muted: bool, cx: &mut Context<Self>) {
        self.muted = muted;
        audio::audio().mic.muted.store(muted, std::sync::atomic::Ordering::Relaxed);
        cx.notify();
    }

    pub fn set_deafened(&mut self, deafened: bool, cx: &mut Context<Self>) {
        self.deafened = deafened;
        audio::audio().mixer.deafened.store(deafened, std::sync::atomic::Ordering::Relaxed);
        cx.notify();
    }

    /// Lets others' cameras and screens skip turning frames into pictures nobody sees.
    pub fn hide_streams(&self, hidden: bool) {
        self.streams_hidden.store(hidden, Ordering::Relaxed);
    }

    pub fn is_speaking(&self, user: UserId) -> bool {
        self.speaking.contains(&user)
    }

    fn peer_url(&self, mid: i64, kind: &str) -> String {
        let t = &self.tokens;
        let token = url::form_urlencoded::byte_serialize(t.token.as_bytes()).collect::<String>();
        format!("{}/vc-{}-{}-{kind}/whep?token={token}", t.whep_base.trim_end_matches('/'), base36(self.channel), base36(mid))
    }

    /// Brings everything in line with a new roster, and plays the join, leave and live cues.
    pub fn sync(&mut self, roster: &[Member], previous: Option<&[Member]>, cx: &mut Context<Self>) {
        let me = self.session.read(cx).me.id;
        if let Some(prev) = previous
            && prefs(cx).voice_sounds
        {
            let before: HashSet<UserId> = prev.iter().map(|m| m.user_id).collect();
            let now: HashSet<UserId> = roster.iter().map(|m| m.user_id).collect();
            if now.difference(&before).any(|u| *u != me) {
                audio::cue(Cue::Join);
            } else if before.difference(&now).any(|u| *u != me) {
                audio::cue(Cue::Leave);
            }
            let went_live = roster
                .iter()
                .any(|m| m.user_id != me && m.publishes("s") && !prev.iter().any(|p| p.user_id == m.user_id && p.publishes("s")));
            if went_live {
                audio::cue(Cue::Live);
            }
        }
        self.roster = roster.to_vec();
        if let Some(mine) = roster.iter().find(|m| m.user_id == me) {
            self.force_muted = mine.force_muted;
        }
        let wanted: HashMap<i64, UserId> =
            roster.iter().filter(|m| m.user_id != me && m.publishes("v")).map(|m| (m.mid, m.user_id)).collect();
        let gone: Vec<i64> = self.peers.iter().filter(|(mid, p)| wanted.get(mid) != Some(&p.user)).map(|(mid, _)| *mid).collect();
        for mid in gone {
            if let Some(p) = self.peers.remove(&mid) {
                close_peer(p);
            }
        }
        let mut delay = 0u64;
        for (mid, user) in wanted {
            if self.peers.contains_key(&mid) {
                continue;
            }
            let source = Source::new(SourceKind::Voice, volume_of(user), false);
            audio::audio().mixer.add(source.clone());
            self.peers.insert(mid, Peer { user, source, watch: Watch::default() });
            self.connect_peer(mid, Duration::from_millis(delay), cx);
            delay += 75;
        }
        super::video::sync_tiles(self, roster, me, cx);
        self.retry(cx);
        cx.notify();
    }

    fn connect_peer(&mut self, mid: i64, after: Duration, cx: &mut Context<Self>) {
        let Some(peer) = self.peers.get_mut(&mid) else { return };
        let generation = peer.watch.begin();
        let source = peer.source.clone();
        let api = self.session.read(cx).api.clone();
        let url = self.peer_url(mid, "v");
        let ice = self.ice(cx);
        let pumps: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Default::default();
        let pump_slot = pumps.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(after).await;
            let link = core::run(async move {
                rtc::watch(&api, &url, &ice, false, move |track| {
                    if let Track::Audio(a) = track {
                        let src = source.clone();
                        log::info!("voice from slot {mid}: track in hand");
                        pump_slot.lock().push(core::runtime().spawn(async move {
                            let mut stream = NativeAudioStream::new(a, audio::RATE as i32, 1);
                            while let Some(frame) = stream.next().await {
                                src.push_i16(&frame.data, frame.num_channels as usize);
                            }
                        }));
                    }
                })
                .await
            })
            .await;
            let pumps = std::mem::take(&mut *pumps.lock());
            let _ = this.update(cx, |v, cx| {
                let Some(peer) = v.peers.get_mut(&mid) else {
                    // Left while connecting; the link hangs up as it drops.
                    pumps.iter().for_each(|p| p.abort());
                    return;
                };
                match peer.watch.finish(generation, link, pumps, Instant::now()) {
                    Outcome::Stale => {}
                    Outcome::Attached => log::info!("voice from slot {mid}: connected"),
                    Outcome::Failed(e) => {
                        log::info!("voice from slot {mid}: {} ({})", e.message, e.code);
                        if e.code == ErrorCode::Unauthorized {
                            v.refresh_tokens(cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn connect_tile(&mut self, tile: &Entity<Tile>, cx: &mut Context<Self>) {
        let (generation, mid, kind, slot, sound) =
            tile.update(cx, |t, _| (t.watch.begin(), t.mid, t.kind, t.slot.clone(), t.sound.clone()));
        let api = self.session.read(cx).api.clone();
        let url = self.peer_url(mid, kind.path());
        let ice = self.ice(cx);
        let weak = tile.downgrade();
        let pumps: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Default::default();
        let pump_slot = pumps.clone();
        cx.spawn(async move |this, cx| {
            let link = core::run(async move {
                rtc::watch(&api, &url, &ice, true, move |track| {
                    if let Some(p) = super::video::pump(track, &slot, sound.as_ref()) {
                        pump_slot.lock().push(p);
                    }
                })
                .await
            })
            .await;
            let pumps = std::mem::take(&mut *pumps.lock());
            let _ = this.update(cx, |v, cx| {
                let Some(tile) = weak.upgrade() else {
                    pumps.iter().for_each(|p| p.abort());
                    return;
                };
                let outcome = tile.update(cx, |t, cx| {
                    let outcome = t.watch.finish(generation, link, pumps, Instant::now());
                    t.failed = t.watch.backoff.failures >= TILE_FAILED_AFTER;
                    cx.notify();
                    outcome
                });
                match outcome {
                    Outcome::Stale => {}
                    Outcome::Attached => log::info!("{} from slot {mid}: connected", kind.path()),
                    Outcome::Failed(e) => {
                        log::info!("{} from slot {mid}: {} ({})", kind.path(), e.message, e.code);
                        if e.code == ErrorCode::Unauthorized {
                            v.refresh_tokens(cx);
                        }
                    }
                }
            });
        })
        .detach();
    }

    fn refresh_tokens(&mut self, cx: &mut Context<Self>) {
        let rt = self.session.read(cx).realtime.clone();
        let channel = self.channel;
        cx.spawn(async move |this, cx| {
            if let Ok(v) = Session::request(rt, "voice:refresh", serde_json::json!({ "channelId": channel })).await
                && let Ok(t) = serde_json::from_value::<VoiceTokens>(v)
            {
                let _ = this.update(cx, |v, _| v.tokens = t);
            }
        })
        .detach();
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut speaking = HashSet::new();
        let me = self.session.read(cx).me.id;
        let a = audio::audio();
        let mine = a.mic.level.take();
        let mut levels: Vec<(UserId, f32)> =
            self.peers.values().map(|p| (p.user, p.source.peak.take() * p.source.gain.get().min(1.))).collect();
        levels.push((me, if self.muted || !a.mic.open.load(std::sync::atomic::Ordering::Relaxed) { 0. } else { mine }));
        for (user, level) in levels {
            let (env, held) = self.envelopes.entry(user).or_insert((0., now));
            *env = level.max(*env * 0.65);
            let was = self.speaking.contains(&user);
            if *env > SPEAK_ON {
                *held = now;
            }
            let on = *env > SPEAK_ON || (was && (*env > SPEAK_OFF || now.duration_since(*held) < Duration::from_millis(400)));
            if on {
                speaking.insert(user);
            }
        }
        if speaking != self.speaking {
            self.speaking = speaking;
            cx.notify();
        }
    }

    /// Whoever is due gets another attempt: subscriptions that failed or dropped, then what you
    /// publish.
    fn retry(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let due: Vec<i64> = self.peers.iter().filter(|(_, p)| p.watch.due(now)).map(|(mid, _)| *mid).collect();
        for mid in due {
            self.connect_peer(mid, Duration::ZERO, cx);
        }
        let due: Vec<Entity<Tile>> = self
            .tiles
            .iter()
            .filter(|t| {
                let t = t.read(cx);
                !t.local && t.watch.due(now)
            })
            .cloned()
            .collect();
        for tile in due {
            self.connect_tile(&tile, cx);
        }
        let camera_error = self.shares.camera.as_ref().and_then(|c| c.capture.error.lock().clone());
        if let Some(e) = camera_error {
            self.stop_camera(cx);
            crate::ui::overlay::toast(trf!("Your camera stopped: {}", "Sua câmera parou: {}", e), cx);
        }
        self.reconcile(cx);
    }

    /// Puts what you publish right: the microphone (one attempt at a time, never while an admin
    /// holds you muted), the camera and screen, and what the server is told about them.
    fn reconcile(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if self.mic.as_ref().is_some_and(|l| l.is_dead()) {
            log::info!("the microphone connection dropped; publishing again");
            self.drop_mic();
        }
        if self.force_muted && (self.mic.is_some() || self.mic_attempt.is_some()) {
            self.drop_mic();
        }
        if self.mic.is_none() && self.mic_attempt.is_none() && !self.force_muted && self.mic_backoff.ready(now) {
            self.start_mic(cx);
        }
        self.reconcile_shares(now, cx);
        self.announce(cx);
    }

    /// Tells the server whatever it has wrong about which of your paths are live, going by your
    /// entry in the roster.
    pub(super) fn announce(&mut self, cx: &mut Context<Self>) {
        let me = self.session.read(cx).me.id;
        // Not in the roster yet, or a roster from before a rejoin.
        let Some(mine) = self.roster.iter().find(|m| m.user_id == me && Some(m.mid) == self.mid) else { return };
        let live = [("v", self.mic.is_some()), ("c", self.share_live(TileKind::Camera)), ("s", self.share_live(TileKind::Screen))];
        let now = Instant::now();
        let rt = self.session.read(cx).realtime.clone();
        let channel = self.channel;
        for (kind, on) in publishing_fixes(live, &mine.publishing) {
            if self.announced.get(kind).is_some_and(|&(was, at)| was == on && now.duration_since(at) < ANNOUNCE_AGAIN) {
                continue;
            }
            self.announced.insert(kind, (on, now));
            log::info!("telling the server {kind} is {}", if on { "on" } else { "off" });
            let rt = rt.clone();
            cx.spawn(async move |this, cx| {
                let payload = serde_json::json!({ "channelId": channel, "kind": kind, "on": on });
                if let Err(e) = Session::request(rt, "voice:publishing", payload).await {
                    log::warn!("voice:publishing {kind} {on} failed: {e}");
                    let _ = this.update(cx, |v, _| v.announced.remove(kind));
                }
            })
            .detach();
        }
    }

    /// Stalls and ping, which need stats.
    fn check(&mut self, cx: &mut Context<Self>) {
        let mut links: Vec<(Watched, u64, Arc<Link>)> =
            self.peers.iter().filter_map(|(mid, p)| Some((Watched::Peer(*mid), p.watch.generation, p.watch.link.clone()?))).collect();
        for tile in &self.tiles {
            let t = tile.read(cx);
            if let Some(l) = t.watch.link.clone() {
                links.push((Watched::Tile(tile.downgrade()), t.watch.generation, l));
            }
        }
        let mic = self.mic.clone();
        cx.spawn(async move |this, cx| {
            let (counts, (rtt, route)) = core::run(async move {
                let mut counts = Vec::new();
                for (who, generation, l) in links {
                    counts.push((who, generation, l.packets_received().await));
                }
                let path = match mic {
                    Some(m) => (m.rtt_ms().await, m.route().await),
                    None => (None, None),
                };
                (counts, path)
            })
            .await;
            let _ = this.update(cx, |v, cx| {
                v.ping_ms = rtt;
                v.route = route;
                let now = Instant::now();
                for (who, generation, n) in counts {
                    match who {
                        Watched::Peer(mid) => {
                            if v.peers.get_mut(&mid).is_some_and(|p| p.watch.stalled(generation, n, now)) {
                                log::info!("voice from slot {mid} went quiet; subscribing again");
                                v.connect_peer(mid, Duration::ZERO, cx);
                            }
                        }
                        Watched::Tile(tile) => {
                            let Some(tile) = tile.upgrade() else { continue };
                            if tile.update(cx, |t, _| t.watch.stalled(generation, n, now)) {
                                log::info!("{} from slot {} went quiet; subscribing again", tile.read(cx).kind.path(), tile.read(cx).mid);
                                v.connect_tile(&tile, cx);
                            }
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Sets someone's volume (0..3.5) for you; 0 is "mute for me".
    pub fn set_volume(&mut self, user: UserId, gain: f32, cx: &mut Context<Self>) {
        let gain = gain.clamp(0., MAX_GAIN);
        volumes().lock().insert(user, gain);
        for p in self.peers.values().filter(|p| p.user == user) {
            p.source.gain.set(gain);
        }
        cx.notify();
    }

    /// Stops watching a tile until that stream ends or it is asked for again.
    pub fn close_tile(&mut self, tile: &Entity<Tile>, cx: &mut Context<Self>) {
        let (user, kind) = {
            let t = tile.read(cx);
            (t.user, t.kind)
        };
        self.closed.0.insert((user, kind));
        self.tiles.retain(|t| t != tile);
        tile.update(cx, |t, cx| t.close(cx));
        cx.notify();
    }

    pub fn reopen_tile(&mut self, user: UserId, kind: TileKind, cx: &mut Context<Self>) {
        self.closed.0.remove(&(user, kind));
        let roster = self.roster.clone();
        let me = self.session.read(cx).me.id;
        super::video::sync_tiles(self, &roster, me, cx);
        self.retry(cx);
        cx.notify();
    }

    pub fn tokens(&self) -> &VoiceTokens {
        &self.tokens
    }

    pub fn session(&self) -> &Entity<Session> {
        &self.session
    }

    pub fn mid(&self) -> Option<i64> {
        self.mid
    }

    pub fn shutdown(&mut self, cx: &mut Context<Self>) {
        // Leaving tells the server everything is off; nothing more to announce.
        self.roster.clear();
        self.stop_camera(cx);
        self.stop_screen(cx);
        self.drop_mic();
        self._mic_guard = None;
        for (_, p) in self.peers.drain() {
            close_peer(p);
        }
        for tile in self.tiles.drain(..) {
            tile.update(cx, |t, cx| t.close(cx));
        }
        self._slow = Task::ready(());
        audio::audio().mixer.deafened.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

fn close_peer(mut p: Peer) {
    audio::audio().mixer.remove(&p.source);
    p.watch.hang_up();
}

/// Input volume, noise gate and processing, from settings.
pub fn apply_mic_prefs(cx: &App) {
    apply_mic_levels(cx);
    let p = prefs(cx);
    let a = audio::audio();
    a.mic.echo_cancellation.store(p.echo_cancellation, std::sync::atomic::Ordering::Relaxed);
    a.mic.noise_suppression.store(p.noise_suppression, std::sync::atomic::Ordering::Relaxed);
    a.set_input(&p.voice_input_id);
    super::rtc::set_hardware_encoding(p.hardware_encoding != "off");
}

/// Gain and gate only: plain stores, cheap enough for every step of a slider drag.
pub fn apply_mic_levels(cx: &App) {
    let p = prefs(cx);
    let a = audio::audio();
    a.mic.gain.set(p.mic_gain as f32 / 100.);
    a.mic.gate.set(gate_threshold(p.mic_sensitivity));
}

/// The old client's gate: 0 is off, 1..100 maps to an RMS threshold.
pub fn gate_threshold(sensitivity: u32) -> f32 {
    if sensitivity == 0 { 0. } else { 0.0008 * 60f32.powf(sensitivity as f32 / 100.) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err() -> ApiError {
        ApiError { status: 404, code: ErrorCode::NotLive, message: "no stream".into(), body: serde_json::Value::Null }
    }

    #[test]
    fn backoff_doubles_to_eight_seconds() {
        let delays: Vec<u64> = (0..6).map(|n| Backoff::delay(n).as_secs()).collect();
        assert_eq!(delays, [0, 1, 2, 4, 8, 8]);
        let t = Instant::now();
        let mut b = Backoff::default();
        assert!(b.ready(t));
        b.fail(t);
        b.fail(t);
        assert!(!b.ready(t + Duration::from_millis(1900)));
        assert!(b.ready(t + Duration::from_secs(2)));
        b.reset();
        assert!(b.ready(t));
    }

    #[test]
    fn a_stale_attempt_is_not_attached() {
        let mut w = Watch::default();
        let first = w.begin();
        let second = w.begin();
        assert!(matches!(w.finish(first, Err(err()), Vec::new(), Instant::now()), Outcome::Stale));
        assert!(!w.due(Instant::now()), "the newer attempt is still in flight");
        assert!(matches!(w.finish(second, Err(err()), Vec::new(), Instant::now()), Outcome::Failed(_)));
        assert_eq!(w.backoff.failures, 1);
        // Hung up meanwhile: a late completion is stale too.
        let third = w.begin();
        w.hang_up();
        assert!(matches!(w.finish(third, Err(err()), Vec::new(), Instant::now()), Outcome::Stale));
    }

    #[test]
    fn failures_space_out_retries() {
        let t = Instant::now();
        let mut w = Watch::default();
        assert!(w.due(t));
        let g = w.begin();
        w.finish(g, Err(err()), Vec::new(), t);
        assert!(!w.due(t + Duration::from_millis(500)));
        assert!(w.due(t + Duration::from_secs(1)));
        let g = w.begin();
        w.finish(g, Err(err()), Vec::new(), t);
        assert!(!w.due(t + Duration::from_secs(1)));
        assert!(w.due(t + Duration::from_secs(2)));
    }

    #[test]
    fn a_stream_is_stalled_after_eight_quiet_seconds() {
        let t = Instant::now();
        let mut m = Meter::default();
        assert!(!m.stalled(40, t));
        assert!(!m.stalled(40, t + Duration::from_secs(4)), "quiet starts counting here");
        assert!(!m.stalled(40, t + Duration::from_secs(11)));
        assert!(m.stalled(40, t + Duration::from_secs(12)));
        assert!(!m.stalled(41, t + Duration::from_secs(13)), "a packet resets it");
        // Without a link there is nothing to stall.
        let mut w = Watch::default();
        assert!(!w.stalled(w.generation, 0, t) && !w.stalled(w.generation, 0, t + STALL * 2));
    }

    #[test]
    fn the_server_is_told_only_what_it_has_wrong() {
        let server = vec!["v".to_string(), "s".to_string()];
        assert_eq!(publishing_fixes([("v", true), ("c", true), ("s", false)], &server), [("c", true), ("s", false)]);
        assert!(publishing_fixes([("v", true), ("c", false), ("s", true)], &server).is_empty());
        // After a reconnect the server has nothing: everything live is announced again.
        assert_eq!(publishing_fixes([("v", true), ("c", true), ("s", false)], &[]), [("v", true), ("c", true)]);
    }
}
