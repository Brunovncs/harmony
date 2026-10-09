//! Being in a voice channel: joining and leaving through the socket, keeping the media tokens
//! fresh, mute and deafen, and the audio and video connections (see `voice`).

use super::audio::Cue;
use super::voice::{Voice, roster_cues, voice_cue};
use crate::core::types::*;
use crate::session::{Session, SessionEvent};
use gpui::{App, AppContext, Context, Entity, EventEmitter, Global, Subscription, Task, WeakEntity};
use serde_json::json;
use std::time::{Duration, Instant};

/// After a reconnect everyone's state comes back in a rush (rejoins, streams announced again)
/// that is not news; for this long, rosters only move the cues' baseline.
const RESYNC_QUIET: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq)]
pub enum CallState {
    Joining,
    Connected,
    /// The socket dropped; the call comes back with it.
    Reconnecting,
}

pub enum CallEvent {
    /// The server wants the channel's password (or a different one).
    NeedsPassword {
        channel: ChannelId,
        wrong: bool,
    },
    Ended(Option<String>),
}

pub struct Call {
    pub session: Entity<Session>,
    pub channel: ChannelId,
    pub state: CallState,
    pub tokens: Option<VoiceTokens>,
    pub mid: Option<i64>,
    pub muted: bool,
    pub deafened: bool,
    pub voice: Option<Entity<Voice>>,
    password: Option<String>,
    /// The roster the cues compare against; none until one has been seen, so arriving in a room
    /// of six plays nothing.
    cue_roster: Option<Vec<Member>>,
    quiet_until: Option<Instant>,
    left: bool,
    _refresh: Task<()>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<CallEvent> for Call {}

impl Call {
    pub fn join(
        session: Entity<Session>,
        channel: ChannelId,
        password: Option<String>,
        muted: bool,
        deafened: bool,
        cx: &mut gpui::App,
    ) -> Entity<Call> {
        let call = cx.new(|cx| {
            let sub = cx.subscribe(&session, |this: &mut Call, _, ev: &SessionEvent, cx| match ev {
                SessionEvent::Roster(id) if *id == this.channel => this.on_roster(cx),
                SessionEvent::Reconnected => {
                    this.state = CallState::Reconnecting;
                    this.cue_roster = None;
                    this.quiet_until = Some(Instant::now() + RESYNC_QUIET);
                    this.send_join(cx);
                }
                _ => {}
            });
            let mut call = Call {
                session,
                channel,
                state: CallState::Joining,
                tokens: None,
                mid: None,
                muted,
                deafened,
                voice: None,
                password,
                cue_roster: None,
                quiet_until: None,
                left: false,
                _refresh: Task::ready(()),
                _subs: vec![sub],
            };
            call.send_join(cx);
            call
        });
        if !cx.has_global::<ActiveCall>() {
            cx.on_app_quit(|cx| {
                leave_before_quitting(cx);
                async {}
            })
            .detach();
        }
        cx.set_global(ActiveCall(call.downgrade()));
        call
    }

    fn send_join(&mut self, cx: &mut Context<Self>) {
        let rt = self.session.read(cx).realtime.clone();
        let mut payload = json!({ "channelId": self.channel });
        if let Some(p) = &self.password {
            payload["password"] = json!(p);
        }
        let reply = Session::request(rt, "voice:join", payload);
        cx.spawn(async move |this, cx| {
            let reply = reply.await;
            let _ = this.update(cx, |this, cx| match reply {
                Ok(v) => {
                    let tokens: Option<VoiceTokens> = serde_json::from_value(v.clone()).ok();
                    this.mid = v.get("mid").and_then(|m| m.as_i64());
                    if let Some(roster) = v.get("roster").and_then(|r| serde_json::from_value::<Vec<Member>>(r.clone()).ok()) {
                        let channel = this.channel;
                        this.session.update(cx, |s, cx| {
                            s.rosters.insert(channel, roster);
                            s.unlocked.insert(channel);
                            cx.notify();
                        });
                    }
                    let first = this.tokens.is_none();
                    if first {
                        voice_cue(Cue::Connect, cx);
                    }
                    this.tokens = tokens;
                    this.state = CallState::Connected;
                    this.schedule_refresh(cx);
                    this.start_media(first, cx);
                    if this.muted || this.deafened {
                        this.send_mute(cx);
                    }
                    cx.notify();
                }
                Err(e) => match e.code {
                    ErrorCode::PasswordRequired | ErrorCode::BadPassword => {
                        cx.emit(CallEvent::NeedsPassword { channel: this.channel, wrong: e.code == ErrorCode::BadPassword });
                        cx.emit(CallEvent::Ended(None));
                    }
                    ErrorCode::ChannelFull => {
                        let full = match e.reply.get("cap").and_then(|c| c.as_i64()) {
                            Some(cap) => trf!("That channel is full ({} people).", "Esse canal está cheio ({} pessoas).", cap),
                            None => tr!("That channel is full.", "Esse canal está cheio.").into(),
                        };
                        cx.emit(CallEvent::Ended(Some(full)))
                    }
                    ErrorCode::NoSuchChannel => {
                        cx.emit(CallEvent::Ended(Some(tr!("That channel is gone.", "Esse canal não existe mais.").into())))
                    }
                    ErrorCode::Offline | ErrorCode::Timeout => {
                        // The socket is down; `Reconnected` will try again.
                        this.state = CallState::Reconnecting;
                        cx.notify();
                    }
                    _ => {
                        let why = crate::core::api::friendly(e.code, &e.reply)
                            .unwrap_or_else(|| trf!("Could not join voice ({}).", "Não foi possível entrar na voz ({}).", e));
                        cx.emit(CallEvent::Ended(Some(why)))
                    }
                },
            });
        })
        .detach();
    }

