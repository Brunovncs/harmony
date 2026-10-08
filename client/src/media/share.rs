//! Sending your camera and your screen in a call: each to its own path (`c`, `s`) over WHIP,
//! and shown to you as a local tile. The voice reconciler announces them once they are up and
//! publishes them again when the connection drops or the slot changes.

use super::audio::Cue;
use super::camera::{self, Capture};
use super::rtc::{self, Link, VideoParams};
use super::screen::{ScreenCapture, ScreenChoice};
use super::video::{Tile, TileKind};
use super::voice::{self, Backoff, Voice};
use crate::core;
use crate::core::settings::Background;
use crate::core::types::VoiceTokens;
use crate::session::Session;
use gpui::{Context, Entity};
use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

/// A published camera or screen's connection.
#[derive(Default)]
pub struct Outgoing {
    pub link: Option<Arc<Link>>,
    /// The publish in flight, by generation.
    attempt: Option<u64>,
    backoff: Backoff,
    /// The slot the link publishes to.
    mid: Option<i64>,
    /// Until it has gone out once, a failure ends the share instead of retrying.
    went_live: bool,
}

impl Outgoing {
    fn hang_up(&mut self) {
        self.attempt = None;
        if let Some(l) = self.link.take() {
            l.close();
        }
    }
}

pub struct LocalShare<C> {
    pub capture: C,
    pub tile: Entity<Tile>,
    pub out: Outgoing,
}

#[derive(Default)]
pub struct Shares {
    pub camera: Option<LocalShare<Capture>>,
    pub screen: Option<LocalShare<ScreenCapture>>,
}

const KINDS: [TileKind; 2] = [TileKind::Camera, TileKind::Screen];

impl Voice {
    pub fn camera_on(&self) -> bool {
        self.shares.camera.is_some()
    }

    pub fn screen_on(&self) -> bool {
        self.shares.screen.is_some()
    }

    pub fn screen_choice(&self) -> Option<ScreenChoice> {
        self.shares.screen.as_ref().map(|s| s.capture.choice.lock().clone())
    }

    pub fn start_camera(&mut self, device: &str, background: Background, cx: &mut Context<Self>) {
        if self.camera_on() {
            return;
        }
        let capture = Capture::start(device, background);
        let me = self.session().read(cx).me.id;
        let tile = Tile::local(me, self.mid().unwrap_or(-1), TileKind::Camera, capture.preview.clone(), cx);
        self.tiles.insert(0, tile.clone());
        self.shares.camera = Some(LocalShare { capture, tile, out: Outgoing::default() });
        self.publish(TileKind::Camera, cx);
        cx.notify();
    }

    pub fn set_camera_background(&mut self, bg: Background) {
        if let Some(c) = &self.shares.camera {
            c.capture.set_background(bg);
        }
    }

    pub fn stop_camera(&mut self, cx: &mut Context<Self>) {
        if let Some(mut share) = self.shares.camera.take() {
            share.capture.stop();
            share.out.hang_up();
            self.unpublish(&share.tile, cx);
        }
    }

    pub fn start_screen(&mut self, choice: ScreenChoice, cx: &mut Context<Self>) {
        if self.shares.screen.is_some() {
            // Already sharing: switch what is captured, keep the connection.
            self.update_screen(choice, cx);
            return;
        }
        let capture = ScreenCapture::start(choice);
        let me = self.session().read(cx).me.id;
        let tile = Tile::local(me, self.mid().unwrap_or(-1), TileKind::Screen, capture.preview.clone(), cx);
        self.tiles.insert(0, tile.clone());
        self.shares.screen = Some(LocalShare { capture, tile, out: Outgoing::default() });
        self.publish(TileKind::Screen, cx);
        // Yours sounds the moment it happens, not when the roster echoes it back.
        voice::voice_cue(Cue::StreamStart, cx);
        cx.notify();
    }

    /// Changes resolution or frame rate while sharing.
    pub fn update_screen(&mut self, choice: ScreenChoice, cx: &mut Context<Self>) {
        if let Some(share) = &self.shares.screen {
            let params = choice.params();
            share.capture.switch(choice.clone());
            if let Some(l) = &share.out.link {
                l.set_video_params(params);
            }
        }
        cx.notify();
    }

    pub fn stop_screen(&mut self, cx: &mut Context<Self>) {
        if self.end_screen(cx) {
            voice::voice_cue(Cue::StreamStop, cx);
        }
    }

    /// Stops sharing without a sound, for leaving the call; true if there was a share.
    pub(super) fn end_screen(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(mut share) = self.shares.screen.take() else { return false };
        share.capture.stop();
        share.out.hang_up();
        self.unpublish(&share.tile, cx);
        true
    }

    fn outgoing(&mut self, kind: TileKind) -> Option<&mut Outgoing> {
        match kind {
            TileKind::Camera => self.shares.camera.as_mut().map(|s| &mut s.out),
            TileKind::Screen => self.shares.screen.as_mut().map(|s| &mut s.out),
        }
    }

    /// Whether that share is up on the slot you have now.
    pub(super) fn share_live(&self, kind: TileKind) -> bool {
        let out = match kind {
            TileKind::Camera => self.shares.camera.as_ref().map(|s| &s.out),
            TileKind::Screen => self.shares.screen.as_ref().map(|s| &s.out),
        };
        out.is_some_and(|o| o.link.is_some() && o.mid == self.mid())
    }

