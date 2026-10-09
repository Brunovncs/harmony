//! Peer connections to MediaMTX. Every stream gets its own connection (MediaMTX can't
//! renegotiate), set up by WHIP to publish or WHEP to watch: the whole offer, ICE gathered first
//! (up to 4 s), POSTed once, and hung up with a DELETE on the session's URL.

use crate::core::api::{Api, ApiError};
use crate::core::types::{ErrorCode, IceServer};
use libwebrtc::MediaType;
use libwebrtc::audio_track::RtcAudioTrack;
use libwebrtc::media_stream_track::MediaStreamTrack;
use libwebrtc::native::frame_cryptor::{
    EncryptionAlgorithm, EncryptionState, FrameCryptor, KeyDerivationAlgorithm, KeyProvider, KeyProviderOptions,
};
use libwebrtc::peer_connection::{IceGatheringState, OfferOptions, PeerConnection, PeerConnectionState};
use libwebrtc::peer_connection_factory::{self as pcf, PeerConnectionFactory, RtcConfiguration};
use libwebrtc::rtp_parameters::{DegradationPreference, RtpCodecCapability};
use libwebrtc::rtp_transceiver::{RtpTransceiverDirection, RtpTransceiverInit};
use libwebrtc::session_description::{SdpType, SessionDescription};
use libwebrtc::stats::RtcStats;
use libwebrtc::video_track::RtcVideoTrack;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// The key a private call's media is sealed with, frame by frame, before it leaves this computer.
///
/// MediaMTX relays the RTP as always, but every audio and video frame in it is AES-GCM sealed
/// under this key (LiveKit's frame cryptor): the relay forwards what it cannot read. Codec headers
/// stay in the clear, which is all the relay needs to packetize and to find keyframes; measured
/// through MediaMTX for Opus, H.264 and VP8 before this was built. The key itself is random per
/// call and reaches the other person sealed inside the call's ring (`dm::Vault`).
#[derive(Clone)]
pub struct FrameKey(KeyProvider);

impl FrameKey {
    pub fn new(key: &[u8; 32]) -> FrameKey {
        let provider = KeyProvider::new(KeyProviderOptions {
            shared_key: true,
            // No ratcheting: a call's key never changes, and with a window every frame that fails
            // to open would cost a round of key derivation trying the next one.
            ratchet_window_size: 0,
            ratchet_salt: b"harmony-call-v1".to_vec(),
            // A frame that does not open is dropped, never played; no count makes the key invalid.
            failure_tolerance: -1,
            key_ring_size: 16,
            key_derivation_algorithm: KeyDerivationAlgorithm::PBKDF2,
        });
        provider.set_shared_key(0, key.to_vec());
        FrameKey(provider)
    }

    fn cryptor_for(&self, kind: Side) -> FrameCryptor {
        let fc = match kind {
            Side::Send(sender) => FrameCryptor::new_for_rtp_sender(factory(), "harmony".into(), EncryptionAlgorithm::AesGcm, self.0.clone(), sender),
            Side::Receive(receiver) => {
                FrameCryptor::new_for_rtp_receiver(factory(), "harmony".into(), EncryptionAlgorithm::AesGcm, self.0.clone(), receiver)
            }
        };
        fc.on_state_change(Some(Box::new(|_, state: EncryptionState| match state {
            EncryptionState::Ok | EncryptionState::New => {}
            other => log::warn!("call encryption: {other:?}"),
        })));
        fc.set_key_index(0);
        // The C++ side starts disabled.
        fc.set_enabled(true);
        fc
    }
}

enum Side {
    Send(libwebrtc::rtp_sender::RtpSender),
    Receive(libwebrtc::rtp_receiver::RtpReceiver),
}

static FACTORY: OnceLock<PeerConnectionFactory> = OnceLock::new();
static HARDWARE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether cameras and screens should use the graphics card's encoder when there is one.
pub fn set_hardware_encoding(on: bool) {
    HARDWARE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The encoder to ask for: the GPU's unless turned off. On Windows that is H.264 through Media
/// Foundation (the vendored webrtc-sys), which hands over to OpenH264 by itself if the hardware
/// fails; `watch_encoder` logs which one runs.
fn encoder_backend() -> libwebrtc::rtp_sender::VideoEncoderBackend {
    use libwebrtc::rtp_sender::VideoEncoderBackend as B;
    static AVAILABLE: OnceLock<Vec<B>> = OnceLock::new();
    let available = AVAILABLE.get_or_init(|| {
        let list: Vec<B> = B::list_available().into_iter().collect();
        log::info!("video encoders: {list:?}");
        list
    });
    if !HARDWARE.load(std::sync::atomic::Ordering::Relaxed) {
        return B::Software;
    }
    [B::Nvenc, B::Hardware].into_iter().find(|b| available.contains(b)).unwrap_or(B::Auto)
}

static HARDWARE_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the video encoder last seen running is the graphics card's, which takes NV12 frames
/// without converting them (OpenH264 converts NV12 to I420 first).
pub fn hardware_encoder_running() -> bool {
    HARDWARE_RUNNING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether to log video frame rates and timings (`HARMONY_VIDEO_STATS`).
pub fn video_stats() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("HARMONY_VIDEO_STATS").is_some())
}