    /// Asks for fresh tokens at half their life, as the old client did.
    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        let every =
            self.tokens.as_ref().map(|t| Duration::from_millis((t.expires_in_ms / 2).max(5000))).unwrap_or(Duration::from_secs(300));
        self._refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(every).await;
                let Ok(rt) = this.update(cx, |this, cx| (this.session.read(cx).realtime.clone(), this.channel)) else { return };
                let reply = Session::request(rt.0, "voice:refresh", json!({ "channelId": rt.1 })).await;
                if let Ok(v) = reply
                    && let Ok(tokens) = serde_json::from_value::<VoiceTokens>(v)
                {
                    let _ = this.update(cx, |this, cx| {
                        this.tokens = Some(tokens.clone());
                        if let Some(voice) = &this.voice {
                            voice.update(cx, |v, cx| v.set_tokens(tokens, cx));
                        }
                    });
                }
            }
        });
    }

    fn start_media(&mut self, first: bool, cx: &mut Context<Self>) {
        let Some(tokens) = self.tokens.clone() else { return };
        match &self.voice {
            Some(v) if !first => v.update(cx, |v, cx| v.restart(tokens, self.mid, cx)),
            _ => {
                let session = self.session.clone();
                let (muted, deafened, mid, channel) = (self.muted, self.deafened, self.mid, self.channel);
                let voice = cx.new(|cx| Voice::new(session, channel, tokens, mid, muted, deafened, cx));
                cx.observe(&voice, |_, _, cx| cx.notify()).detach();
                self.voice = Some(voice);
            }
        }
        self.on_roster(cx);
    }

    fn on_roster(&mut self, cx: &mut Context<Self>) {
        let roster = self.session.read(cx).rosters.get(&self.channel).cloned().unwrap_or_default();
        let me = self.session.read(cx).me.id;
        if let Some(mine) = roster.iter().find(|m| m.user_id == me) {
            self.mid = Some(mine.mid);
            if mine.silenced() && !self.muted {
                self.muted = true;
                voice_cue(Cue::Mute, cx);
                crate::ui::overlay::toast(
                    if mine.force_muted {
                        tr!("An admin muted your microphone.", "Um admin silenciou seu microfone.")
                    } else {
                        tr!(
                            "Microphones are locked in this channel: only the owner speaks.",
                            "Os microfones estão bloqueados neste canal: só o dono fala."
                        )
                    },
                    cx,
                );
                if let Some(v) = &self.voice {
                    v.update(cx, |v, cx| v.set_muted(true, cx));
                }
            }
        }
        let quiet = self.quiet_until.is_some_and(|t| Instant::now() < t);
        if let Some(before) = &self.cue_roster
            && !quiet
        {
            for cue in roster_cues(before, &roster, me) {
                voice_cue(cue, cx);
            }
        }
        if let Some(voice) = &self.voice {
            voice.update(cx, |v, cx| v.sync(&roster, cx));
        }
        self.cue_roster = Some(roster);
        cx.notify();
    }

    pub fn set_muted(&mut self, muted: bool, cx: &mut Context<Self>) {
        let me = self.session.read(cx).me.id;
        let mine = self.session.read(cx).rosters.get(&self.channel).and_then(|r| r.iter().find(|m| m.user_id == me)).cloned();
        if !muted && let Some(mine) = mine.filter(|m| m.silenced()) {
            crate::ui::overlay::toast(
                if mine.force_muted {
                    tr!(
                        "An admin muted you. Only an admin can unmute you.",
                        "Um admin silenciou você. Só um admin pode reativar seu microfone."
                    )
                } else {
                    tr!(
                        "Microphones are locked in this channel. Only the owner can speak.",
                        "Os microfones estão bloqueados neste canal. Só o dono pode falar."
                    )
                },
                cx,
            );
            return;
        }
        // From what actually changes: unmuting while deafened undeafens too, and says so.
        if !muted && self.deafened {
            self.deafened = false;
            voice_cue(Cue::Undeafen, cx);
        } else if muted != self.muted {
            voice_cue(if muted { Cue::Mute } else { Cue::Unmute }, cx);
        }
        self.muted = muted;
        self.apply_mute(cx);
    }

    pub fn set_deafened(&mut self, deafened: bool, cx: &mut Context<Self>) {
        if deafened != self.deafened {
            voice_cue(if deafened { Cue::Deafen } else { Cue::Undeafen }, cx);
        }
        self.deafened = deafened;
        // Deafening mutes too; undeafening leaves the microphone as it was before.
        if deafened {
            self.muted = true;
        }
        self.apply_mute(cx);
    }

    fn apply_mute(&mut self, cx: &mut Context<Self>) {
        if let Some(v) = &self.voice {
            let (m, d) = (self.muted, self.deafened);
            v.update(cx, |v, cx| {
                v.set_muted(m, cx);
                v.set_deafened(d, cx);
            });
        }
        self.send_mute(cx);
        cx.notify();
    }

    fn send_mute(&self, cx: &mut Context<Self>) {
        let rt = self.session.read(cx).realtime.clone();
        let reply =
            Session::request(rt, "voice:mute", json!({ "channelId": self.channel, "muted": self.muted, "deafened": self.deafened }));
        cx.background_executor()
            .spawn(async move {
                let _ = reply.await;
            })
            .detach();
    }

    pub fn leave(&mut self, cx: &mut Context<Self>) {
        drop(self.hang_up(cx));
    }

    /// Hangs up every connection and leaves the channel; the handle finishes once the server
    /// has the leave.
    fn hang_up(&mut self, cx: &mut Context<Self>) -> tokio::task::JoinHandle<()> {
        self.left = true;
        if let Some(v) = self.voice.take() {
            v.update(cx, |v, cx| v.shutdown(cx));
        }
        self._refresh = Task::ready(());
        let rt = self.session.read(cx).realtime.clone();
        // Queued now, so a join that follows cannot overtake it.
        let reply = Session::request(rt, "voice:leave", json!({ "channelId": self.channel }));
        crate::core::runtime().spawn(async move {
            if let Err(e) = reply.await {
                log::info!("voice:leave: {e}");
            }
        })
    }

    pub fn is_speaking(&self, user: UserId, cx: &gpui::App) -> bool {
        self.voice.as_ref().is_some_and(|v| v.read(cx).is_speaking(user))
    }
}

