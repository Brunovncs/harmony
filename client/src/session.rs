//! What the window knows about the server it is connected to: who is there, the channels and
//! who is in each, chat history, emoji, soundpad clips. One entity, fed by the realtime socket and
//! by HTTP replies; views observe it.

use crate::core::api::{Api, ApiError};
use crate::core::cache::Cache;
use crate::core::lru::Lru;
use crate::core::realtime::{self, Realtime};
use crate::core::types::*;
use crate::core::{self};
use gpui::{App, AppContext, Context, Entity, EventEmitter, RenderImage, Task};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Messages per page of history, as the server pages them.
const PAGE: usize = 50;
/// What decoded pictures may hold before the least recently drawn are let go.
const PICTURE_BUDGET: usize = 192 << 20;
/// Pictures are decoded at most twice the biggest box they are drawn in (an attachment's
/// 420 × 300), so a huge upload costs a thumbnail's worth of memory.
const THUMBNAIL: (u32, u32) = (840, 600);
/// The lightbox gets the picture at its own size, up to this on either side.
const FULL: (u32, u32) = (4096, 4096);

#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    /// Signed in and the socket is up.
    Up,
    /// The socket dropped; it is coming back on its own.
    Reconnecting,
}

#[derive(Default)]
pub struct ChatLog {
    pub messages: Vec<Message>,
    pub pinned: Vec<Message>,
    pub loaded: bool,
    /// The latest page is on its way (the first load, or catching up after a reconnect).
    fetching: bool,
    loading_older: bool,
    /// Pushes that arrived while the latest page was on its way, replayed onto it.
    pending: Vec<LogOp>,
    /// Older messages exist before the first one shown.
    pub more_before: bool,
    pub error: Option<String>,
}

/// A push that changes a chat log.
#[derive(Clone, Debug)]
enum LogOp {
    Added(Message),
    Updated(Message),
    Deleted(MessageId),
    Reactions(MessageId, Vec<Reaction>),
}

impl ChatLog {
    fn push(&mut self, op: LogOp) {
        if self.fetching {
            self.pending.push(op);
        } else if self.loaded {
            self.apply(op);
        }
    }

    fn apply(&mut self, op: LogOp) {
        match op {
            LogOp::Added(m) => {
                if !self.messages.iter().any(|x| x.id == m.id) {
                    let at = self.messages.partition_point(|x| x.id < m.id);
                    self.messages.insert(at, m);
                }
            }
            LogOp::Updated(m) => {
                if let Some(x) = self.messages.iter_mut().find(|x| x.id == m.id) {
                    *x = m.clone();
                }
                self.pinned.retain(|x| x.id != m.id);
                if m.pinned {
                    self.pinned.insert(0, m);
                    self.pinned.sort_by_key(|m| std::cmp::Reverse(m.created_at));
                }
            }
            LogOp::Deleted(id) => {
                self.messages.retain(|m| m.id != id);
                self.pinned.retain(|m| m.id != id);
            }
            LogOp::Reactions(id, r) => {
                for m in self.messages.iter_mut().chain(self.pinned.iter_mut()).filter(|m| m.id == id) {
                    m.reactions = r.clone();
                }
            }
        }
    }

    /// The latest page arrived: within its span it is the truth (edits, deletions, messages
    /// missed while away), older messages already shown stay, and the pushes that came in
    /// meanwhile go on top.
    fn land(&mut self, page: History) {
        let full = page.messages.len() >= PAGE;
        let oldest = page.messages.first().map(|m| m.id);
        let gap = oldest.is_some_and(|o| self.messages.last().is_some_and(|m| m.id < o));
        if !self.loaded || !full || gap {
            self.more_before = full;
            self.messages = page.messages;
        } else if let Some(oldest) = oldest {
            self.messages.retain(|m| m.id < oldest);
            self.messages.extend(page.messages);
        }
        self.pinned = page.pinned;
        self.loaded = true;
        self.fetching = false;
        self.error = None;
        for op in std::mem::take(&mut self.pending) {
            self.apply(op);
        }
    }

    fn fetch_failed(&mut self, message: String) {
        self.fetching = false;
        let pending = std::mem::take(&mut self.pending);
        if self.loaded {
            for op in pending {
                self.apply(op);
            }
        } else {
            self.error = Some(message);
        }
    }
}