pub fn factory() -> &'static PeerConnectionFactory {
    FACTORY.get_or_init(PeerConnectionFactory::default)
}

/// TURN relays only, never STUN. Every connection goes to the media server, which advertises its
/// own address and learns ours from the first packet, so a public address from STUN adds nothing,
/// and it costs: where STUN's IPv6 lookups fail, gathering never completes and every offer waited
/// out the timeout. Servers since 3.0.1 send none; this covers one that still does.
fn relays(ice: &[IceServer]) -> Vec<pcf::IceServer> {
    ice.iter()
        .filter_map(|s| {
            let urls: Vec<String> = s.urls.iter().filter(|u| !is_stun(u)).cloned().collect();
            (!urls.is_empty()).then(|| pcf::IceServer {
                urls,
                username: s.username.clone().unwrap_or_default(),
                password: s.credential.clone().unwrap_or_default(),
            })
        })
        .collect()
}

fn is_stun(url: &str) -> bool {
    let u = url.to_ascii_lowercase();
    u.starts_with("stun:") || u.starts_with("stuns:")
}

fn config(ice: &[IceServer]) -> RtcConfiguration {
    let mut c = RtcConfiguration::default();
    c.ice_servers = relays(ice);
    c.continual_gathering_policy = pcf::ContinualGatheringPolicy::GatherOnce;
    c
}

/// How a published video is encoded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoParams {
    pub max_bitrate: u64,
    pub max_fps: f64,
    /// Keep the resolution and drop frames when short of bandwidth (text stays sharp), or the
    /// other way around (motion stays smooth).
    pub sharp: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Straight to the server over UDP, the usual path.
    Udp,
    /// The TCP fallback, for networks that block UDP. Works, with more delay.
    Tcp,
    /// Through a TURN relay.
    Relay,
}

/// The round trip of the nominated candidate pair, the one the media takes; the others only
/// carry connectivity checks.
fn nominated_rtt_ms(stats: &[RtcStats]) -> Option<u32> {
    stats.iter().find_map(|s| match s {
        RtcStats::CandidatePair(p) if p.candidate_pair.nominated && p.candidate_pair.current_round_trip_time > 0. => {
            Some((p.candidate_pair.current_round_trip_time * 1000.).round() as u32)
        }
        _ => None,
    })
}

/// One live connection, publishing or watching. Dropping the last handle hangs it up.
pub struct Link {
    pub pc: PeerConnection,
    /// The session's URL on the media server, until it is hung up.
    resource: parking_lot::Mutex<Option<String>>,
    closed: std::sync::atomic::AtomicBool,
    /// Since when the connection has been `Disconnected`, as last looked at by `is_stuck`.
    down_since: parking_lot::Mutex<Option<std::time::Instant>>,
    api: Api,
    /// A private call's frame cryptors, one per track: they live exactly as long as the connection.
    _cryptors: Vec<FrameCryptor>,
}

impl Link {
    pub fn state(&self) -> PeerConnectionState {
        self.pc.connection_state()
    }

    /// Whether the connection is gone for good.
    pub fn is_dead(&self) -> bool {
        matches!(self.state(), PeerConnectionState::Failed | PeerConnectionState::Closed)
    }

    /// Gone for good, or `Disconnected` for longer than `limit`. ICE comes back from a short
    /// disconnect by itself, but one that lasts can sit there without ever reaching `Failed`, and a
    /// publish stuck in it reaches nobody. Only as accurate as how often it is asked.
    pub fn is_stuck(&self, limit: Duration) -> bool {
        let mut since = self.down_since.lock();
        match self.state() {
            PeerConnectionState::Failed | PeerConnectionState::Closed => true,
            PeerConnectionState::Disconnected => since.get_or_insert_with(std::time::Instant::now).elapsed() >= limit,
            _ => {
                *since = None;
                false
            }
        }
    }