/// The call in progress, for the quit hook.
struct ActiveCall(WeakEntity<Call>);

impl Global for ActiveCall {}

/// Quitting mid-call: leave and hang up properly, or the media server keeps the slot's paths for
/// a while and a quick relaunch is refused its microphone. Closing the window drops the call
/// before this runs, and its connections hang up as they drop; an explicit quit still has it, so
/// it is left here. GPUI only waits 200 ms for quit handlers, so this blocks instead, for about a
/// second at most.
fn leave_before_quitting(cx: &mut App) {
    let call = cx.try_global::<ActiveCall>().and_then(|a| a.0.upgrade()).filter(|c| !c.read(cx).left);
    let leave = call.map(|call| call.update(cx, |c, cx| c.hang_up(cx)));
    log::info!("quitting: {}hanging up", if leave.is_some() { "leaving the call and " } else { "" });
    let (done, wait) = std::sync::mpsc::channel();
    crate::core::runtime().spawn(async move {
        let limit = Duration::from_secs(1);
        let _ = tokio::time::timeout(limit, async {
            if let Some(leave) = leave {
                let _ = leave.await;
            }
            super::rtc::flush_hang_ups(limit).await;
        })
        .await;
        let _ = done.send(());
    });
    let _ = wait.recv_timeout(Duration::from_millis(1200));
    log::info!("quitting: done");
}
