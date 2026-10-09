//! A server: channels on the left, the open text channel or the voice stage in the middle,
//! members on the right, each in its own pane, as Texel's studio lays out its three panes.

mod account;
mod admin;
pub mod chat;
pub mod emoji_picker;
mod members;
mod picker;
pub mod private;
pub mod rich;
mod share;
pub mod sidebar;
mod stage;

use super::Root;
use crate::core::settings::HotkeyAction;
use crate::core::types::*;
use crate::media::audio::{Cue, MAX_GAIN};
use crate::media::call::{Call, CallEvent, CallState};
use crate::media::voice::{cue_volume, voice_cue, volume_of};
use crate::prefs::{prefs, set_prefs};
use crate::session::{Link, Session, SessionEvent};
use crate::theme::{GUTTER, Theme, current, px, radius};
use crate::ui::overlay::{self, Ask, Field};
use crate::widgets::*;
use chat::{ChatEvent, ChatView};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnimationExt, AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render, SpringAnimation, SpringConfig, StatefulInteractiveElement,
    StyleRefinement, Styled, Subscription, WeakEntity, Window, div,
};
use sidebar::{Menu, MenuEntry};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Center {
    /// A text channel or a private conversation.
    Chat(Room),
    /// The voice channel you are in: people, cameras, screens.
    Stage,
}

pub struct ServerView {
    pub session: Entity<Session>,
    root: WeakEntity<Root>,
    pub center: Center,
    pub folded: HashSet<i64>,
    chats: HashMap<Room, Entity<ChatView>>,
    pub call: Option<Entity<Call>>,
    /// The "is calling you" dialog, while it rings.
    ringing: Option<Entity<private::Incoming>>,
    pub focused_tile: Option<Entity<crate::media::video::Tile>>,
    /// The pointer is on the connection dot, which opens to say what it means.
    link_hover: bool,
    /// When the last message tone played, so a burst of messages makes one sound.
    message_cue_at: Option<std::time::Instant>,
    /// The channel list draws again whenever this view is notified (the session, the call and
    /// who speaks all notify it); the member list only when the session changes.
    sidebar: Entity<Panel>,
    members: Entity<Panel>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl Focusable for ServerView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// A part of the server view drawn as a view of its own, so the window can keep it as it was
/// while a video frame or a keystroke redraws something else. It draws again only when notified.
struct Panel {
    server: WeakEntity<ServerView>,
    draw: fn(&mut ServerView, &Theme, &mut Window, &mut Context<ServerView>) -> AnyElement,
}

impl Panel {
    fn new<T: 'static>(
        server: &Entity<ServerView>,
        redraw_with: &Entity<T>,
        draw: fn(&mut ServerView, &Theme, &mut Window, &mut Context<ServerView>) -> AnyElement,
        cx: &mut App,
    ) -> Entity<Panel> {
        cx.new(|cx| {
            cx.observe(redraw_with, |_, _, cx| cx.notify()).detach();
            Panel { server: server.downgrade(), draw }
        })
    }
}

impl Render for Panel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let draw = self.draw;
        self.server.update(cx, |s, cx| draw(s, &t, window, cx)).unwrap_or_else(|_| div().into_any_element())
    }
}

/// Sends a request on the socket now and toasts it if the server turns it down, as
/// `Session::call` does for HTTP.
pub(super) fn request(session: &Entity<Session>, kind: &'static str, payload: serde_json::Value, cx: &mut App) {
    let reply = Session::request(session.read(cx).realtime.clone(), kind, payload);
    cx.spawn(async move |cx| {
        if let Err(e) = reply.await {
            log::info!("{kind} failed: {e}");
            cx.update(|cx| overlay::toast(refusal(&e), cx));
        }
    })
    .detach();
}

fn refusal(e: &crate::core::realtime::RequestError) -> gpui::SharedString {
    crate::core::api::friendly(e.code, &e.reply)
        .unwrap_or_else(|| trf!("The server refused that ({}).", "O servidor recusou isso ({}).", e))
        .into()
}