    /// Publishes again whatever share has no connection: it dropped, or the slot changed.
    pub(super) fn reconcile_shares(&mut self, now: Instant, cx: &mut Context<Self>) {
        for kind in KINDS {
            let Some(out) = self.outgoing(kind) else { continue };
            if out.link.as_ref().is_some_and(|l| l.is_dead()) {
                log::info!("the {} connection dropped; publishing again", kind.path());
                out.hang_up();
            }
            if out.link.is_none() && out.attempt.is_none() && out.backoff.ready(now) {
                self.publish(kind, cx);
            }
        }
    }

    /// After the slot changed: the old paths are hung up (an orphan there would refuse whoever
    /// gets that slot next) and the reconciler publishes to the new ones.
    pub(super) fn repath_shares(&mut self, cx: &mut Context<Self>) {
        let mid = self.mid().unwrap_or(-1);
        for kind in KINDS {
            if let Some(out) = self.outgoing(kind) {
                out.hang_up();
                out.backoff.reset();
            }
        }
        let tiles: Vec<Entity<Tile>> =
            [self.shares.camera.as_ref().map(|s| s.tile.clone()), self.shares.screen.as_ref().map(|s| s.tile.clone())]
                .into_iter()
                .flatten()
                .collect();
        for tile in tiles {
            tile.update(cx, |t, _| t.mid = mid);
        }
    }

    fn publish(&mut self, kind: TileKind, cx: &mut Context<Self>) {
        let tracks = match kind {
            TileKind::Camera => self.shares.camera.as_ref().map(|s| {
                let params = VideoParams { max_bitrate: camera::BITRATE, max_fps: camera::FPS as f64, sharp: false };
                (rtc::factory().create_video_track("camera", s.capture.source.clone()), None, params)
            }),
            TileKind::Screen => self.shares.screen.as_ref().map(|s| {
                let params = s.capture.choice.lock().params();
                (rtc::factory().create_video_track("screen", s.capture.source.clone()), s.capture.audio_track(), params)
            }),
        };
        let Some((track, audio, params)) = tracks else { return };
        let generation = voice::next_generation();
        let mid = self.mid();
        if let Some(out) = self.outgoing(kind) {
            out.attempt = Some(generation);
        }
        let session: Entity<Session> = self.session().clone();
        let (api, ice, rt) = {
            let s = session.read(cx);
            (s.api.clone(), s.ice_servers.clone(), s.realtime.clone())
        };
        let channel = self.channel;
        cx.spawn(async move |this, cx| {
            // Fresh tokens first: the ones from joining may be close to expiring.
            let tokens = match Session::request(rt, "voice:refresh", json!({ "channelId": channel })).await {
                Ok(v) => serde_json::from_value::<VoiceTokens>(v).ok(),
                Err(_) => None,
            };
            let tokens = match tokens {
                Some(t) => {
                    let _ = this.update(cx, |v, cx| v.set_tokens(t.clone(), cx));
                    t
                }
                None => match this.update(cx, |v, _| v.tokens().clone()) {
                    Ok(t) => t,
                    Err(_) => return,
                },
            };
            let url = match kind {
                TileKind::Camera => tokens.publish.cam.clone(),
                TileKind::Screen => tokens.publish.screen.clone(),
            };
            let link = core::run(async move {
                let link = rtc::publish(&api, &url, &ice, audio.map(|a| (a, 128_000)), Some((track, params))).await?;
                if link.connected(voice::CONNECT_LIMIT).await { Ok(link) } else { Err(voice::not_connected()) }
            })
            .await;
            let _ = this.update(cx, |v, cx| {
                // Stopped, or superseded by a newer attempt: the link hangs up as it drops.
                let Some(out) = v.outgoing(kind).filter(|o| o.attempt == Some(generation)) else { return };
                out.attempt = None;
                match link {
                    Ok(link) => {
                        log::info!("{} published to slot {mid:?}", kind.path());
                        out.link = Some(Arc::new(link));
                        out.mid = mid;
                        out.went_live = true;
                        out.backoff.reset();
                        v.announce(cx);
                        cx.notify();
                    }
                    Err(e) if out.went_live && out.backoff.failures < 4 => {
                        log::info!("{} publish failed, trying again: {} ({})", kind.path(), e.message, e.code);
                        out.backoff.fail(Instant::now());
                    }
                    Err(e) => {
                        log::warn!("{} publish failed: {} ({}, {})", kind.path(), e.message, e.code, e.status);
                        crate::ui::overlay::toast(trf!("Could not share: {}", "Não foi possível compartilhar: {}", e.message), cx);
                        match kind {
                            TileKind::Camera => v.stop_camera(cx),
                            TileKind::Screen => v.stop_screen(cx),
                        }
                    }
                }
            });
        })
        .detach();
    }

    /// Takes the share's tile down; the reconciler tells the server the path is off.
    fn unpublish(&mut self, tile: &Entity<Tile>, cx: &mut Context<Self>) {
        self.tiles.retain(|t| t != tile);
        tile.update(cx, |t, cx| t.close(cx));
        self.announce(cx);
        cx.notify();
    }
}