    /// Waits for ICE and DTLS to finish, up to `limit`. False if it failed or never came up.
    pub async fn connected(&self, limit: Duration) -> bool {
        let start = std::time::Instant::now();
        loop {
            match self.state() {
                PeerConnectionState::Connected => return true,
                PeerConnectionState::Failed | PeerConnectionState::Closed => return false,
                _ if start.elapsed() >= limit => return false,
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    /// Changes the encoding of the published video while it runs.
    pub fn set_video_params(&self, params: VideoParams) {
        for sender in self.pc.senders() {
            if matches!(sender.track(), Some(MediaStreamTrack::Video(_))) {
                apply_params(&sender, params);
            }
        }
    }

    pub async fn stats(&self) -> Vec<RtcStats> {
        self.pc.get_stats().await.unwrap_or_default()
    }

    /// The round trip of the connection's active candidate pair, in ms.
    pub async fn rtt_ms(&self) -> Option<u32> {
        nominated_rtt_ms(&self.stats().await)
    }

    /// How the media travels, from the nominated candidate pair.
    pub async fn route(&self) -> Option<Route> {
        let stats = self.stats().await;
        let pair = stats.iter().find_map(|s| match s {
            RtcStats::CandidatePair(p) if p.candidate_pair.nominated => Some(&p.candidate_pair),
            _ => None,
        })?;
        let ends: Vec<_> = stats
            .iter()
            .filter_map(|s| match s {
                RtcStats::LocalCandidate(c) if c.rtc.id == pair.local_candidate_id => Some(&c.local_candidate),
                RtcStats::RemoteCandidate(c) if c.rtc.id == pair.remote_candidate_id => Some(&c.remote_candidate),
                _ => None,
            })
            .collect();
        Some(if ends.iter().any(|c| c.candidate_type == Some(libwebrtc::stats::IceCandidateType::Relay)) {
            Route::Relay
        } else if ends.iter().any(|c| c.protocol == "tcp") {
            Route::Tcp
        } else {
            Route::Udp
        })
    }

    /// With `HARMONY_VIDEO_STATS` set, logs what the video encoder or decoder of this connection
    /// is doing, for working out where frames go missing.
    pub async fn log_video_stats(&self) {
        if !video_stats() {
            return;
        }
        for s in self.stats().await {
            match s {
                RtcStats::MediaSource(m) if m.source.kind == "video" => {
                    log::info!(
                        "video source: {}x{} {} fps, {} frames",
                        m.video.width,
                        m.video.height,
                        m.video.frames_per_second,
                        m.video.frames
                    )
                }
                RtcStats::OutboundRtp(o) if o.stream.kind == "video" => {
                    let e = &o.outbound;
                    log::info!(
                        "video out: {}x{} {} fps, {} encoded, {} sent, {} bytes, target {:.0} kbps, limited by {:?} {:?}, {}",
                        e.frame_width,
                        e.frame_height,
                        e.frames_per_second,
                        e.frames_encoded,
                        e.frames_sent,
                        o.sent.bytes_sent,
                        e.target_bitrate / 1000.,
                        e.quality_limitation_reason,
                        e.quality_limitation_durations,
                        e.encoder_implementation
                    )
                }
                RtcStats::InboundRtp(i) if i.stream.kind == "video" => {
                    let d = &i.inbound;
                    log::info!(
                        "video in: {}x{} {} fps, {} decoded ({:.2} ms each), {} dropped, {} freezes, {} bytes, {}",
                        d.frame_width,
                        d.frame_height,
                        d.frames_per_second,
                        d.frames_decoded,
                        d.total_decode_time * 1000. / d.frames_decoded.max(1) as f64,
                        d.frames_dropped,
                        d.freeze_count,
                        d.bytes_received,
                        d.decoder_implementation
                    )
                }
                _ => {}
            }
        }
    }

    /// Packets sent over all outbound streams.
    pub async fn packets_sent(&self) -> u64 {
        self.stats()
            .await
            .into_iter()
            .map(|s| match s {
                RtcStats::OutboundRtp(o) => o.sent.packets_sent,
                _ => 0,
            })
            .sum()
    }

    /// Packets received over all inbound streams, to notice a connection gone quiet.
    pub async fn packets_received(&self) -> u64 {
        let stats = self.stats().await;
        if log::log_enabled!(log::Level::Debug) {
            let rtt = nominated_rtt_ms(&stats);
            for s in &stats {
                let RtcStats::InboundRtp(i) = s else { continue };
                let (r, d) = (&i.received, &i.inbound);
                let buffer = if d.jitter_buffer_emitted_count > 0 { d.jitter_buffer_delay / d.jitter_buffer_emitted_count as f64 } else { 0. };
                let concealed = if d.total_samples_received > 0 { d.concealed_samples as f64 / d.total_samples_received as f64 } else { 0. };
                log::debug!(
                    "inbound {}: {} packets, {} lost, jitter {:.0} ms, buffer {:.0} ms, concealed {:.1}%, rtt {rtt:?} ms",
                    i.stream.kind,
                    r.packets_received,
                    r.packets_lost,
                    r.jitter * 1000.,
                    buffer * 1000.,
                    concealed * 100.
                );
            }
        }
        stats
            .into_iter()
            .map(|s| match s {
                RtcStats::InboundRtp(i) => i.received.packets_received,
                _ => 0,
            })
            .sum()
    }

    /// Hangs up: the connection closes and the media server is told, in the background. Safe
    /// to call more than once.
    pub fn close(&self) {
        if self.closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let (pc, url, api) = (self.pc.clone(), self.resource.lock().take(), self.api.clone());
        let task = crate::core::runtime().spawn(async move {
            pc.close();
            if let Some(url) = url {
                api.delete_resource(&url).await;
            }
        });
        let mut pending = hang_ups().lock();
        pending.retain(|t| !t.is_finished());
        pending.push(task);
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.close();
    }
}

/// Hang-ups still on their way to the media server, so quitting can wait for them.
fn hang_ups() -> &'static parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>> {
    static H: OnceLock<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>> = OnceLock::new();
    H.get_or_init(Default::default)
}

/// Waits, up to `limit`, for every hang-up so far to reach the media server. A slot the server
/// still thinks is live refuses the next publish, so quitting should not skip this.
pub async fn flush_hang_ups(limit: Duration) {
    let pending = std::mem::take(&mut *hang_ups().lock());
    let _ = tokio::time::timeout(limit, futures::future::join_all(pending)).await;
}

fn apply_params(sender: &libwebrtc::rtp_sender::RtpSender, p: VideoParams) {
    let mut params = sender.parameters();
    for e in params.encodings.iter_mut() {
        e.max_bitrate = Some(p.max_bitrate);
        e.max_framerate = Some(p.max_fps);
    }
    params.set_degradation_preference(if p.sharp { DegradationPreference::MaintainResolution } else { DegradationPreference::Balanced });
    if let Err(e) = sender.set_parameters(params) {
        log::debug!("sender parameters: {}", e.message);
    }
}

/// H.264 first, in the order hardware encoders and decoders like: High, then Main, then
/// Constrained Baseline; packetization mode 1 before 0; higher levels first. (Chromium's NVIDIA
/// path refused baseline, and level 3.1 caps a stream at about 720p30.) MediaMTX's WHIP answer
/// takes Baseline whatever comes first, which the Media Foundation encoders on NVIDIA and AMD
/// both encode.
pub fn prefer_h264(codecs: Vec<RtpCodecCapability>) -> Vec<RtpCodecCapability> {
    fn fmtp(c: &RtpCodecCapability, key: &str) -> Option<String> {
        c.sdp_fmtp_line.as_deref()?.split(';').find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == key).then(|| v.to_lowercase())
        })
    }
    fn rank(c: &RtpCodecCapability) -> (u8, u8, i32) {
        let pli = fmtp(c, "profile-level-id").unwrap_or_default();
        let profile = match pli.get(0..2) {
            Some("64") => 0,
            Some("4d") => 1,
            Some("42") => 2,
            _ => 3,
        };
        let mode = if fmtp(c, "packetization-mode").as_deref() == Some("1") { 0 } else { 1 };
        let level = pli.get(4..6).and_then(|l| i32::from_str_radix(l, 16).ok()).unwrap_or(0);
        (profile, mode, -level)
    }
    let (mut h264, rest): (Vec<_>, Vec<_>) = codecs.into_iter().partition(|c| c.mime_type.eq_ignore_ascii_case("video/H264"));
    h264.sort_by_key(rank);
    h264.into_iter().chain(rest).collect()
}