impl ServerView {
    pub fn new(session: Entity<Session>, root: WeakEntity<Root>, window: &mut Window, cx: &mut Context<Self>) -> ServerView {
        let subs = vec![
            cx.observe(&session, |this, _, cx| this.on_session(cx)),
            cx.subscribe_in(&session, window, |this, _, ev: &SessionEvent, window, cx| this.on_event(ev, window, cx)),
            // The shortcuts are heard here, so focus comes back when a dialog closes or a click
            // lands on nothing focusable.
            cx.on_focus_lost(window, |this, window, cx| this.focus.focus(window, cx)),
        ];
        crate::media::voice::apply_mic_prefs(cx);
        crate::media::audio::audio().set_output(&prefs(cx).voice_output_id);
        let this = cx.entity();
        // The ring: someone calling you, or the other end ringing while you wait.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(2600)).await;
                let Ok(()) = this.update(cx, |v, cx| v.ring_tick(cx)) else { break };
            }
        })
        .detach();
        let sidebar = Panel::new(&this, &this, |s, t, w, cx| s.render_sidebar(t, w, cx).into_any_element(), cx);
        let members = Panel::new(&this, &session, |s, t, w, cx| s.render_members(t, w, cx).into_any_element(), cx);
        ServerView {
            session,
            root,
            center: Center::Stage,
            folded: HashSet::new(),
            chats: HashMap::new(),
            call: None,
            focused_tile: None,
            link_hover: false,
            message_cue_at: None,
            ringing: None,
            sidebar,
            members,
            focus: cx.focus_handle(),
            _subs: subs,
        }
    }

    fn on_session(&mut self, cx: &mut Context<Self>) {
        let s = self.session.read(cx);
        let server = (s.api.base(), s.clips.iter().map(|c| c.id).collect());
        crate::ui::hotkeys::set_server(Some(server), cx);
        // Off the stage with no call: back to the chat that was open (the conversation a call was
        // made from, say), or the first text channel once the list arrives.
        if self.center == Center::Stage && self.call.is_none() {
            let s = self.session.read(cx);
            let last = s.open_room.filter(|r| match r {
                Room::Channel(id) => s.channel(*id).is_some(),
                Room::Dm(id) => s.dms.contains_key(id),
            });
            let first = || s.layout().into_iter().flat_map(|(_, cs)| cs).find(|c| c.kind == ChannelKind::Text).map(|c| Room::Channel(c.id));
            if let Some(room) = last.or_else(first) {
                self.center = Center::Chat(room);
                self.session.update(cx, |s, cx| s.open(room, cx));
            }
        }
        let gone = match self.center {
            Center::Chat(Room::Channel(id)) => self.session.read(cx).channel(id).is_none(),
            Center::Chat(Room::Dm(id)) => self.session.read(cx).dms_loaded && !self.session.read(cx).dms.contains_key(&id),
            Center::Stage => false,
        };
        if gone {
            self.show_stage(cx);
        }
        cx.notify();
    }

    fn on_event(&mut self, ev: &SessionEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            SessionEvent::Mentioned => {
                if prefs(cx).mention_sound {
                    crate::media::audio::cue(Cue::Mention, cue_volume(cx));
                }
            }
            SessionEvent::Message(room, m) => self.message_cue(*room, m, window, cx),
            SessionEvent::Ringing(id) => self.show_ringing(*id, window, cx),
            SessionEvent::CallChanged(id) => {
                let still = self.session.read(cx).calls.get(id).is_some_and(|c| c.state == CallPhase::Ringing);
                if !still && let Some(r) = self.ringing.take_if(|r| r.read(cx).conversation == *id) {
                    r.update(cx, |r, cx| r.close(cx));
                }
                cx.notify();
            }
            SessionEvent::SoundpadPlay { hash } => {
                let deaf = self.call.as_ref().is_some_and(|c| c.read(cx).deafened);
                if !deaf {
                    stage::play_clip(&self.session, hash.clone(), prefs(cx).soundpad_volume as f32 / 100., cx);
                }
            }
            SessionEvent::Moved(to) => {
                // Moved somewhere: that channel's connect is the sound of it.
                self.hang_up(to.is_none(), cx);
                if let Some(ch) = to {
                    self.join_voice(*ch, None, window, cx);
                }
            }
            _ => {}
        }
    }

    /// The tone for someone else's message, unless you are looking at it, silenced its channel or
    /// turned the tone off. A mention has its own; a burst makes one sound.
    fn message_cue(&mut self, room: Room, m: &Message, window: &Window, cx: &mut Context<Self>) {
        const BURST: std::time::Duration = std::time::Duration::from_secs(2);
        let s = self.session.read(cx);
        let p = prefs(cx);
        let mention = m.mentions.contains(&s.me.id) || m.mentions_everyone;
        let looking = s.viewing == Some(room) && window.is_window_active();
        let private = room.conversation().is_some();
        // A private message is written to you, as a mention is: it rings like one.
        let (cue, wanted) = if private { (Cue::Mention, p.mention_sound) } else { (Cue::Message, p.message_sound) };
        if private && m.user_id != Some(s.me.id) && !looking {
            let name = s.display_name(m.user_id, None);
            crate::ui::tray::attention(window, &name, tr!("sent you a private message", "te mandou uma mensagem privada"), cx);
        }
        if m.user_id == Some(s.me.id)
            || (mention && !private)
            || looking
            || !wanted
            || room.channel().is_some_and(|id| p.channel_muted(&s.api.base(), id))
            || self.message_cue_at.is_some_and(|at| at.elapsed() < BURST)
        {
            return;
        }
        self.message_cue_at = Some(std::time::Instant::now());
        crate::media::audio::cue(cue, cue_volume(cx));
    }

    /// Every couple of seconds: the ring of a call coming in, or the one going out.
    fn ring_tick(&mut self, cx: &mut Context<Self>) {
        if self.ringing.is_some() {
            crate::media::audio::cue(Cue::Ring, cue_volume(cx).max(0.6));
        } else if self.call.as_ref().is_some_and(|c| c.read(cx).ringing(cx)) {
            crate::media::audio::cue(Cue::RingBack, cue_volume(cx));
        }
    }

    // Private conversations and calls.

    /// Opens the conversation with `user`, making it if needed.
    pub fn message_user(&mut self, user: UserId, window: &mut Window, cx: &mut Context<Self>) {
        let task = self.session.update(cx, |s, cx| s.open_dm_with(user, cx));
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let got = task.await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| match got {
                    Ok(id) => this.open_room(Room::Dm(id), window, cx),
                    Err(e) => overlay::toast(e.message, cx),
                });
            });
        })
        .detach();
    }

    /// Opens the conversation with `user` and rings them.
    pub fn call_user(&mut self, user: UserId, window: &mut Window, cx: &mut Context<Self>) {
        let task = self.session.update(cx, |s, cx| s.open_dm_with(user, cx));
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let got = task.await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| match got {
                    Ok(id) => {
                        this.open_room(Room::Dm(id), window, cx);
                        this.start_call(id, window, cx);
                    }
                    Err(e) => overlay::toast(e.message, cx),
                });
            });
        })
        .detach();
    }

    /// Rings the other person of a private conversation.
    pub fn start_call(&mut self, conversation: ConversationId, window: &mut Window, cx: &mut Context<Self>) {
        if self.call.as_ref().is_some_and(|c| c.read(cx).place == Place::Call(conversation)) {
            self.show_stage(cx);
            return;
        }
        let (muted, deafened) = self.carried_mute(cx);
        self.hang_up(false, cx);
        match Call::dial(self.session.clone(), conversation, muted, deafened, cx) {
            Ok(call) => self.adopt_call(call, window, cx),
            Err(e) => overlay::toast(e.message, cx),
        }
    }

    fn show_ringing(&mut self, conversation: ConversationId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(caller) = self.session.read(cx).calls.get(&conversation).map(|c| c.caller_id) else { return };
        if let Some(old) = self.ringing.take() {
            old.update(cx, |r, cx| r.close(cx));
        }
        let this = cx.entity().downgrade();
        let (answer, decline) = (this.clone(), this);
        let view = cx.new(|cx| private::Incoming {
            session: self.session.clone(),
            conversation,
            caller,
            on_answer: std::rc::Rc::new(move |id, window, cx| {
                if let Some(this) = answer.upgrade() {
                    this.update(cx, |this, cx| this.answer_call(id, window, cx));
                }
            }),
            on_decline: std::rc::Rc::new(move |id, cx| {
                if let Some(this) = decline.upgrade() {
                    this.update(cx, |this, cx| {
                        this.ringing = None;
                        request(&this.session, "call:hang-up", serde_json::json!({ "conversationId": id }), cx);
                    });
                }
            }),
            focus: cx.focus_handle(),
        });
        // Closed without an answer (Escape, a click outside): stop ringing here; the caller
        // still hears it ring until they give up or it times out, as with a phone left alone.
        cx.subscribe(&view, |this, v, _: &overlay::Dismiss, _| {
            if this.ringing.as_ref() == Some(&v) {
                this.ringing = None;
            }
        })
        .detach();
        self.ringing = Some(view.clone());
        crate::media::audio::cue(Cue::Ring, cue_volume(cx).max(0.6));
        let name = self.session.read(cx).display_name(Some(caller), None);
        crate::ui::tray::attention(window, &name, tr!("is calling you", "está te ligando"), cx);
        overlay::open_dialog(view, window, cx);
    }

    fn answer_call(&mut self, conversation: ConversationId, window: &mut Window, cx: &mut Context<Self>) {
        self.ringing = None;
        let (muted, deafened) = self.carried_mute(cx);
        self.hang_up(false, cx);
        match Call::answer(self.session.clone(), conversation, muted, deafened, cx) {
            Some(call) => {
                self.adopt_call(call, window, cx);
                self.open_room(Room::Dm(conversation), window, cx);
                self.show_stage(cx);
            }
            None => overlay::toast(
                tr!("That call couldn't be opened on this computer.", "Essa chamada não pôde ser aberta neste computador."),
                cx,
            ),
        }
    }

    /// The mute and deafen a new call starts with: as the call before had them, and muted when
    /// that is the setting.
    fn carried_mute(&self, cx: &App) -> (bool, bool) {
        let (muted, deafened) = self.call.as_ref().map(|c| (c.read(cx).muted, c.read(cx).deafened)).unwrap_or((false, false));
        (muted || prefs(cx).join_muted, deafened)
    }

    /// Makes `call` the one in progress: its events, the tray, the stage.
    fn adopt_call(&mut self, call: Entity<Call>, window: &mut Window, cx: &mut Context<Self>) {
        let sub = cx.subscribe_in(&call, window, |this, _, ev: &CallEvent, window, cx| match ev {
            CallEvent::NeedsPassword { channel, wrong } => this.ask_password(*channel, *wrong, window, cx),
            CallEvent::Ended(why) => {
                this.leave_voice(cx);
                if let Some(why) = why {
                    overlay::toast(why.clone(), cx);
                }
            }
        });
        self._subs.push(sub);
        cx.observe(&call, |_, _, cx| cx.notify()).detach();
        self.call = Some(call);
        crate::ui::updates::call_changed(true, cx);
        self.show_stage(cx);
        cx.notify();
    }

    /// The voice stage in the middle, in place of any chat.
    pub fn show_stage(&mut self, cx: &mut Context<Self>) {
        self.center = Center::Stage;
        self.session.update(cx, |s, cx| s.leave_chat(cx));
        cx.notify();
    }

    pub fn shutdown(&mut self, cx: &mut Context<Self>) {
        self.leave_voice(cx);
        crate::ui::hotkeys::set_server(None, cx);
        self.session.update(cx, |s, _| s.stop());
        let _ = &self.root;
    }

    // Where you are.

    pub fn open_text(&mut self, id: ChannelId, window: &mut Window, cx: &mut Context<Self>) {
        self.open_room(Room::Channel(id), window, cx);
    }

    /// A text channel or a private conversation, in the middle.
    pub fn open_room(&mut self, room: Room, window: &mut Window, cx: &mut Context<Self>) {
        self.center = Center::Chat(room);
        let chat = self.chat_view(room, window, cx);
        self.session.update(cx, |s, cx| s.open(room, cx));
        chat.update(cx, |c, cx| c.focus_composer(window, cx));
        cx.notify();
    }

    fn chat_view(&mut self, room: Room, window: &mut Window, cx: &mut Context<Self>) -> Entity<ChatView> {
        if let Some(chat) = self.chats.get(&room) {
            return chat.clone();
        }
        let session = self.session.clone();
        let chat = cx.new(|cx| ChatView::new(session, room, window, cx));
        let sub = cx.subscribe_in(&chat, window, |this, _, ev: &ChatEvent, window, cx| match ev {
            ChatEvent::Call(id) => this.start_call(*id, window, cx),
        });
        self._subs.push(sub);
        self.chats.insert(room, chat.clone());
        chat
    }

    pub fn call_place(&self, cx: &App) -> Option<Place> {
        self.call.as_ref().map(|c| c.read(cx).place)
    }

    pub fn call_channel(&self, cx: &App) -> Option<ChannelId> {
        self.call_place(cx).and_then(Place::channel)
    }

    pub fn is_speaking(&self, place: Place, user: UserId, cx: &App) -> bool {
        self.call.as_ref().is_some_and(|c| c.read(cx).place == place && c.read(cx).is_speaking(user, cx))
    }

    /// A click on a voice channel: asks first, unless that was turned off or you are already in
    /// it. Being moved, or answering a password, joins without asking; that was not a click.
    pub fn ask_join_voice(&mut self, id: ChannelId, window: &mut Window, cx: &mut Context<Self>) {
        if self.call_channel(cx) == Some(id) || !prefs(cx).confirm_voice_join {
            return self.join_voice(id, None, window, cx);
        }
        let Some(channel) = self.session.read(cx).channel(id).cloned() else { return };
        let owner = self.session.read(cx).me.role == Role::Owner;
        let mut text = match self.call_channel(cx).and_then(|c| self.session.read(cx).channel(c)) {
            Some(now) => trf!("You will leave {} and join this one.", "Você vai sair de {} e entrar neste.", now.name),
            None => tr!("Everyone in it will be able to hear you.", "Todos que estão nele vão poder te ouvir.").into(),
        };
        if channel.mic_locked && !owner {
            text = trf!(
                "{} Microphones are locked here: only the owner speaks.",
                "{} Os microfones estão bloqueados aqui: só o dono fala.",
                text
            );
        }
        let this = cx.entity().downgrade();
        Ask::open(
            trf!("Join {}?", "Entrar em {}?", channel.name),
            Some(text),
            tr!("Join", "Entrar"),
            false,
            vec![Field::Check { label: tr!("Don't ask again", "Não perguntar de novo"), on: false }],
            window,
            cx,
            move |v, window, cx| {
                if !v[0].is_empty() {
                    set_prefs(cx, |p| p.confirm_voice_join = false);
                }
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| this.join_voice(id, None, window, cx));
                }
                None
            },
        );
    }

    pub fn join_voice(&mut self, id: ChannelId, password: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.call_channel(cx) == Some(id) {
            self.show_stage(cx);
            cx.notify();
            return;
        }
        let (muted, deafened) = self.carried_mute(cx);
        // Quietly: the new channel's connect is the sound of switching.
        self.hang_up(false, cx);
        let call = Call::join(self.session.clone(), id, password, muted, deafened, cx);
        self.adopt_call(call, window, cx);
    }

    pub fn leave_voice(&mut self, cx: &mut Context<Self>) {
        self.hang_up(true, cx);
    }

    /// Leaves the call, with the disconnect cue when `cue` and the call had got as far as
    /// connecting (a join that was refused never made a sound).
    fn hang_up(&mut self, cue: bool, cx: &mut Context<Self>) {
        if let Some(call) = self.call.take() {
            let connected = call.read(cx).tokens.is_some();
            call.update(cx, |c, cx| c.leave(cx));
            crate::ui::updates::call_changed(false, cx);
            if cue && connected {
                voice_cue(Cue::Disconnect, cx);
            }
        }
        if self.center == Center::Stage {
            self.on_session(cx);
        }
        cx.notify();
    }

    fn ask_password(&mut self, channel: ChannelId, wrong: bool, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.session.read(cx).channel(channel).map(|c| c.name.clone()).unwrap_or_default();
        let this = cx.entity().downgrade();
        Ask::open(
            trf!("{} is locked", "{} está trancado", name),
            Some(
                if wrong {
                    tr!("That password is not it. Try again.", "Essa senha não é a certa. Tente de novo.")
                } else {
                    tr!("Type the channel's password to join.", "Digite a senha do canal para entrar.")
                }
                .into(),
            ),
            tr!("Join", "Entrar"),
            false,
            vec![Field::Text {
                label: tr!("Password", "Senha"),
                value: String::new(),
                placeholder: "",
                secret: true,
                multiline: false,
                max: 64,
            }],
            window,
            cx,
            move |v, window, cx| {
                if let Some(this) = this.upgrade() {
                    let pw = v[0].clone();
                    this.update(cx, |this, cx| this.join_voice(channel, Some(pw), window, cx));
                }
                None
            },
        );
    }

    pub fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        if let Some(call) = &self.call {
            call.update(cx, |c, cx| {
                let m = !c.muted;
                c.set_muted(m, cx)
            });
        }
    }

    pub fn toggle_deafen(&mut self, cx: &mut Context<Self>) {
        if let Some(call) = &self.call {
            call.update(cx, |c, cx| {
                let d = !c.deafened;
                c.set_deafened(d, cx)
            });
        }
    }

    /// A global hotkey was pressed, most likely from inside a game. Mute and deafen do what
    /// Ctrl+Shift+M and D do; a clip plays as a click on it does. Outside a call, nothing.
    pub fn on_hotkey(&mut self, action: HotkeyAction, cx: &mut Context<Self>) {
        match action {
            HotkeyAction::Mute => self.toggle_mute(cx),
            HotkeyAction::Deafen => self.toggle_deafen(cx),
            HotkeyAction::Clip(id) => {
                if let Some(channel) = self.call_channel(cx)
                    && self.session.read(cx).clips.iter().any(|c| c.id == id)
                {
                    request(&self.session, "soundpad:play", serde_json::json!({ "channelId": channel, "clipId": id }), cx);
                }
            }
        }
    }

    /// Right-click on someone in voice: their volume for you, and the admin's tools.
    pub fn peer_menu(&mut self, place: Place, user: UserId, mid: i64, at: Point<Pixels>, _: &mut Window, cx: &mut Context<Self>) {
        let s = self.session.read(cx);
        // An admin's say ends at the channels: a private call is nobody's to moderate.
        let channel = place.channel().unwrap_or(-1);
        let admin = s.me.role.is_admin() && place.channel().is_some();
        let force_muted = s.rosters.get(&channel).and_then(|r| r.iter().find(|m| m.mid == mid)).is_some_and(|m| m.force_muted);
        let in_my_call = self.call_place(cx) == Some(place);
        let others: Vec<(ChannelId, String)> =
            s.channels.iter().filter(|c| c.kind == ChannelKind::Voice && c.id != channel).map(|c| (c.id, c.name.clone())).collect();
        let name = s.display_name(Some(user), None);
        let mut items = Vec::new();
        if in_my_call && let Some(call) = self.call.clone() {
            let voice = call.read(cx).voice.clone();
            items.push(MenuEntry::custom(move |t, _, _| {
                let gain = volume_of(user);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .px(px(10.))
                    .py(px(8.))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(label(trf!("Volume for {}", "Volume de {}", name), t))
                            .child(mono(format!("{:.0}%", gain * 100.), if gain > 1. { t.caution } else { t.text2 })),
                    )
                    .into_any_element()
            }));
            if let Some(voice) = voice {
                let v2 = voice.clone();
                items.push(MenuEntry::custom(move |t, _, cx| {
                    let gain = volume_of(user);
                    let voice = v2.clone();
                    div()
                        .px(px(10.))
                        .pb(px(8.))
                        .child(VolumeSlider::new(gain, move |g, cx| voice.update(cx, |v, cx| v.set_volume(user, g, cx))).render(t, cx))
                        .into_any_element()
                }));
                let muted_for_me = volume_of(user) == 0.;
                items.push(MenuEntry::item(
                    if muted_for_me { "volume" } else { "volume-off" },
                    if muted_for_me { tr!("Unmute for me", "Reativar o som para mim") } else { tr!("Mute for me", "Silenciar para mim") },
                    false,
                    move |_, cx| voice.update(cx, |v, cx| v.set_volume(user, if muted_for_me { 1. } else { 0. }, cx)),
                ));
            }
        }
        if admin {
            if !items.is_empty() {
                items.push(MenuEntry::rule());
            }
            let session = self.session.clone();
            items.push(MenuEntry::item(
                "mic-off",
                if force_muted { tr!("Let them speak", "Deixar falar") } else { tr!("Mute for everyone", "Silenciar para todos") },
                !force_muted,
                move |_, cx| {
                    let payload = serde_json::json!({ "channelId": channel, "mid": mid, "muted": !force_muted });
                    request(&session, "admin:force-mute", payload, cx);
                },
            ));
            for (to, to_name) in others {
                let session = self.session.clone();
                items.push(MenuEntry::item("arrow-right", trf!("Move to {}", "Mover para {}", to_name), false, move |_, cx| {
                    request(&session, "admin:move", serde_json::json!({ "userId": user, "toChannelId": to }), cx);
                }));
            }
            let session = self.session.clone();
            items.push(MenuEntry::item("phone-off", tr!("Disconnect", "Desconectar"), true, move |_, cx| {
                request(&session, "admin:move", serde_json::json!({ "userId": user, "toChannelId": null }), cx);
            }));
        }
        if items.is_empty() {
            return;
        }
        let menu = cx.new(|_| Menu { items });
        overlay::open_menu(menu, at, cx);
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if k.modifiers.control && !k.modifiers.shift {
            match k.key.as_str() {
                "," => self.open_settings(window, cx),
                "/" => {
                    let view = cx.new(|cx| Shortcuts { focus: cx.focus_handle() });
                    overlay::open_dialog(view, window, cx);
                }
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        // Ctrl+Shift+M and Ctrl+Shift+D, as most voice apps use.
        if k.modifiers.control && k.modifiers.shift {
            match k.key.as_str() {
                "m" => self.toggle_mute(cx),
                "d" => self.toggle_deafen(cx),
                _ => return,
            }
            cx.stop_propagation();
        }
    }

    // Drawing.

    fn render_topbar(&mut self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let picture = self.session.update(cx, |s, cx| s.server_picture(cx));
        let s = self.session.read(cx);
        let role = s.me.role;
        let up = s.link == Link::Up;
        // Before the first snapshot the link is coming up, not coming back.
        let first = s.channels.is_empty();
        let show_members = prefs(cx).show_members;
        let (state, color) = match (up, first) {
            (true, _) => (tr!("Connected", "Conectado"), t.success),
            (false, true) => (tr!("Connecting…", "Conectando…"), t.caution),
            (false, false) => (tr!("Reconnecting…", "Reconectando…"), t.caution),
        };
        // Connected, it is only the dot; it opens on hover, and stays open while something is wrong.
        let open = !up || self.link_hover;
        let text2 = t.text2;
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(48.))
            .px(px(GUTTER + 4.))
            .child(admin::mark(picture, t))
            .child(admin::entry(s.server_name.clone(), role, t, cx))
            // Down a touch, to sit on the name's letters as the gear does.
            .when(role.is_admin(), |d| d.child(admin::role_chip(role, t).relative().top(px(1.))))
            .child(div().flex_1())
            .children(crate::ui::updates::button(cx))
            .child(
                // Texel's live pill: a glowing dot, and the word beside it when there is something to say.
                div()
                    .id("link")
                    .flex()
                    .items_center()
                    .h(px(28.))
                    .px(px(9.))
                    .rounded_full()
                    .border_1()
                    .child(status_dot(color, true))
                    .on_hover(cx.listener(|this, on: &bool, _, cx| {
                        this.link_hover = *on;
                        cx.notify();
                    }))
                    .with_spring("link-open", SpringAnimation::new(SpringConfig::new(420., 41., 1.)).to(open), move |d, phase| {
                        let p = phase.0.clamp(0., 1.);
                        // The label's width is not known before layout; 120 is room for the longest.
                        d.border_color(color.opacity(0.4 * p)).child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .ml(px(8. * p))
                                .max_w(px(120. * p))
                                .opacity(p)
                                .font_family(crate::theme::MONO)
                                .text_size(px(11.))
                                .text_color(text2)
                                .child(state),
                        )
                    }),
            )
            .child(
                tool_button("toggle-members", "users", show_members, if show_members { t.accent } else { t.text2 }, t)
                    .tooltip(tip(
                        if show_members { tr!("Hide members", "Ocultar membros") } else { tr!("Show members", "Mostrar membros") },
                        t,
                    ))
                    .on_click(cx.listener(|_, _, _, cx| set_prefs(cx, |p| p.show_members = !p.show_members))),
            )
            .child(
                icon_button("settings", "settings", t)
                    .tooltip(tip(tr!("Settings (Ctrl+,)", "Configurações (Ctrl+,)"), t))
                    .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
            )
            .child(
                icon_button("sign-out", "log-out", t)
                    .tooltip(tip(tr!("Sign out", "Sair da conta"), t))
                    .on_click(cx.listener(|this, _, window, cx| this.sign_out(window, cx))),
            )
    }

    fn render_self(&mut self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let me = self.session.read(cx).me.clone();
        let img = self.session.update(cx, |s, cx| s.avatar(me.id, cx));
        let call = self.call.as_ref().map(|c| c.read(cx));
        let (muted, deafened) = call.map(|c| (c.muted, c.deafened)).unwrap_or((false, false));
        let in_call = call.is_some();
        let speaking = call.is_some_and(|c| c.is_speaking(me.id, cx));
        let status = if !in_call {
            tr!("Online", "Online")
        } else if deafened {
            tr!("Deafened", "Som desativado")
        } else if muted {
            tr!("Muted", "Silenciado")
        } else {
            tr!("In voice", "Na voz")
        };
        let hover = t.layer_hover;
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .m(px(8.))
            .p(px(6.))
            .rounded(px(radius::CARD))
            .bg(t.layer)
            .border_1()
            .border_color(t.stroke)
            .child(
                div()
                    .id("self-profile")
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .items_center()
                    .gap(px(8.))
                    .p(px(2.))
                    .rounded(px(radius::INNER))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(avatar(me.name(), img, 32., speaking.then_some(t.success)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w(px(0.))
                            .child(
                                div().truncate().text_size(px(13.5)).font_weight(gpui::FontWeight::SEMIBOLD).child(me.name().to_string()),
                            )
                            .child(div().truncate().text_size(px(11.5)).text_color(t.text3).child(status)),
                    )
                    .tooltip(tip(tr!("Your profile and settings", "Seu perfil e configurações"), t))
                    .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
            )
            .child(
                tool_button(
                    "mute",
                    if muted { "mic-off" } else { "mic" },
                    muted && in_call,
                    if muted && in_call { t.critical } else { t.text2 },
                    t,
                )
                .when(!in_call, |d| d.opacity(0.4))
                .tooltip(tip(
                    if muted {
                        tr!("Unmute (Ctrl+Shift+M)", "Reativar o microfone (Ctrl+Shift+M)")
                    } else {
                        tr!("Mute (Ctrl+Shift+M)", "Silenciar (Ctrl+Shift+M)")
                    },
                    t,
                ))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_mute(cx))),
            )
            .child(
                tool_button(
                    "deafen",
                    if deafened { "headphones-off" } else { "headphones" },
                    deafened && in_call,
                    if deafened && in_call { t.critical } else { t.text2 },
                    t,
                )
                .when(!in_call, |d| d.opacity(0.4))
                .tooltip(tip(
                    if deafened {
                        tr!("Undeafen (Ctrl+Shift+D)", "Reativar o som (Ctrl+Shift+D)")
                    } else {
                        tr!("Deafen (Ctrl+Shift+D)", "Desativar o som (Ctrl+Shift+D)")
                    },
                    t,
                ))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_deafen(cx))),
            )
    }

    fn render_voice_panel(&mut self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let call = self.call.clone()?;
        let c = call.read(cx);
        let name = self.session.read(cx).place_name(c.place);
        let private = c.place.conversation().is_some();
        let ringing = c.ringing(cx);
        let state = c.state.clone();
        let voice = c.voice.clone();
        let ping = voice.as_ref().and_then(|v| v.read(cx).ping_ms);
        let mic_failed = voice.as_ref().is_some_and(|v| v.read(cx).mic_failed());
        let (camera_on, screen_on) = stage::sharing(&self.session, voice.as_ref(), cx);
        let (label_text, color) = match state {
            _ if ringing => (tr!("Ringing…", "Chamando…"), t.caution),
            CallState::Connected if private => (tr!("In a private call", "Em chamada privada"), t.success),
            CallState::Connected if mic_failed => (tr!("Microphone not sent", "Microfone não enviado"), t.caution),
            CallState::Connected => (tr!("Voice connected", "Voz conectada"), t.success),
            CallState::Joining => (tr!("Connecting…", "Conectando…"), t.caution),
            CallState::Reconnecting => (tr!("Reconnecting…", "Reconectando…"), t.caution),
        };
        let bars = signal_bars(ping, t);
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .mx(px(8.))
                .p(px(10.))
                .rounded(px(radius::CARD))
                // The state shows in the edge alone; the card stays the colour of the one below it.
                .bg(t.layer)
                .border_1()
                .border_color(color.opacity(0.65))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().id("ping").child(bars).tooltip(tip(
                            ping.map(|p| format!("Ping {p} ms")).unwrap_or_else(|| tr!("Measuring ping…", "Medindo o ping…").into()),
                            t,
                        )))
                        .child(
                            div()
                                .id("voice-title")
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w(px(0.))
                                .cursor_pointer()
                                .child(div().text_size(px(13.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(color).child(label_text))
                                .child(div().truncate().text_size(px(12.)).text_color(t.text2).child(name))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_stage(cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            tool_button("hang-up", "phone-off", false, t.critical, t)
                                .tooltip(tip(if private { tr!("Hang up", "Desligar") } else { tr!("Leave the channel", "Sair do canal") }, t))
                                .on_click(cx.listener(|this, _, _, cx| this.leave_voice(cx))),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(
                            panel_button("camera-toggle", if camera_on { "camera-off" } else { "camera" }, camera_on, t)
                                .tooltip(tip(
                                    if camera_on {
                                        tr!("Turn the camera off", "Desligar a câmera")
                                    } else {
                                        tr!("Turn the camera on", "Ligar a câmera")
                                    },
                                    t,
                                ))
                                .on_click(cx.listener(|this, _, window, cx| this.toggle_camera(window, cx))),
                        )
                        .child(
                            panel_button("screen-toggle", if screen_on { "screen-share-off" } else { "screen-share" }, screen_on, t)
                                .tooltip(tip(
                                    if screen_on {
                                        tr!("Change or stop what you share", "Trocar ou parar o que você compartilha")
                                    } else {
                                        tr!("Share your screen", "Compartilhar a tela")
                                    },
                                    t,
                                ))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, e: &MouseDownEvent, window, cx| {
                                        cx.stop_propagation();
                                        this.toggle_screen(e.position, window, cx)
                                    }),
                                ),
                        )
                        .when(!private, |d| d.child(
                            panel_button("soundpad", "soundboard", false, t)
                                .tooltip(tip(tr!("Soundboard: play a sound for everyone", "Painel de sons: toque um som para todos"), t))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, e: &MouseDownEvent, window, cx| {
                                        cx.stop_propagation();
                                        this.open_soundpad(e.position, window, cx)
                                    }),
                                ),
                        )),
                )
                .into_any_element(),
        )
    }
}