#[derive(Clone)]
pub enum Picture {
    Loading,
    Ready(Arc<RenderImage>),
    Failed,
}

/// A picture as the session holds it.
enum Slot {
    Loading,
    Ready(Arc<RenderImage>),
    /// `retry_at: None` when trying again would not help (not a picture, gone from the server).
    Failed {
        retry_at: Option<Instant>,
        tries: u32,
    },
}

/// Something the rest of the window should react to, beyond redrawing.
pub enum SessionEvent {
    /// A message mentioning you arrived in a channel that is not open.
    Mentioned,
    /// A new message in any channel (for scrolling and sounds).
    Message(Message),
    /// The roster of the voice channel you are in changed.
    Roster(ChannelId),
    SoundpadPlay {
        hash: String,
    },
    /// An admin moved you (`None`: disconnected you).
    Moved(Option<ChannelId>),
    /// The socket is back after a drop: rejoin voice.
    Reconnected,
    /// The account was removed or the token refused: back to the sign-in screen.
    SignedOut(SignOutReason),
    Toast(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignOutReason {
    /// The server no longer takes this sign-in.
    Expired,
    /// An owner deleted the account.
    Removed,
}

impl SignOutReason {
    pub fn message(self) -> &'static str {
        match self {
            SignOutReason::Expired => tr!("This sign-in is no longer valid. Sign in again.", "Este login expirou. Entre de novo."),
            SignOutReason::Removed => tr!("Your account was removed from this server.", "Sua conta foi removida deste servidor."),
        }
    }
}

pub struct Session {
    pub api: Api,
    pub cache: Cache,
    pub realtime: Option<Realtime>,
    pub link: Link,
    pub me: User,
    pub server_name: String,
    /// The server's picture, by upload hash.
    pub server_icon: Option<String>,
    /// The server's settings as `GET /api/server` and the `server` push give them; asked for
    /// when the server settings open.
    pub server_info: Option<ServerInfo>,
    pub users: HashMap<UserId, User>,
    pub online: HashSet<UserId>,
    pub channels: Vec<Channel>,
    pub groups: Vec<Group>,
    pub rosters: HashMap<ChannelId, Vec<Member>>,
    /// Locked channels this account holds a grant for.
    pub unlocked: HashSet<ChannelId>,
    pub voice_cap: i64,
    pub ice_servers: Vec<IceServer>,
    pub emojis: Vec<CustomEmoji>,
    pub clips: Vec<Clip>,
    pub chats: HashMap<ChannelId, ChatLog>,
    /// Messages naming you (or everyone) per channel since you last opened it. Counted here from
    /// the pushes and nowhere else: nothing is sent to the server, and it goes when the app closes.
    pub mentioned: HashMap<ChannelId, u32>,
    pub open_channel: Option<ChannelId>,
    /// Channels with messages from others since you last looked at them. Counted from the pushes,
    /// like the mentions, and kept nowhere else.
    pub unread: HashSet<ChannelId>,
    /// The chat on screen; none while the stage is. `open_channel` stays on the last chat opened,
    /// which is not the same thing once you go to the stage.
    pub viewing: Option<ChannelId>,
    pictures: Lru<String, Slot>,
    _pump: Task<()>,
}

impl EventEmitter<SessionEvent> for Session {}

impl Session {
    /// Opens the socket and starts filling in what the server knows.
    pub fn start(
        api: Api,
        cache: Cache,
        me: User,
        server_name: String,
        server_icon: Option<String>,
        ice_servers: Vec<IceServer>,
        cx: &mut App,
    ) -> Entity<Session> {
        cx.new(|cx| {
            let (tx, rx) = async_channel::unbounded();
            let rt = Realtime::start(core::runtime().handle(), api.clone(), tx);
            let pump = cx.spawn(async move |this, cx| {
                while let Ok(ev) = rx.recv().await {
                    if this.update(cx, |s: &mut Session, cx| s.on_realtime(ev, cx)).is_err() {
                        break;
                    }
                }
            });
            // The cache's index goes to disk every few seconds, so a crash loses little.
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    let Ok(cache) = this.read_with(cx, |s: &Session, _| s.cache.clone()) else { break };
                    cx.background_executor().spawn(async move { cache.flush() }).await;
                }
            })
            .detach();
            let mut s = Session {
                api,
                cache,
                realtime: Some(rt),
                link: Link::Reconnecting,
                users: HashMap::from([(me.id, me.clone())]),
                me,
                server_name,
                server_icon,
                server_info: None,
                online: HashSet::new(),
                channels: Vec::new(),
                groups: Vec::new(),
                rosters: HashMap::new(),
                unlocked: HashSet::new(),
                voice_cap: 16,
                ice_servers,
                emojis: Vec::new(),
                clips: Vec::new(),
                chats: HashMap::new(),
                mentioned: HashMap::new(),
                open_channel: None,
                unread: HashSet::new(),
                viewing: None,
                pictures: Lru::new(PICTURE_BUDGET),
                _pump: pump,
            };
            s.refresh_lists(cx);
            s
        })
    }

    pub fn stop(&mut self) {
        if let Some(rt) = self.realtime.take() {
            rt.stop();
        }
        self.cache.flush_all();
    }

    /// Users, emoji and soundpad clips, and the REST channel list for the `unlocked` flags.
    pub fn refresh_lists(&mut self, cx: &mut Context<Self>) {
        let api = self.api.clone();
        cx.spawn(async move |this, cx| {
            let (users, emojis, clips, channels) =
                core::run(async move { tokio::join!(api.users(), api.emojis(), api.soundpad(), api.channels()) }).await;
            let _ = this.update(cx, |s, cx| {
                if let Ok(users) = users {
                    s.users = users.into_iter().map(|u| (u.id, u)).collect();
                    if let Some(me) = s.users.get(&s.me.id) {
                        s.me = me.clone();
                    }
                }
                if let Ok(e) = emojis {
                    s.emojis = e;
                }
                if let Ok(c) = clips {
                    s.clips = c;
                }
                if let Ok(snap) = channels {
                    s.unlocked = snap.channels.iter().filter(|c| c.unlocked == Some(true)).map(|c| c.id).collect();
                    if s.channels.is_empty() {
                        s.apply_snapshot(snap);
                    }
                }
                s.update_keep_set(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_snapshot(&mut self, snap: Snapshot) {
        self.channels = snap.channels;
        self.groups = snap.groups;
        self.online = snap.online.into_iter().collect();
        self.rosters = snap.rosters.into_iter().filter_map(|(k, v)| Some((k.parse().ok()?, v))).collect();
        if let Some(cap) = snap.voice_cap {
            self.voice_cap = cap;
        }
        if let Some(me) = snap.user {
            self.users.insert(me.id, me.clone());
            self.me = me;
        }
    }

    fn on_realtime(&mut self, ev: realtime::Event, cx: &mut Context<Self>) {
        match ev {
            realtime::Event::Up(snap) => {
                let was_down = self.link == Link::Reconnecting && !self.channels.is_empty();
                if let Some(snap) = snap {
                    self.apply_snapshot(*snap);
                }
                self.link = Link::Up;
                if was_down {
                    cx.emit(SessionEvent::Reconnected);
                    self.refresh_lists(cx);
                    self.catch_up(cx);
                }
            }
            realtime::Event::Down => self.link = Link::Reconnecting,
            realtime::Event::Rejected => cx.emit(SessionEvent::SignedOut(SignOutReason::Expired)),
            realtime::Event::Push(v) => self.on_push(v, cx),
        }
        cx.notify();
    }

    fn on_push(&mut self, v: Value, cx: &mut Context<Self>) {
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        let parse = |key: &str| v.get(key).cloned().unwrap_or(Value::Null);
        match kind.as_str() {
            "presence" => {
                if let Some(online) = list_of::<UserId>(parse("online")) {
                    self.online = online.into_iter().collect();
                }
            }
            "channels" => {
                if let Some(c) = list_of(parse("channels")) {
                    self.channels = c;
                }
                if let Some(g) = list_of(parse("groups")) {
                    self.groups = g;
                }
            }
            "voice:roster" => {
                let id = v.get("channelId").and_then(Value::as_i64).unwrap_or(-1);
                if let Some(roster) = list_of::<Member>(parse("roster")) {
                    self.rosters.insert(id, roster);
                    cx.emit(SessionEvent::Roster(id));
                }
            }
            "message" => {
                if let Ok(m) = serde_json::from_value::<Message>(parse("message")) {
                    let mine = m.user_id == Some(self.me.id);
                    let for_me = m.mentions.contains(&self.me.id) || m.mentions_everyone;
                    let away = self.viewing != Some(m.channel_id);
                    if !mine && away {
                        self.unread.insert(m.channel_id);
                    }
                    if for_me && !mine && away {
                        *self.mentioned.entry(m.channel_id).or_default() += 1;
                        cx.emit(SessionEvent::Mentioned);
                    }
                    if let Some(log) = self.chats.get_mut(&m.channel_id) {
                        log.push(LogOp::Added(m.clone()));
                    }
                    cx.emit(SessionEvent::Message(m));
                }
            }
            "message:updated" => {
                if let Ok(m) = serde_json::from_value::<Message>(parse("message"))
                    && let Some(log) = self.chats.get_mut(&m.channel_id)
                {
                    log.push(LogOp::Updated(m));
                }
            }
            "message:deleted" => {
                let id = v.get("id").and_then(Value::as_i64).unwrap_or(-1);
                let ch = v.get("channelId").and_then(Value::as_i64).unwrap_or(-1);
                if let Some(log) = self.chats.get_mut(&ch) {
                    log.push(LogOp::Deleted(id));
                }
            }
            "message:reactions" => {
                let id = v.get("id").and_then(Value::as_i64).unwrap_or(-1);
                let ch = v.get("channelId").and_then(Value::as_i64).unwrap_or(-1);
                if let (Some(log), Some(r)) = (self.chats.get_mut(&ch), list_of::<Reaction>(parse("reactions"))) {
                    log.push(LogOp::Reactions(id, r));
                }
            }
            "emojis" => {
                if let Some(e) = list_of(parse("emojis")) {
                    self.emojis = e;
                    self.update_keep_set(cx);
                }
            }
            "soundpad" => {
                if let Some(c) = list_of(parse("clips")) {
                    self.clips = c;
                    self.update_keep_set(cx);
                }
            }
            "server" => {
                if let Ok(s) = serde_json::from_value::<ServerInfo>(parse("server")) {
                    self.set_server_info(s, cx);
                }
            }
            "user:updated" => {
                if let Ok(u) = serde_json::from_value::<User>(parse("user")) {
                    if u.id == self.me.id {
                        self.me = u.clone();
                    }
                    self.users.insert(u.id, u);
                    self.update_keep_set(cx);
                }
            }
            "accounts" => {
                if let Some(users) = list_of::<User>(parse("users")) {
                    self.users = users.into_iter().map(|u| (u.id, u)).collect();
                }
            }
            "kicked" => cx.emit(SessionEvent::SignedOut(SignOutReason::Removed)),
            "soundpad:play" => {
                let hash = v.get("hash").and_then(Value::as_str).unwrap_or("").to_string();
                cx.emit(SessionEvent::SoundpadPlay { hash });
            }
            "voice:moved" => {
                let to = v.get("channelId").and_then(Value::as_i64);
                let by = v.get("by").and_then(Value::as_str).unwrap_or(tr!("an admin", "um admin"));
                cx.emit(SessionEvent::Toast(match to {
                    Some(_) => trf!("{} moved you to another channel.", "{} moveu você para outro canal.", by),
                    None => trf!("{} disconnected you from voice.", "{} desconectou você da voz.", by),
                }));
                cx.emit(SessionEvent::Moved(to));
            }
            _ => {}
        }
    }

    // Lookups.

    pub fn channel(&self, id: ChannelId) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    pub fn user_by_nick(&self, nick: &str) -> Option<&User> {
        self.users.values().find(|u| u.nickname.eq_ignore_ascii_case(nick))
    }

    pub fn display_name(&self, id: Option<UserId>, fallback: Option<&str>) -> String {
        id.and_then(|id| self.users.get(&id))
            .map(|u| u.name().to_string())
            .or(fallback.map(str::to_string))
            .unwrap_or_else(|| tr!("someone", "alguém").into())
    }

    pub fn emoji(&self, name: &str) -> Option<&CustomEmoji> {
        self.emojis.iter().find(|e| e.name == name)
    }

    /// Channels in drawing order, grouped: (group, channels) with ungrouped first.
    pub fn layout(&self) -> Vec<(Option<&Group>, Vec<&Channel>)> {
        let mut channels: Vec<&Channel> = self.channels.iter().collect();
        channels.sort_by_key(|c| c.position);
        let mut groups: Vec<&Group> = self.groups.iter().collect();
        groups.sort_by_key(|g| g.position);
        let mut out =
            vec![(None, channels.iter().copied().filter(|c| c.group_id.is_none_or(|g| !groups.iter().any(|x| x.id == g))).collect())];
        for g in groups {
            out.push((Some(g), channels.iter().copied().filter(|c| c.group_id == Some(g.id)).collect()));
        }
        out
    }

    pub fn can_read(&self, ch: &Channel) -> bool {
        !ch.locked || self.unlocked.contains(&ch.id)
    }

    // Pictures: avatars, emoji and attachments, by hash.

    /// The picture behind a hash, small enough for any box it is drawn in, loading it in the
    /// background the first time it is asked for (and again later if that failed).
    pub fn picture(&mut self, hash: &str, cx: &mut Context<Self>) -> Picture {
        let tries = match self.pictures.get(hash) {
            Some(Slot::Loading) => return Picture::Loading,
            Some(Slot::Ready(img)) => return Picture::Ready(img.clone()),
            Some(Slot::Failed { retry_at: Some(at), tries }) if Instant::now() >= *at => *tries,
            Some(Slot::Failed { .. }) => return Picture::Failed,
            None => 0,
        };
        self.put_picture(hash.to_string(), Slot::Loading, 0, cx);
        let (api, cache, h) = (self.api.clone(), self.cache.clone(), hash.to_string());
        cx.spawn(async move |this, cx| {
            let key = h.clone();
            let got = core::run(async move { cache.get(&api, &h).await }).await;
            let (slot, cost) = match got {
                Ok((bytes, ct)) => match cx.background_executor().spawn(async move { decode_picture(&bytes, &ct, THUMBNAIL) }).await {
                    Ok((img, cost)) => (Slot::Ready(img), cost),
                    Err(e) => {
                        log::info!("picture {key} did not decode: {e}");
                        (Slot::Failed { retry_at: None, tries }, 0)
                    }
                },
                Err(e) if e.code == ErrorCode::NoSuchFile => (Slot::Failed { retry_at: None, tries }, 0),
                Err(e) => {
                    log::info!("picture {key} did not load: {e}");
                    (Slot::Failed { retry_at: Some(Instant::now() + retry_after(tries)), tries: tries + 1 }, 0)
                }
            };
            let retry = matches!(slot, Slot::Failed { retry_at: Some(_), .. });
            let stored = this.update(cx, |s, cx| {
                s.put_picture(key, slot, cost, cx);
                cx.notify();
            });
            if stored.is_err() || !retry {
                return;
            }
            // Draw again once the wait is over, so whoever shows it asks again.
            cx.background_executor().timer(retry_after(tries)).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
        Picture::Loading
    }

    fn put_picture(&mut self, hash: String, slot: Slot, cost: usize, cx: &mut Context<Self>) {
        for old in self.pictures.insert(hash, slot, cost) {
            if let Slot::Ready(img) = old {
                cx.drop_image(img, None);
            }
        }
    }

    /// The picture at its own size (to `FULL`), for the lightbox. Not kept here: whoever asked
    /// drops it from the GPU when done.
    pub fn full_picture(&self, hash: &str, cx: &mut Context<Self>) -> Task<Option<Arc<RenderImage>>> {
        let (api, cache, h) = (self.api.clone(), self.cache.clone(), hash.to_string());
        cx.spawn(async move |_, cx| {
            let (bytes, ct) = core::run(async move { cache.get(&api, &h).await }).await.ok()?;
            cx.background_executor().spawn(async move { decode_picture(&bytes, &ct, FULL) }).await.ok().map(|(img, _)| img)
        })
    }

    pub fn avatar(&mut self, user: UserId, cx: &mut Context<Self>) -> Option<Arc<RenderImage>> {
        let hash = self.users.get(&user)?.avatar_hash.clone()?;
        match self.picture(&hash, cx) {
            Picture::Ready(img) => Some(img),
            _ => None,
        }
    }

    /// The server's picture, once it has loaded.
    pub fn server_picture(&mut self, cx: &mut Context<Self>) -> Option<Arc<RenderImage>> {
        match self.picture(&self.server_icon.clone()?, cx) {
            Picture::Ready(img) => Some(img),
            _ => None,
        }
    }

    /// The server's settings as a reply or a push gives them.
    pub fn set_server_info(&mut self, info: ServerInfo, cx: &mut Context<Self>) {
        self.server_name = info.name.clone();
        self.server_icon = info.icon_hash.clone();
        self.server_info = Some(info);
        self.update_keep_set(cx);
    }

    /// The cache keeps, whatever its budget, what is drawn all the time: avatars, emoji, sounds,
    /// and every saved server's picture, so the rail still has them offline.
    fn update_keep_set(&self, cx: &App) {
        let keep = self
            .users
            .values()
            .filter_map(|u| u.avatar_hash.clone())
            .chain(self.emojis.iter().map(|e| e.hash.clone()))
            .chain(self.clips.iter().map(|c| c.hash.clone()))
            .chain(self.server_icon.clone())
            .chain(crate::prefs::prefs(cx).saved_servers.iter().filter(|s| !s.icon.is_empty()).map(|s| s.icon.clone()));
        self.cache.set_keep(keep);
    }

    // Chat.

    pub fn open(&mut self, channel: ChannelId, cx: &mut Context<Self>) {
        self.open_channel = Some(channel);
        self.viewing = Some(channel);
        self.mentioned.remove(&channel);
        self.unread.remove(&channel);
        let log = self.chats.entry(channel).or_default();
        if !log.loaded && !log.fetching {
            self.fetch_latest(channel, cx);
        }
        cx.notify();
    }

    /// No chat is on screen any more: the stage took its place.
    pub fn leave_chat(&mut self, cx: &mut Context<Self>) {
        if self.viewing.take().is_some() {
            cx.notify();
        }
    }

    /// After a reconnect: whatever happened in the channels already shown while the socket was
    /// down (and another go at the open one, if it never loaded).
    fn catch_up(&mut self, cx: &mut Context<Self>) {
        let due: Vec<ChannelId> =
            self.chats.iter().filter(|(id, l)| !l.fetching && (l.loaded || self.open_channel == Some(**id))).map(|(id, _)| *id).collect();
        for id in due {
            self.fetch_latest(id, cx);
        }
    }

    fn fetch_latest(&mut self, channel: ChannelId, cx: &mut Context<Self>) {
        self.chats.entry(channel).or_default().fetching = true;
        let api = self.api.clone();
        cx.spawn(async move |this, cx| {
            let got = core::run(async move { api.messages(channel, None).await }).await;
            let _ = this.update(cx, |s, cx| {
                let log = s.chats.entry(channel).or_default();
                match got {
                    Ok(h) => log.land(h),
                    Err(e) => log.fetch_failed(e.message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// One more page of history before the oldest message shown.
    pub fn load_older(&mut self, channel: ChannelId, cx: &mut Context<Self>) {
        let Some(log) = self.chats.get_mut(&channel) else { return };
        if log.fetching || log.loading_older || !log.more_before {
            return;
        }
        let Some(first) = log.messages.first().map(|m| m.id) else { return };
        log.loading_older = true;
        let api = self.api.clone();
        cx.spawn(async move |this, cx| {
            let got = core::run(async move { api.messages(channel, Some(first)).await }).await;
            let _ = this.update(cx, |s, cx| {
                let log = s.chats.entry(channel).or_default();
                log.loading_older = false;
                if let Ok(h) = got {
                    log.more_before = h.messages.len() >= PAGE;
                    let mut older = h.messages;
                    older.retain(|m| !log.messages.iter().any(|x| x.id == m.id));
                    older.append(&mut log.messages);
                    log.messages = older;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Runs an API call in the background and turns a failure into a toast.
    pub fn call<T: Send + 'static>(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(Api) -> futures::future::BoxFuture<'static, Result<T, ApiError>> + Send + 'static,
        done: impl FnOnce(&mut Session, T, &mut Context<Session>) + 'static,
    ) {
        let api = self.api.clone();
        cx.spawn(async move |this, cx| {
            let got = core::run(async move { f(api).await }).await;
            let _ = this.update(cx, |s, cx| match got {
                Ok(v) => done(s, v, cx),
                Err(e) => cx.emit(SessionEvent::Toast(e.message)),
            });
        })
        .detach();
    }

    /// Sends a request on the socket right away (so requests go out in the order they are made)
    /// and returns the reply to await.
    pub fn request(
        rt: Option<Realtime>,
        kind: &str,
        payload: Value,
    ) -> impl Future<Output = Result<Value, realtime::RequestError>> + use<> {
        let reply = rt.map(|rt| rt.request(kind, payload));
        async move {
            match reply {
                Some(reply) => core::run(reply).await,
                None => Err(realtime::RequestError { code: ErrorCode::Offline, reply: Value::Null }),
            }
        }
    }
}

/// How long to wait before loading a picture again after `tries` failures: 2 s, doubling, to a
/// minute.
fn retry_after(tries: u32) -> Duration {
    Duration::from_secs((2u64 << tries.min(5)).min(60))
}

/// The image format for a content type, falling back to sniffing the bytes.
pub fn image_format(content_type: &str, bytes: &[u8]) -> Option<image::ImageFormat> {
    use image::ImageFormat;
    Some(match content_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::WebP,
        "image/bmp" => ImageFormat::Bmp,
        _ => match bytes {
            [0x89, b'P', b'N', b'G', ..] => ImageFormat::Png,
            [0xff, 0xd8, ..] => ImageFormat::Jpeg,
            [b'G', b'I', b'F', ..] => ImageFormat::Gif,
            [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => ImageFormat::WebP,
            [b'B', b'M', ..] => ImageFormat::Bmp,
            _ => return None,
        },
    })
}

/// What a picture may cost to decode at all: a bigger one fails rather than taking the memory.
fn decode_limits() -> image::Limits {
    let mut l = image::Limits::default();
    l.max_image_width = Some(16_384);
    l.max_image_height = Some(16_384);
    l.max_alloc = Some(512 << 20);
    l
}

/// The size `w` × `h` scaled down (never up) to fit in `fit`, keeping its shape.
pub fn fit_within(w: u32, h: u32, fit: (u32, u32)) -> (u32, u32) {
    if w <= fit.0 && h <= fit.1 {
        return (w, h);
    }
    let scale = (fit.0 as f64 / w as f64).min(fit.1 as f64 / h as f64);
    (((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1))
}

/// Decodes a picture no bigger than `fit`, as the BGRA frames GPUI draws, and what it costs in
/// bytes. Animated GIFs keep their frames, as many as fit in 64 MB. Slow: run it off the UI thread.
pub fn decode_picture(bytes: &[u8], content_type: &str, fit: (u32, u32)) -> anyhow::Result<(Arc<RenderImage>, usize)> {
    use image::{AnimationDecoder, Frame, ImageDecoder, RgbaImage};
    use std::io::Cursor;
    const MAX_FRAMES_BYTES: usize = 64 << 20;
    let format = image_format(content_type, bytes).ok_or_else(|| anyhow::anyhow!("not a picture"))?;
    let bgra = |mut img: RgbaImage| {
        for px in img.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        img
    };
    let mut frames = Vec::new();
    let mut cost = 0;
    if format == image::ImageFormat::Gif {
        let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))?;
        decoder.set_limits(decode_limits())?;
        for frame in decoder.into_frames() {
            let frame = frame?;
            let delay = frame.delay();
            let buf = frame.into_buffer();
            let (w, h) = fit_within(buf.width(), buf.height(), fit);
            let buf = if (w, h) == buf.dimensions() { buf } else { image::imageops::thumbnail(&buf, w, h) };
            cost += buf.len();
            frames.push(Frame::from_parts(bgra(buf), 0, 0, delay));
            if cost >= MAX_FRAMES_BYTES {
                break;
            }
        }
        anyhow::ensure!(!frames.is_empty(), "a GIF without frames");
    } else {
        let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
        reader.limits(decode_limits());
        let img = reader.decode()?;
        let img = if img.width() > fit.0 || img.height() > fit.1 { img.thumbnail(fit.0, fit.1) } else { img };
        let buf = img.into_rgba8();
        cost = buf.len();
        frames.push(Frame::new(bgra(buf)));
    }
    Ok((Arc::new(RenderImage::new(frames)), cost))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: MessageId, body: &str) -> Message {
        serde_json::from_value(serde_json::json!({ "id": id, "channelId": 1, "userId": 1, "body": body })).unwrap()
    }

    fn page(ids: impl IntoIterator<Item = MessageId>) -> History {
        History { messages: ids.into_iter().map(|i| msg(i, "")).collect(), pinned: Vec::new() }
    }

    fn ids(log: &ChatLog) -> Vec<MessageId> {
        log.messages.iter().map(|m| m.id).collect()
    }

    #[test]
    fn pushes_during_the_first_load_are_kept() {
        let mut log = ChatLog { fetching: true, ..Default::default() };
        log.push(LogOp::Added(msg(4, "new")));
        log.push(LogOp::Added(msg(3, "")));
        log.push(LogOp::Updated(msg(2, "edited")));
        log.land(page([1, 2, 3]));
        assert_eq!(ids(&log), [1, 2, 3, 4]);
        assert_eq!(log.messages[1].body, "edited");
        assert!(log.loaded && !log.more_before && log.pending.is_empty());
    }

    #[test]
    fn catching_up_merges_the_latest_page_by_id() {
        let mut log = ChatLog::default();
        log.land(page(1..=50));
        log.apply(LogOp::Added(msg(51, "")));
        log.fetching = true;
        log.push(LogOp::Added(msg(61, "pushed while fetching")));
        // While away: 10 and 51 were deleted, 52..=60 arrived.
        log.land(page((9..=60).filter(|i| *i != 10 && *i != 51)));
        let mut want: Vec<MessageId> = (1..=9).collect();
        want.extend((11..=50).chain(52..=61));
        assert_eq!(ids(&log), want);
        assert!(log.more_before);
    }

    #[test]
    fn a_gap_too_big_for_one_page_starts_over() {
        let mut log = ChatLog::default();
        log.land(page([1, 2]));
        log.fetching = true;
        log.land(page(100..150));
        assert_eq!(ids(&log), (100..150).collect::<Vec<_>>());
        assert!(log.more_before);
    }

    #[test]
    fn a_failed_catch_up_still_applies_what_came_in() {
        let mut log = ChatLog::default();
        log.land(page([1]));
        log.fetching = true;
        log.push(LogOp::Deleted(1));
        log.fetch_failed("offline".into());
        assert!(log.messages.is_empty() && log.error.is_none() && !log.fetching);
    }

    #[test]
    fn retries_back_off_to_a_minute() {
        assert_eq!(retry_after(0), Duration::from_secs(2));
        assert_eq!(retry_after(2), Duration::from_secs(8));
        assert_eq!(retry_after(9), Duration::from_secs(60));
    }

    #[test]
    fn pictures_shrink_to_fit_and_keep_their_shape() {
        assert_eq!(fit_within(400, 200, THUMBNAIL), (400, 200));
        assert_eq!(fit_within(11_000, 11_000, THUMBNAIL), (600, 600));
        assert_eq!(fit_within(8_400, 1_000, THUMBNAIL), (840, 100));
        assert_eq!(fit_within(10_000, 1, (100, 100)), (100, 1));
    }

    #[test]
    fn a_big_picture_decodes_small_and_a_huge_one_is_refused() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(2000, 1000, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let (img, cost) = decode_picture(&png, "image/png", THUMBNAIL).unwrap();
        assert_eq!((img.size(0).width.0, img.size(0).height.0), (840, 420));
        assert_eq!(cost, 840 * 420 * 4);
        // Blue first: GPUI wants BGRA.
        assert_eq!(&img.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
        assert!(decode_picture(b"not a picture", "text/plain", THUMBNAIL).is_err());

        // Wider than any picture is allowed to be: refused instead of decoded.
        let mut wide = Vec::new();
        image::GrayImage::new(16_385, 1).write_to(&mut std::io::Cursor::new(&mut wide), image::ImageFormat::Png).unwrap();
        let err = decode_picture(&wide, "image/png", THUMBNAIL).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("limit"), "{err}");
    }
}