type Candidates = Arc<parking_lot::Mutex<Vec<(usize, String)>>>;

struct Gathering {
    done: Arc<tokio::sync::Notify>,
    candidates: Candidates,
}

/// Waits for gathering to finish, up to 4 s as the old client did; once host candidates are in,
/// a slow TURN server only gets until 1.5 s. Without STUN or TURN it completes in a blink.
async fn wait_for_ice(pc: &PeerConnection, g: &Gathering) {
    let start = std::time::Instant::now();
    loop {
        if pc.ice_gathering_state() == IceGatheringState::Complete {
            return;
        }
        let have = !g.candidates.lock().is_empty();
        let elapsed = start.elapsed();
        if elapsed >= Duration::from_secs(4) || (have && elapsed >= Duration::from_millis(1500)) {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_millis(100), g.done.notified()).await;
    }
}

/// The offer with every gathered candidate written into its media sections, since it goes out
/// whole rather than trickled. (The bindings only expose the description once it is current,
/// which for an offer is after the answer.)
pub fn with_candidates(sdp: &str, candidates: &[(usize, String)]) -> String {
    let mut out = String::with_capacity(sdp.len() + candidates.len() * 100);
    let mut section: Option<usize> = None;
    let flush = |out: &mut String, section: Option<usize>| {
        if let Some(i) = section {
            for (_, c) in candidates.iter().filter(|(m, _)| *m == i) {
                let c = c.trim_start_matches("a=");
                out.push_str("a=");
                out.push_str(c);
                out.push_str("\r\n");
            }
            out.push_str("a=end-of-candidates\r\n");
        }
    };
    for line in sdp.split("\r\n").flat_map(|l| l.split('\n')).filter(|l| !l.is_empty()) {
        if line.starts_with("m=") {
            flush(&mut out, section);
            section = Some(section.map_or(0, |i| i + 1));
        }
        if line.starts_with("a=candidate:") || line == "a=end-of-candidates" {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    flush(&mut out, section);
    out
}

async fn negotiate(
    api: &Api,
    pc: PeerConnection,
    url: &str,
    gathering: Gathering,
    receive: bool,
    cryptors: Vec<FrameCryptor>,
) -> Result<Link, ApiError> {
    let fail = |m: String| ApiError { status: 0, code: ErrorCode::MediaError, message: m, body: Default::default() };
    // The bindings always pass the legacy offerToReceive options, and `false` turns receiving
    // transceivers inactive, so watching has to ask for it.
    let options = OfferOptions { ice_restart: false, offer_to_receive_audio: receive, offer_to_receive_video: receive };
    let offer = pc.create_offer(options).await.map_err(|e| fail(e.message))?;
    let offer_sdp = offer.to_string();
    pc.set_local_description(offer).await.map_err(|e| fail(e.message))?;
    wait_for_ice(&pc, &gathering).await;
    let sdp = with_candidates(&offer_sdp, &gathering.candidates.lock());
    let (answer, resource) = match api.sdp_exchange(url, &sdp).await {
        Ok(a) => a,
        Err(e) => {
            pc.close();
            return Err(e);
        }
    };
    if std::env::var_os("HARMONY_SDP_DEBUG").is_some() {
        log::info!("offer to {url}:\n{sdp}\nanswer:\n{answer}");
    }
    let answer = SessionDescription::parse(&answer, SdpType::Answer).map_err(|e| fail(e.description))?;
    if let Err(e) = pc.set_remote_description(answer).await {
        pc.close();
        if let Some(r) = &resource {
            api.delete_resource(r).await;
        }
        return Err(fail(e.message));
    }
    Ok(Link {
        pc,
        resource: parking_lot::Mutex::new(resource),
        closed: Default::default(),
        down_since: Default::default(),
        api: api.clone(),
        _cryptors: cryptors,
    })
}

fn new_pc(ice: &[IceServer]) -> Result<(PeerConnection, Gathering), ApiError> {
    let pc = factory().create_peer_connection(config(ice)).map_err(|e| ApiError {
        status: 0,
        code: ErrorCode::MediaError,
        message: e.message,
        body: Default::default(),
    })?;
    let done = Arc::new(tokio::sync::Notify::new());
    let candidates: Candidates = Default::default();
    let g = done.clone();
    pc.on_ice_gathering_state_change(Some(Box::new(move |s| {
        if s == IceGatheringState::Complete {
            g.notify_one();
        }
    })));
    let c = candidates.clone();
    pc.on_ice_candidate(Some(Box::new(move |cand| {
        c.lock().push((cand.sdp_mline_index().max(0) as usize, cand.candidate()));
    })));
    Ok((pc, Gathering { done, candidates }))
}

/// Publishes a microphone and/or a video to a WHIP URL; with a `FrameKey`, every frame is sealed
/// before it leaves, from the very first one.
pub async fn publish(
    api: &Api,
    url: &str,
    ice: &[IceServer],
    audio: Option<(RtcAudioTrack, u64)>,
    video: Option<(RtcVideoTrack, VideoParams)>,
    key: Option<&FrameKey>,
) -> Result<Link, ApiError> {
    let (pc, gathered) = new_pc(ice)?;
    let init = || RtpTransceiverInit {
        direction: RtpTransceiverDirection::SendOnly,
        stream_ids: vec!["harmony".into()],
        send_encodings: Vec::new(),
    };
    let fail = |m: String| ApiError { status: 0, code: ErrorCode::MediaError, message: m, body: Default::default() };
    let mut cryptors = Vec::new();
    let mut audio_bitrate = None;
    if let Some((track, bitrate)) = audio {
        let t = pc.add_transceiver(MediaStreamTrack::Audio(track), init()).map_err(|e| fail(e.message))?;
        cryptors.extend(key.map(|k| k.cryptor_for(Side::Send(t.sender()))));
        audio_bitrate = Some(bitrate);
    }
    let mut video_params = None;
    if let Some((track, params)) = video {
        let t = pc.add_transceiver(MediaStreamTrack::Video(track), init()).map_err(|e| fail(e.message))?;
        t.sender().set_video_encoder_backend(encoder_backend());
        let caps = factory().get_rtp_sender_capabilities(MediaType::Video);
        let _ = t.set_codec_preferences(prefer_h264(caps.codecs));
        cryptors.extend(key.map(|k| k.cryptor_for(Side::Send(t.sender()))));
        video_params = Some(params);
    }
    let link = negotiate(api, pc, url, gathered, false, cryptors).await?;
    if video_params.is_some() {
        watch_encoder(link.pc.clone());
    }
    // Encoder limits go on after the answer, once the sender has its encodings.
    for sender in link.pc.senders() {
        match sender.track() {
            Some(MediaStreamTrack::Video(_)) => {
                if let Some(p) = video_params {
                    apply_params(&sender, p);
                }
            }
            Some(MediaStreamTrack::Audio(_)) => {
                if let Some(b) = audio_bitrate {
                    let mut params = sender.parameters();
                    for e in params.encodings.iter_mut() {
                        e.max_bitrate = Some(b);
                    }
                    let _ = sender.set_parameters(params);
                }
            }
            None => {}
        }
    }
    Ok(link)
}

/// Logs the video encoder that actually runs once it is known, and again whenever it changes: the
/// graphics card's giving way to the software one shows up here. Ends with the connection.
fn watch_encoder(pc: PeerConnection) {
    crate::core::runtime().spawn(async move {
        let mut last = String::new();
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if matches!(pc.connection_state(), PeerConnectionState::Failed | PeerConnectionState::Closed) {
                return;
            }
            let Ok(stats) = pc.get_stats().await else { return };
            for s in stats {
                if let RtcStats::OutboundRtp(o) = s
                    && o.stream.kind == "video"
                    && !o.outbound.encoder_implementation.is_empty()
                    && o.outbound.encoder_implementation != last
                {
                    let e = &o.outbound;
                    let kind = if e.power_efficient_encoder { "hardware" } else { "software" };
                    log::info!(
                        "video encoder: {} ({kind}, {}x{} at {} fps)",
                        e.encoder_implementation,
                        e.frame_width,
                        e.frame_height,
                        e.frames_per_second
                    );
                    HARDWARE_RUNNING.store(e.power_efficient_encoder, std::sync::atomic::Ordering::Relaxed);
                    last = e.encoder_implementation.clone();
                }
            }
        }
    });
}

/// The kinds of ICE candidate this machine can offer (host, srflx, relay), by gathering for a
/// throwaway connection.
pub async fn candidate_types(ice: &[IceServer]) -> Vec<String> {
    let Ok((pc, gathering)) = new_pc(ice) else { return Vec::new() };
    let init = RtpTransceiverInit { direction: RtpTransceiverDirection::RecvOnly, stream_ids: Vec::new(), send_encodings: Vec::new() };
    if pc.add_transceiver_for_media(MediaType::Video, init).is_err() {
        return Vec::new();
    }
    let options = OfferOptions { ice_restart: false, offer_to_receive_audio: false, offer_to_receive_video: true };
    if let Ok(offer) = pc.create_offer(options).await
        && pc.set_local_description(offer).await.is_ok()
    {
        // Gathering in full, as the old test did: STUN gets its six seconds here.
        let start = std::time::Instant::now();
        while pc.ice_gathering_state() != IceGatheringState::Complete && start.elapsed() < Duration::from_secs(6) {
            let _ = tokio::time::timeout(Duration::from_millis(100), gathering.done.notified()).await;
        }
    }
    pc.close();
    let mut types: Vec<String> = gathering
        .candidates
        .lock()
        .iter()
        .filter_map(|(_, c)| c.split_whitespace().skip_while(|w| *w != "typ").nth(1).map(str::to_string))
        .collect();
    types.sort();
    types.dedup();
    types
}

pub enum Track {
    Audio(RtcAudioTrack),
    Video(RtcVideoTrack),
}

/// Watches a WHEP URL. Tracks arrive through `on_track` as the connection comes up. With a
/// `FrameKey`, frames are opened as they arrive; one that does not open is never played.
pub async fn watch(
    api: &Api,
    url: &str,
    ice: &[IceServer],
    video: bool,
    key: Option<&FrameKey>,
    on_track: impl FnMut(Track) + Send + 'static,
) -> Result<Link, ApiError> {
    let (pc, gathered) = new_pc(ice)?;
    let init = || RtpTransceiverInit { direction: RtpTransceiverDirection::RecvOnly, stream_ids: Vec::new(), send_encodings: Vec::new() };
    let fail = |m: String| ApiError { status: 0, code: ErrorCode::MediaError, message: m, body: Default::default() };
    let mut cryptors = Vec::new();
    if video {
        let t = pc.add_transceiver_for_media(MediaType::Video, init()).map_err(|e| fail(e.message))?;
        let caps = factory().get_rtp_receiver_capabilities(MediaType::Video);
        let _ = t.set_codec_preferences(prefer_h264(caps.codecs));
        cryptors.extend(key.map(|k| k.cryptor_for(Side::Receive(t.receiver()))));
    }
    let t = pc.add_transceiver_for_media(MediaType::Audio, init()).map_err(|e| fail(e.message))?;
    cryptors.extend(key.map(|k| k.cryptor_for(Side::Receive(t.receiver()))));
    let link = negotiate(api, pc, url, gathered, true, cryptors).await?;
    // Each receiving transceiver has its track from the start; hand them over once connected.
    let mut on_track = on_track;
    log::debug!("{} transceivers", link.pc.transceivers().len());
    for t in link.pc.transceivers() {
        log::info!("transceiver {:?}: track {}", t.mid(), t.receiver().track().is_some());
        match t.receiver().track() {
            Some(MediaStreamTrack::Audio(a)) => on_track(Track::Audio(a)),
            Some(MediaStreamTrack::Video(v)) => on_track(Track::Video(v)),
            None => {}
        }
    }
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libwebrtc::rtp_sender::VideoEncoderBackend as Backend;

    fn cap(mime: &str, fmtp: &str) -> RtpCodecCapability {
        RtpCodecCapability { channels: None, clock_rate: Some(90000), mime_type: mime.into(), sdp_fmtp_line: Some(fmtp.into()) }
    }

    /// CI's Windows runners have no graphics card, so no hardware encoder to find.
    fn no_gpu_here() -> bool {
        std::env::var_os("CI").is_some()
    }

    fn hardware_listed() -> bool {
        let _ = factory();
        Backend::list_available().into_iter().any(|b| b == Backend::Hardware)
    }

    #[test]
    fn hardware_encoder_is_listed() {
        if no_gpu_here() {
            return;
        }
        assert!(hardware_listed(), "no hardware H.264 encoder found: {:?}", Backend::list_available().into_iter().collect::<Vec<_>>());
    }

    /// A sender asking for the hardware encoder and a receiver, in this process: the stream has
    /// to arrive decoded, and from the Media Foundation encoder rather than OpenH264.
    #[test]
    fn hardware_encoder_streams() {
        use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
        use libwebrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
        use libwebrtc::video_source::VideoResolution;
        use libwebrtc::video_source::native::NativeVideoSource;
        let _ = env_logger::builder().is_test(true).try_init();
        if no_gpu_here() || !hardware_listed() {
            return;
        }
        let (w, h) = (1280u32, 720u32);
        let _rt = crate::core::runtime().enter();
        let source = NativeVideoSource::new(VideoResolution { width: w, height: h }, true);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let feeder = {
            let (source, stop) = (source.clone(), stop.clone());
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                for n in 0u32.. {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let mut buffer = I420Buffer::new(w, h);
                    let (sy, _, _) = buffer.strides();
                    let (y, u, v) = buffer.data_mut();
                    for (row, line) in y.chunks_mut(sy as usize).enumerate() {
                        for (col, p) in line.iter_mut().enumerate() {
                            *p = ((col as u32 + n * 8) ^ row as u32) as u8;
                        }
                    }
                    u.fill(90);
                    v.fill(160);
                    let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
                    frame.timestamp_us = start.elapsed().as_micros() as i64;
                    source.capture_frame(&frame);
                    std::thread::sleep(Duration::from_millis(33));
                }
            })
        };

        let (sent, bitrate, received) = crate::core::runtime().block_on(async {
            let (tx, tx_ice) = new_pc(&[]).unwrap();
            let (rx, rx_ice) = new_pc(&[]).unwrap();
            let track = factory().create_video_track("loopback", source.clone());
            let init = RtpTransceiverInit {
                direction: RtpTransceiverDirection::SendOnly,
                stream_ids: vec!["t".into()],
                send_encodings: Vec::new(),
            };
            let t = tx.add_transceiver(MediaStreamTrack::Video(track), init).unwrap();
            t.sender().set_video_encoder_backend(Backend::Hardware);
            // Baseline, which is all MediaMTX's WHIP takes, and the profile hardware encoders
            // are fussiest about.
            let baseline = factory()
                .get_rtp_sender_capabilities(MediaType::Video)
                .codecs
                .into_iter()
                .filter(|c| c.sdp_fmtp_line.as_deref().is_some_and(|f| f.contains("packetization-mode=1;profile-level-id=42")))
                .collect();
            t.set_codec_preferences(baseline).unwrap();

            let offer = tx.create_offer(OfferOptions::default()).await.unwrap();
            let offer_sdp = offer.to_string();
            tx.set_local_description(offer).await.unwrap();
            wait_for_ice(&tx, &tx_ice).await;
            let offer = SessionDescription::parse(&with_candidates(&offer_sdp, &tx_ice.candidates.lock()), SdpType::Offer).unwrap();
            rx.set_remote_description(offer).await.unwrap();
            let answer = rx.create_answer(Default::default()).await.unwrap();
            let answer_sdp = answer.to_string();
            rx.set_local_description(answer).await.unwrap();
            wait_for_ice(&rx, &rx_ice).await;
            let answer = SessionDescription::parse(&with_candidates(&answer_sdp, &rx_ice.candidates.lock()), SdpType::Answer).unwrap();
            tx.set_remote_description(answer).await.unwrap();
            // A cap the busy test picture would blow through if the rates did not reach the
            // transform.
            apply_params(&t.sender(), VideoParams { max_bitrate: 1_000_000, max_fps: 30., sharp: true });

            let outbound = async || {
                tx.get_stats().await.unwrap().into_iter().find_map(|s| match s {
                    RtcStats::OutboundRtp(o) if o.stream.kind == "video" => Some(o),
                    _ => None,
                })
            };
            tokio::time::sleep(Duration::from_secs(4)).await;
            let before = outbound().await.expect("no outbound video").sent.bytes_sent;
            tokio::time::sleep(Duration::from_secs(3)).await;
            let sent = outbound().await.expect("no outbound video");
            let bitrate = (sent.sent.bytes_sent - before) * 8 / 3;
            let received = rx.get_stats().await.unwrap().into_iter().find_map(|s| match s {
                RtcStats::InboundRtp(i) if i.stream.kind == "video" => Some(i.inbound),
                _ => None,
            });
            tx.close();
            rx.close();
            (sent.outbound, bitrate, received.expect("no inbound video"))
        });
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        feeder.join().unwrap();

        eprintln!(
            "{}: {bitrate} bps, {} key frames; {} frames decoded by {} at {:.2} ms each",
            sent.encoder_implementation,
            sent.key_frames_encoded,
            received.frames_decoded,
            received.decoder_implementation,
            received.total_decode_time * 1000. / received.frames_decoded.max(1) as f64
        );
        assert!(sent.encoder_implementation.starts_with("MediaFoundation"), "encoded by {}", sent.encoder_implementation);
        assert!(sent.power_efficient_encoder);
        assert!(sent.key_frames_encoded >= 1);
        assert!(received.frames_decoded >= 120, "{} frames decoded", received.frames_decoded);
        assert!((200_000..1_300_000).contains(&bitrate), "{bitrate} bps against a 1 Mbps cap");
        assert_eq!((received.frame_width, received.frame_height), (w, h));
    }

    #[test]
    fn only_turn_survives() {
        let server = |urls: &[&str]| IceServer {
            urls: urls.iter().map(|u| u.to_string()).collect(),
            username: Some("u".into()),
            credential: Some("p".into()),
        };
        let out = relays(&[
            server(&["stun:stun.l.google.com:19302", "STUNS:x:5349"]),
            server(&["stun:a:3478", "turn:relay.example:3478?transport=udp"]),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].urls, vec!["turn:relay.example:3478?transport=udp".to_string()]);
        assert_eq!(out[0].username, "u");
    }

    #[test]
    fn candidates_land_in_their_sections() {
        let sdp = "v=0\r\nm=audio 9 UDP 111\r\na=mid:0\r\nm=video 9 UDP 96\r\na=mid:1\r\n";
        let out = with_candidates(
            sdp,
            &[(0, "candidate:1 1 udp 1 10.0.0.1 5000 typ host".into()), (1, "candidate:2 1 udp 1 10.0.0.1 5002 typ host".into())],
        );
        assert_eq!(
            out,
            "v=0\r\nm=audio 9 UDP 111\r\na=mid:0\r\na=candidate:1 1 udp 1 10.0.0.1 5000 typ host\r\na=end-of-candidates\r\nm=video 9 UDP 96\r\na=mid:1\r\na=candidate:2 1 udp 1 10.0.0.1 5002 typ host\r\na=end-of-candidates\r\n"
        );
    }

    #[test]
    fn h264_high_profile_and_mode_one_come_first() {
        let sorted = prefer_h264(vec![
            cap("video/VP8", ""),
            cap("video/H264", "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f"),
            cap("video/H264", "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"),
            cap("video/H264", "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c1f"),
            cap("video/H264", "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c34"),
        ]);
        let ids: Vec<_> = sorted.iter().map(|c| c.sdp_fmtp_line.clone().unwrap_or_default()).collect();
        assert!(ids[0].contains("640c34"));
        assert!(ids[1].contains("640c1f"));
        assert!(ids[2].contains("packetization-mode=1;profile-level-id=42e01f"));
        assert_eq!(sorted.last().unwrap().mime_type, "video/VP8");
    }
}