fn panel_button(id: &'static str, glyph: &'static str, on: bool, t: &Theme) -> gpui::Stateful<gpui::Div> {
    let hover = t.control_hover;
    div()
        .id(id)
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .h(px(34.))
        .rounded(px(8.))
        .cursor_pointer()
        .bg(if on { t.accent_soft } else { t.control })
        .border_1()
        .border_color(if on { t.accent.opacity(0.5) } else { t.stroke })
        .when(!on, |d| d.hover(move |s| s.bg(hover)))
        .active(|s| s.opacity(0.8))
        .child(icon(glyph, 16., if on { t.accent } else { t.text }))
}

/// Ping as three bars: good to 60 ms, fair to 150, poor above.
fn signal_bars(ping: Option<u32>, t: &Theme) -> gpui::Div {
    let (lit, color) = match ping {
        Some(p) if p <= 60 => (3, t.success),
        Some(p) if p <= 150 => (2, t.caution),
        Some(_) => (1, t.critical),
        None => (0, t.text3),
    };
    div().flex().items_end().gap(px(2.)).h(px(14.)).children(
        (0..3).map(|i| div().w(px(3.)).h(px(5. + i as f32 * 4.)).rounded(px(1.)).bg(if i < lit { color } else { t.stroke_strong })),
    )
}

