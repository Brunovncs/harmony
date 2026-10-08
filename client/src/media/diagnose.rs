//! "Test the connection": when someone can't stream, where does it break? The control server,
//! STUN, or the media path look the same from outside. This walks the path a real share takes,
//! with a tiny throwaway stream on your own camera path, and says which step failed.

use super::rtc::{self, VideoParams};
use crate::core::api::Api;
use libwebrtc::peer_connection::PeerConnectionState;
use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use libwebrtc::stats::RtcStats;
use libwebrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use libwebrtc::video_source::VideoResolution;
use libwebrtc::video_source::native::NativeVideoSource;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Step {
    pub name: &'static str,
    /// `None` while it runs.
    pub ok: Option<bool>,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub ok: bool,
    pub verdict: String,
}

/// Runs the test, reporting each step as it starts and ends.
pub async fn run(api: Api, report: impl Fn(Step) + Send + Sync) -> Outcome {
    let step = |name, ok, detail: String| report(Step { name, ok, detail });
    let fail = |verdict: &str| Outcome { ok: false, verdict: verdict.into() };
    // Each name is picked once, so a step's start and end match even if the language changes
    // halfway.
    let reach = tr!("Reach the Harmony server", "Acessar o servidor do Harmony");
    let media = tr!("Media server is running", "Servidor de mídia no ar");
    let reserve = tr!("Reserve a test stream", "Reservar uma transmissão de teste");
    let stun = tr!("Discover your public address (STUN)", "Descobrir seu endereço público (STUN)");
    let send = tr!("Send video to the server", "Enviar vídeo ao servidor");

    step(reach, None, String::new());
    let health = match api.health().await {
        Ok(h) => h,
        Err(e) => {
            step(reach, Some(false), e.message);
            return fail(tr!(
                "The server could not be reached. Check the address, and that your network allows its port.",
                "Não foi possível acessar o servidor. Confira o endereço e se a sua rede libera a porta dele."
            ));
        }
    };
    step(reach, Some(true), health.signaling_base.clone().map(|s| trf!("signaling on {}", "sinalização em {}", s)).unwrap_or_default());
    if !health.ok {
        step(media, Some(false), format!("mediamtx: {}", health.mediamtx.unwrap_or_default()));
        return fail(tr!(
            "The server is up but its media server is not. That is for whoever runs the server to fix.",
            "O servidor está no ar, mas o servidor de mídia dele não. Quem cuida do servidor precisa resolver isso."
        ));
    }
    step(media, Some(true), String::new());

    step(reserve, None, String::new());
    let session = match api.session("", true).await {
        Ok(s) if s.role == "broadcaster" && s.whip_url.is_some() => s,
        Ok(s) => {
            step(reserve, Some(false), trf!("the server answered as {}", "o servidor respondeu como {}", s.role));
            return fail(tr!(
                "Your test stream name is in use, probably by your own camera. Turn it off and try again.",
                "O nome da sua transmissão de teste está em uso, provavelmente pela sua câmera. Desligue-a e tente de novo."
            ));
        }
        Err(e) => {
            step(reserve, Some(false), e.message);
            return fail(tr!("The server refused the test stream.", "O servidor recusou a transmissão de teste."));
        }
    };
    step(reserve, Some(true), session.username.clone());

    step(stun, None, String::new());
    let types = rtc::candidate_types(&session.ice_servers).await;
    let srflx = types.iter().any(|t| t == "srflx");
    step(
        stun,
        Some(srflx),
        if srflx {
            types.join(", ")
        } else {
            trf!(
                "only {} · UDP may be blocked",
                "só {} · o UDP pode estar bloqueado",
                if types.is_empty() { tr!("none", "nenhum").into() } else { types.join(", ") }
            )
        },
    );

    step(send, None, String::new());
    let source = NativeVideoSource::new(VideoResolution { width: 160, height: 90 }, false);
    let track = rtc::factory().create_video_track("test", source.clone());
    // A grey frame ten times a second gives the encoder something to send.
    let stop = Arc::new(AtomicBool::new(false));
    let (s2, st) = (source.clone(), stop.clone());
    std::thread::spawn(move || {
        let started = Instant::now();
        while !st.load(Ordering::Relaxed) {
            let mut vf = VideoFrame::new(VideoRotation::VideoRotation0, I420Buffer::new(160, 90));
            vf.timestamp_us = started.elapsed().as_micros() as i64;
            s2.capture_frame(&vf);
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let url = session.whip_url.clone().unwrap_or_default();
    let params = VideoParams { max_bitrate: 150_000, max_fps: 10., sharp: false };
    let link = rtc::publish(&api, &url, &session.ice_servers, None, Some((track, params))).await;
    let result = match link {
        Err(e) => {
            step(send, Some(false), e.message);
            fail(tr!("The media server would not take the stream.", "O servidor de mídia não aceitou a transmissão."))
        }
        Ok(link) => {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut connected = false;
            while Instant::now() < deadline {
                match link.state() {
                    PeerConnectionState::Connected => {
                        connected = true;
                        break;
                    }
                    PeerConnectionState::Failed => break,
                    _ => tokio::time::sleep(Duration::from_millis(200)).await,
                }
            }
            let outcome = if !connected {
                step(send, Some(false), tr!("no working network path was found", "nenhum caminho de rede funcionou").into());
                fail(if srflx {
                    tr!(
                        "Signaling works but media can't get through: your network blocks the media ports. A TURN relay would be needed.",
                        "A sinalização funciona, mas a mídia não passa: sua rede bloqueia as portas de mídia. Seria preciso um relay TURN."
                    )
                } else {
                    tr!(
                        "UDP looks blocked on your network. The server also offers a TCP path; if this still fails, a TURN relay would be needed.",
                        "O UDP parece bloqueado na sua rede. O servidor também oferece um caminho por TCP; se ainda falhar, seria preciso um relay TURN."
                    )
                })
            } else {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let (via, tcp, rtt) = describe(&link.stats().await);
                step(
                    send,
                    Some(true),
                    match rtt {
                        Some(ms) => format!("{via}, {ms} ms"),
                        None => via,
                    },
                );
                Outcome {
                    ok: true,
                    verdict: if tcp {
                        tr!(
                            "Working, over the TCP fallback. Expect more delay than usual, but it will stream.",
                            "Funciona, pela alternativa via TCP. Pode ter mais atraso que o normal, mas a transmissão passa."
                        )
                        .into()
                    } else {
                        tr!(
                            "Everything works. Voice, cameras and screens will get through.",
                            "Tudo funciona. Voz, câmeras e telas vão passar."
                        )
                        .into()
                    },
                }
            };
            link.close();
            outcome
        }
    };
    stop.store(true, Ordering::Relaxed);
    if let Some(token) = &session.token {
        let _ = api.release_session(&session.username, token).await;
    }
    result
}

/// The winning candidate pair as "type/protocol → type/protocol", whether it is TCP, and the RTT.
fn describe(stats: &[RtcStats]) -> (String, bool, Option<u32>) {
    let pair = stats.iter().find_map(|s| match s {
        RtcStats::CandidatePair(p) if p.candidate_pair.nominated && p.candidate_pair.current_round_trip_time > 0. => Some(p),
        _ => None,
    });
    let Some(pair) = pair else { return (tr!("connected", "conectado").into(), false, None) };
    let find = |id: &str| {
        stats.iter().find_map(|s| match s {
            RtcStats::LocalCandidate(c) if c.rtc.id == id => Some(c.local_candidate.clone()),
            RtcStats::RemoteCandidate(c) if c.rtc.id == id => Some(c.remote_candidate.clone()),
            _ => None,
        })
    };
    let (local, remote) = (find(&pair.candidate_pair.local_candidate_id), find(&pair.candidate_pair.remote_candidate_id));
    let name = |c: &Option<libwebrtc::stats::dictionaries::IceCandidateStats>| {
        c.as_ref()
            .map(|c| format!("{}/{}", c.candidate_type.map(|t| format!("{t:?}").to_lowercase()).unwrap_or_default(), c.protocol))
            .unwrap_or_default()
    };
    let tcp = [&local, &remote].iter().any(|c| c.as_ref().is_some_and(|c| c.protocol == "tcp"));
    let rtt = (pair.candidate_pair.current_round_trip_time * 1000.).round() as u32;
    (format!("{} → {}", name(&local), name(&remote)), tcp, Some(rtt))
}
