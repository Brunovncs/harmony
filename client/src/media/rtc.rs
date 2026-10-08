//! Peer connections to MediaMTX. Every stream gets its own connection (MediaMTX can't
//! renegotiate), set up by WHIP to publish or WHEP to watch: the whole offer, ICE gathered first
//! (up to 4 s), POSTed once, and hung up with a DELETE on the session's URL.

use crate::core::api::{Api, ApiError};
use crate::core::types::{ErrorCode, IceServer};
use libwebrtc::MediaType;
use libwebrtc::audio_track::RtcAudioTrack;
use libwebrtc::media_stream_track::MediaStreamTrack;
use libwebrtc::peer_connection::{IceGatheringState, OfferOptions, PeerConnection, PeerConnectionState};
use libwebrtc::peer_connection_factory::{self as pcf, PeerConnectionFactory, RtcConfiguration};
use libwebrtc::rtp_parameters::{DegradationPreference, RtpCodecCapability};
use libwebrtc::rtp_transceiver::{RtpTransceiverDirection, RtpTransceiverInit};
use libwebrtc::session_description::{SdpType, SessionDescription};
use libwebrtc::stats::RtcStats;
use libwebrtc::video_track::RtcVideoTrack;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

static FACTORY: OnceLock<PeerConnectionFactory> = OnceLock::new();
static HARDWARE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether cameras and screens should use the graphics card's encoder when there is one.
pub fn set_hardware_encoding(on: bool) {
    HARDWARE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The encoder to ask for: the GPU's (NVENC, or whatever the build supports) unless turned off.
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

pub fn factory() -> &'static PeerConnectionFactory {
    FACTORY.get_or_init(PeerConnectionFactory::default)
}

fn config(ice: &[IceServer]) -> RtcConfiguration {
    let mut c = RtcConfiguration::default();
    c.ice_servers = ice
        .iter()
        .map(|s| pcf::IceServer {
            urls: s.urls.clone(),
            username: s.username.clone().unwrap_or_default(),
            password: s.credential.clone().unwrap_or_default(),
        })
        .collect();
    if c.ice_servers.is_empty() {
        c.ice_servers.push(pcf::IceServer {
            urls: vec!["stun:stun.l.google.com:19302".into()],
            username: String::new(),
            password: String::new(),
        });
    }
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

/// One live connection, publishing or watching. Dropping the last handle hangs it up.
pub struct Link {
    pub pc: PeerConnection,
    /// The session's URL on the media server, until it is hung up.
    resource: parking_lot::Mutex<Option<String>>,
    closed: std::sync::atomic::AtomicBool,
    api: Api,
}

impl Link {
    pub fn state(&self) -> PeerConnectionState {
        self.pc.connection_state()
    }

    /// Whether the connection is gone for good.
    pub fn is_dead(&self) -> bool {
        matches!(self.state(), PeerConnectionState::Failed | PeerConnectionState::Closed)
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
        self.stats().await.into_iter().find_map(|s| match s {
            RtcStats::CandidatePair(p) if p.candidate_pair.current_round_trip_time > 0. => {
                Some((p.candidate_pair.current_round_trip_time * 1000.).round() as u32)
            }
            _ => None,
        })
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

    /// Packets received over all inbound streams, to notice a connection gone quiet.
    pub async fn packets_received(&self) -> u64 {
        let stats = self.stats().await;
        if log::log_enabled!(log::Level::Debug) {
            let kinds: Vec<String> = stats.iter().map(|s| format!("{s:?}").chars().take(18).collect()).collect();
            let bytes: u64 = stats
                .iter()
                .map(|s| match s {
                    RtcStats::Transport(t) => t.transport.bytes_received,
                    _ => 0,
                })
                .sum();
            log::debug!("inbound stats: {} entries, transport bytes {bytes}, kinds {kinds:?}", stats.len());
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
/// Constrained Baseline; packetization mode 1 before 0; higher levels first. NVIDIA's encoder
/// refuses baseline, and level 3.1 caps a stream at about 720p30.
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
/// a slow or unreachable STUN server only gets until 1.5 s.
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

async fn negotiate(api: &Api, pc: PeerConnection, url: &str, gathering: Gathering, receive: bool) -> Result<Link, ApiError> {
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
    Ok(Link { pc, resource: parking_lot::Mutex::new(resource), closed: Default::default(), api: api.clone() })
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

/// Publishes a microphone and/or a video to a WHIP URL.
pub async fn publish(
    api: &Api,
    url: &str,
    ice: &[IceServer],
    audio: Option<(RtcAudioTrack, u64)>,
    video: Option<(RtcVideoTrack, VideoParams)>,
) -> Result<Link, ApiError> {
    let (pc, gathered) = new_pc(ice)?;
    let init = || RtpTransceiverInit {
        direction: RtpTransceiverDirection::SendOnly,
        stream_ids: vec!["harmony".into()],
        send_encodings: Vec::new(),
    };
    let fail = |m: String| ApiError { status: 0, code: ErrorCode::MediaError, message: m, body: Default::default() };
    let mut audio_bitrate = None;
    if let Some((track, bitrate)) = audio {
        pc.add_transceiver(MediaStreamTrack::Audio(track), init()).map_err(|e| fail(e.message))?;
        audio_bitrate = Some(bitrate);
    }
    let mut video_params = None;
    if let Some((track, params)) = video {
        let t = pc.add_transceiver(MediaStreamTrack::Video(track), init()).map_err(|e| fail(e.message))?;
        t.sender().set_video_encoder_backend(encoder_backend());
        let caps = factory().get_rtp_sender_capabilities(MediaType::Video);
        let _ = t.set_codec_preferences(prefer_h264(caps.codecs));
        video_params = Some(params);
    }
    let link = negotiate(api, pc, url, gathered, false).await?;
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

/// Watches a WHEP URL. Tracks arrive through `on_track` as the connection comes up.
pub async fn watch(
    api: &Api,
    url: &str,
    ice: &[IceServer],
    video: bool,
    on_track: impl FnMut(Track) + Send + 'static,
) -> Result<Link, ApiError> {
    let (pc, gathered) = new_pc(ice)?;
    let init = || RtpTransceiverInit { direction: RtpTransceiverDirection::RecvOnly, stream_ids: Vec::new(), send_encodings: Vec::new() };
    let fail = |m: String| ApiError { status: 0, code: ErrorCode::MediaError, message: m, body: Default::default() };
    if video {
        let t = pc.add_transceiver_for_media(MediaType::Video, init()).map_err(|e| fail(e.message))?;
        let caps = factory().get_rtp_receiver_capabilities(MediaType::Video);
        let _ = t.set_codec_preferences(prefer_h264(caps.codecs));
    }
    pc.add_transceiver_for_media(MediaType::Audio, init()).map_err(|e| fail(e.message))?;
    let link = negotiate(api, pc, url, gathered, true).await?;
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

    fn cap(mime: &str, fmtp: &str) -> RtpCodecCapability {
        RtpCodecCapability { channels: None, clock_rate: Some(90000), mime_type: mime.into(), sdp_fmtp_line: Some(fmtp.into()) }
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