/// The volume slider used in menus and tiles: 0 to 350%, a mark at 100%, amber past it.
pub struct VolumeSlider<F: Fn(f32, &mut App) + 'static> {
    gain: f32,
    on_change: F,
    width: Pixels,
}

impl<F: Fn(f32, &mut App) + 'static> VolumeSlider<F> {
    pub fn new(gain: f32, on_change: F) -> Self {
        VolumeSlider { gain, on_change, width: px(220.) }
    }

    pub fn width(mut self, width: Pixels) -> Self {
        self.width = width;
        self
    }

    pub fn render(self, t: &Theme, _: &mut App) -> AnyElement {
        let on_change = std::rc::Rc::new(self.on_change);
        let gain = self.gain;
        let color = if gain > 1. { t.caution } else { t.accent };
        let id = gpui::ElementId::from(("volume", (gain * 1000.) as u64));
        let bounds: std::rc::Rc<std::cell::Cell<gpui::Bounds<Pixels>>> = Default::default();
        let (b1, b2) = (bounds.clone(), bounds.clone());
        let (c1, c2) = (on_change.clone(), on_change.clone());
        let to_gain = move |b: gpui::Bounds<Pixels>, x: Pixels| {
            let w = f32::from(b.size.width).max(1.);
            let f = (f32::from(x - b.origin.x) / w).clamp(0., 1.);
            ((f * MAX_GAIN) / 0.05).round() * 0.05
        };
        div()
            .id(id)
            .relative()
            .w(self.width)
            .h(px(20.))
            .flex()
            .items_center()
            .cursor_pointer()
            .child(gpui::canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {}).absolute().inset_0())
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(4.))
                    .rounded(px(2.))
                    .bg(t.well)
                    .child(div().absolute().left_0().top_0().h_full().rounded(px(2.)).bg(color).w(gpui::relative(gain / MAX_GAIN)))
                    .child(div().absolute().top(px(-3.)).h(px(10.)).w(px(2.)).bg(t.text3).left(gpui::relative(1. / MAX_GAIN))),
            )
            .child(
                div()
                    .absolute()
                    .top(px(3.))
                    .size(px(14.))
                    .ml(px(-7.))
                    .left(gpui::relative(gain / MAX_GAIN))
                    .rounded_full()
                    .bg(gpui::white())
                    .border_2()
                    .border_color(color),
            )
            .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, _, cx| {
                c1(to_gain(b1.get(), e.position.x), cx);
                cx.stop_propagation();
            })
            .on_mouse_move(move |e: &gpui::MouseMoveEvent, _, cx| {
                if e.pressed_button == Some(MouseButton::Left) {
                    c2(to_gain(b2.get(), e.position.x), cx);
                }
            })
            .into_any_element()
    }
}

impl Render for ServerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let show_members = prefs(cx).show_members && f32::from(window.viewport_size().width) > 900. * crate::theme::scale();
        let narrow = f32::from(window.viewport_size().width) < 720. * crate::theme::scale();
        let topbar = self.render_topbar(&t, cx);
        let sidebar = self.sidebar.clone().cached(StyleRefinement::default().flex_1().min_h(px(0.)).w_full());
        let voice_panel = self.render_voice_panel(&t, cx);
        let self_strip = self.render_self(&t, cx);
        // Told through the voice, not the tiles: whatever a draw reads draws the window again when
        // it changes, and a tile changes with every frame.
        if let Some(voice) = self.call.as_ref().and_then(|c| c.read(cx).voice.as_ref()) {
            voice.read(cx).hide_streams(self.center != Center::Stage);
        }
        let center: AnyElement = match self.center {
            Center::Chat(room) => self.chat_view(room, window, cx).cached(StyleRefinement::default().size_full()).into_any_element(),
            Center::Stage => self.render_stage(&t, window, cx).into_any_element(),
        };
        let members = show_members.then(|| self.members.clone().cached(StyleRefinement::default().size_full()));
        div()
            .key_context("Server")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .flex_col()
            .child(topbar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .gap(px(GUTTER))
                    .px(px(GUTTER))
                    .pb(px(GUTTER))
                    .when(!narrow, |d| d.child(pane(&t).w(px(268.)).flex_none().child(sidebar).children(voice_panel).child(self_strip)))
                    .child(pane(&t).flex_1().min_w(px(0.)).child(center))
                    .children(members.map(|m| pane(&t).w(px(232.)).flex_none().child(m))),
            )
    }
}

/// Every keyboard shortcut, as keycaps.
struct Shortcuts {
    focus: FocusHandle,
}

impl gpui::EventEmitter<overlay::Dismiss> for Shortcuts {}

impl Focusable for Shortcuts {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Shortcuts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let rows: [(&[&str], &str); 9] = [
            (&["Ctrl", "Shift", "M"], tr!("Mute or unmute", "Silenciar ou reativar o microfone")),
            (&["Ctrl", "Shift", "D"], tr!("Deafen or undeafen", "Desativar ou reativar o som")),
            (&["Enter"], tr!("Send the message", "Enviar a mensagem")),
            (&["Shift", "Enter"], tr!("New line in a message", "Nova linha na mensagem")),
            (&["Ctrl", "V"], tr!("Paste a picture as an attachment", "Colar uma imagem como anexo")),
            (&["Tab"], tr!("Pick the highlighted @mention", "Escolher a @menção destacada")),
            (&["Ctrl", ","], tr!("Settings", "Configurações")),
            (&["Ctrl", "/"], tr!("This list", "Esta lista")),
            (&["Esc"], tr!("Close a dialog or menu", "Fechar uma janela ou menu")),
        ];
        let mut list = div().flex().flex_col().gap(px(2.));
        for (keys, what) in rows {
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .py(px(7.))
                    .border_b_1()
                    .border_color(t.stroke)
                    .child(body(what, t.text2))
                    .child(div().flex().gap(px(4.)).children(keys.iter().map(|k| keycap(*k, &t)))),
            );
        }
        overlay::dialog_card(&t, 440.).track_focus(&self.focus).child(
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .p(px(22.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(title(tr!("Keyboard shortcuts", "Atalhos de teclado"), t.text))
                        .child(
                            icon_button("shortcuts-close", "close", &t)
                                .tooltip(tip(tr!("Close (Esc)", "Fechar (Esc)"), &t))
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(overlay::Dismiss))),
                        ),
                )
                .child(list)
                .child(caption(
                    tr!(
                        "Shortcuts with Ctrl work anywhere in the window, even while typing.",
                        "Os atalhos com Ctrl funcionam em qualquer lugar da janela, até enquanto você digita."
                    ),
                    t.text3,
                )),
        )
    }
}
