//! The session's side of private conversations: keeping the key state and the conversation list
//! current, opening what arrives, sealing what goes out, and following private calls. The
//! cryptography itself is in `core::crypto`, the bookkeeping of keys in `dm::Vault`.

use super::{LogOp, Session, SessionEvent};
use crate::core::api::{Api, ApiError, Sealed};
use crate::core::types::*;
use crate::core::{self, crypto};
use crate::dm::{KeyState, Next, Payload, SealError, Vault};
use gpui::{Context, Task};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// One private conversation as the sidebar shows it.
#[derive(Clone, Debug)]
pub struct Dm {
    pub info: Conversation,
    pub peer: UserId,
    /// The last message, opened, as one line.
    pub preview: String,
    pub preview_mine: bool,
}

/// A file about to be sent in a private conversation, before it is sealed.
pub struct Outgoing {
    pub bytes: Arc<Vec<u8>>,
    pub name: String,
    pub mime: String,
}

fn seal_failed(e: SealError) -> ApiError {
    let code = e.code();
    ApiError { status: 0, code, message: crate::core::api::friendly(code, &Value::Null).unwrap_or_default(), body: Value::Null }
}

fn gone() -> ApiError {
    ApiError { status: 0, code: ErrorCode::Offline, message: String::new(), body: Value::Null }
}

impl Session {
    // Keys.

    /// Asks the server what keys there are, makes this account's first one if it has none, then
    /// loads the conversations. Again after every reconnect.
    pub(super) fn start_private(&mut self, cx: &mut Context<Self>) {
        let api = self.api.clone();
        cx.spawn(async move |this, cx| {
            let got = core::run(async move { api.keys().await }).await;
            let _ = this.update(cx, |s, cx| {
                match got {
                    Ok(reply) => {
                        if s.vault.on_keys(reply) == Next::Create {
                            s.create_key(cx);
                        }
                    }
                    Err(e) if e.status == 404 => s.vault.state = KeyState::Unsupported,
                    Err(e) if !s.vault.ready() => s.vault.state = KeyState::Failed(e.message),
                    Err(_) => {}
                }
                s.load_dms(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Makes and publishes a new identity key for this account. The recovery key is kept here,
    /// sealed, until the person says they have put it somewhere safe.
    fn create_key(&mut self, cx: &mut Context<Self>) {
        let (identity, recovery, wrapped) = Vault::create();
        let (api, public) = (self.api.clone(), identity.public_b64());
        self.vault.state = KeyState::Loading;
        cx.spawn(async move |this, cx| {
            let got = core::run(async move { api.publish_key(&public, &wrapped).await }).await;
            let _ = this.update(cx, |s, cx| {
                match got {
                    Ok(key) => {
                        s.vault.adopt(key, identity, &recovery, false);
                        // Whatever was sealed to the old key shows as unreadable now; draw it again.
                        s.chats.retain(|room, _| matches!(room, Room::Channel(_)));
                        s.load_dms(cx);
                    }
                    Err(e) => s.vault.state = KeyState::Failed(e.message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The recovery key typed on a computer that does not have this account's key yet. True if it
    /// opened it.
    pub fn unlock(&mut self, typed: &str, cx: &mut Context<Self>) -> bool {
        if !self.vault.unlock(typed) {
            return false;
        }
        self.chats.retain(|room, _| matches!(room, Room::Channel(_)));
        self.load_dms(cx);
        cx.notify();
        true
    }

    /// Starts over with a new key, for someone who lost their recovery key and every computer
    /// that had theirs. What was sealed to the old key stays unreadable for them.
    pub fn reset_keys(&mut self, cx: &mut Context<Self>) {
        self.create_key(cx);
    }

    /// A new recovery key for this account's key, replacing the old one on the server.
    pub fn new_recovery(&mut self, cx: &mut Context<Self>) -> Task<Result<String, ApiError>> {
        let Some((id, recovery, wrapped)) = self.vault.new_recovery() else { return Task::ready(Err(seal_failed(SealError::NoKey))) };
        let api = self.api.clone();
        cx.spawn(async move |this, cx| {
            core::run(async move { api.rewrap_key(id, &wrapped).await }).await?;
            let shown = recovery.display();
            this.update(cx, |s, cx| {
                s.vault.set_recovery(&recovery, false);
                cx.notify();
            })
            .map_err(|_| gone())?;
            Ok(shown)
        })
    }

    pub fn mark_backed_up(&mut self, cx: &mut Context<Self>) {
        self.vault.mark_backed_up();
        cx.notify();
    }

    // Conversations.

    fn load_dms(&mut self, cx: &mut Context<Self>) {
        let api = self.api.clone();
        cx.spawn(async move |this, cx| {
            let got = core::run(async move { api.dms().await }).await;
            let _ = this.update(cx, |s, cx| {
                let Ok(list) = got else { return };
                s.vault.learn(list.keys);
                s.blocked = list.blocked.into_iter().collect();
                let me = s.me.id;
                let mut dms = HashMap::new();
                for info in list.conversations {
                    let peer = info.peer(me);
                    let mut dm = Dm { peer, preview: String::new(), preview_mine: false, info };
                    if let Some(last) = dm.info.last.clone() {
                        s.preview(&mut dm, &last);
                    }
                    dms.insert(dm.info.id, dm);
                }
                // A conversation opened here but not written in yet is not on the server's list.
                for (id, dm) in std::mem::take(&mut s.dms) {
                    dms.entry(id).or_insert(dm);
                }
                s.dms = dms;
                s.dms_loaded = true;
                cx.notify();
            });
        })
        .detach();
    }

    fn preview(&mut self, dm: &mut Dm, last: &SealedMessage) {
        let m = self.vault.open_message(last, dm.peer);
        dm.preview_mine = last.user_id == self.me.id;
        dm.preview = preview_of(&m, &self.display_name(Some(last.user_id), None), dm.preview_mine);
    }

    /// Conversations, most recent first.
    pub fn dm_list(&self) -> Vec<&Dm> {
        let mut list: Vec<&Dm> = self.dms.values().collect();
        list.sort_by_key(|d| std::cmp::Reverse((d.info.last_at, d.info.id)));
        list
    }

    pub fn dm_with(&self, user: UserId) -> Option<ConversationId> {
        self.dms.values().find(|d| d.peer == user).map(|d| d.info.id)
    }

    pub fn dm_unread(&self) -> u32 {
        self.dms.values().map(|d| d.info.unread).sum()
    }

    pub fn peer_of(&self, conversation: ConversationId) -> Option<UserId> {
        self.dms.get(&conversation).map(|d| d.peer)
    }

    /// The conversation with `user`, opened on the server if there is none yet.
    pub fn open_dm_with(&mut self, user: UserId, cx: &mut Context<Self>) -> Task<Result<ConversationId, ApiError>> {
        if let Some(id) = self.dm_with(user) {
            return Task::ready(Ok(id));
        }
        let api = self.api.clone();
        cx.spawn(async move |this, cx| {
            let info = core::run(async move { api.open_dm(user).await }).await?;
            this.update(cx, |s, cx| {
                let id = info.id;
                let peer = info.peer(s.me.id);
                s.dms.entry(id).or_insert(Dm { info, peer, preview: String::new(), preview_mine: false });
                s.vault.pin(peer);
                cx.notify();
                id
            })
            .map_err(|_| gone())
        })
    }

    /// History as the server pages it, opened.
    pub(super) fn open_history(&mut self, conversation: ConversationId, page: DmHistory) -> History {
        self.vault.learn(page.keys);
        let me = self.me.id;
        let peer = self.peer_of(conversation).or_else(|| page.messages.iter().map(|m| m.user_id).find(|u| *u != me)).unwrap_or(me);
        self.vault.pin(peer);
        History { messages: page.messages.iter().map(|m| self.vault.open_message(m, peer)).collect(), pinned: Vec::new() }
    }

    /// Moves your read mark to the newest message here, on the server too (so your other computers
    /// clear their badge).
    pub(super) fn mark_read(&mut self, conversation: ConversationId, cx: &mut Context<Self>) {
        let newest = self
            .chats
            .get(&Room::Dm(conversation))
            .and_then(|l| l.messages.last().map(|m| m.id))
            .into_iter()
            .chain(self.dms.get(&conversation).and_then(|d| d.info.last.as_ref().map(|m| m.id)))
            .max();
        let Some(dm) = self.dms.get_mut(&conversation) else { return };
        dm.info.unread = 0;
        let Some(newest) = newest.filter(|n| *n > dm.info.last_read_id) else { return };
        dm.info.last_read_id = newest;
        let api = self.api.clone();
        cx.background_executor()
            .spawn(core::run(async move {
                let _ = api.read_dm(conversation, newest).await;
            }))
            .detach();
    }

    /// Recounts what is unread after a read mark moved elsewhere.
    fn recount(&mut self, conversation: ConversationId) {
        let me = self.me.id;
        let log = self.chats.get(&Room::Dm(conversation)).filter(|l| l.loaded).map(|l| l.messages.clone());
        let Some(dm) = self.dms.get_mut(&conversation) else { return };
        let read = dm.info.last_read_id;
        dm.info.unread = match log {
            Some(messages) => messages
                .iter()
                .filter(|m| m.id > read && m.user_id != Some(me))
                .filter(|m| !m.private.as_ref().and_then(|p| p.call.as_ref()).is_some_and(|c| c.outcome == "ended"))
                .count() as u32,
            None if dm.info.last.as_ref().is_none_or(|m| m.id <= read) => 0,
            None => dm.info.unread,
        };
    }

    // Writing.

    /// Seals with the current keys and sends; if a key changed meanwhile, learns the new one from
    /// the refusal and seals again, once.
    fn send_sealed<T: Send + 'static>(
        &mut self,
        seal: impl Fn(&mut Vault) -> Result<Sealed, SealError> + 'static,
        send: impl Fn(Api, Sealed) -> futures::future::BoxFuture<'static, Result<T, ApiError>> + Send + Sync + 'static,
        cx: &mut Context<Self>,
    ) -> Task<Result<T, ApiError>> {
        let send = Arc::new(send);
        cx.spawn(async move |this, cx| {
            for attempt in 0..2 {
                let (sealed, api) = this.update(cx, |s, _| (seal(&mut s.vault), s.api.clone())).map_err(|_| gone())?;
                let sealed = sealed.map_err(seal_failed)?;
                let send = send.clone();
                match core::run(async move { send(api, sealed).await }).await {
                    Err(e) if e.code == ErrorCode::StaleKey && attempt == 0 => {
                        let keys = e.body.get("keys").cloned().and_then(list_of::<PublicKeyInfo>).unwrap_or_default();
                        let _ = this.update(cx, |s, _| keys.into_iter().for_each(|k| s.vault.key_changed(k)));
                    }
                    got => return got,
                }
            }
            Err(seal_failed(SealError::PeerHasNoKey))
        })
    }

    /// Sends a message, sealing its file first if it has one. It shows when the server echoes it
    /// back, exactly as in a channel.
    pub fn send_dm(&mut self, conversation: ConversationId, body: String, file: Option<Outgoing>, cx: &mut Context<Self>) -> Task<Result<(), ApiError>> {
        let Some(peer) = self.peer_of(conversation) else { return Task::ready(Err(gone())) };
        let (api, cache) = (self.api.clone(), self.cache.clone());
        cx.spawn(async move |this, cx| {
            let mut attachment = None;
            if let Some(f) = file {
                let bytes = f.bytes.clone();
                let (sealed, key) = cx.background_executor().spawn(async move { crypto::seal_file(&bytes) }).await;
                let api = api.clone();
                let up = core::run(async move {
                    // Kept as it is uploaded, so your own picture draws without a round trip.
                    cache.insert(&sealed, "application/octet-stream");
                    api.upload_sealed(conversation, sealed).await
                })
                .await?;
                let file = SealedFile { key: crypto::b64(key.as_slice()), name: f.name, mime: f.mime, size: f.bytes.len() as u64 };
                attachment = Some((up.hash, file));
            }
            let hash = attachment.as_ref().map(|(h, _)| h.clone());
            let payload = Payload { body, file: attachment.map(|(_, f)| f) };
            let task = this
                .update(cx, |s, cx| {
                    s.send_sealed(
                        move |v| v.seal_message(conversation, peer, &payload),
                        move |api, sealed| {
                            let hash = hash.clone();
                            Box::pin(async move { api.send_dm(conversation, &sealed, hash.as_deref()).await.map(|_| ()) })
                        },
                        cx,
                    )
                })
                .map_err(|_| gone())?;
            task.await
        })
    }

    /// Replaces what one of your messages says, keeping its file.
    pub fn edit_dm(&mut self, message: &Message, body: String, cx: &mut Context<Self>) -> Task<Result<(), ApiError>> {
        let conversation = message.channel_id;
        let Some(peer) = self.peer_of(conversation) else { return Task::ready(Err(gone())) };
        let payload = Payload { body, file: message.private.as_ref().and_then(|p| p.file.clone()) };
        let id = message.id;
        self.send_sealed(
            move |v| v.seal_message(conversation, peer, &payload),
            move |api, sealed| Box::pin(async move { api.edit_dm(id, &sealed).await.map(|_| ()) }),
            cx,
        )
    }

    /// Adds your `emoji` to a message, or takes it off if it is there.
    pub fn react_dm(&mut self, message: &Message, emoji: String, cx: &mut Context<Self>) -> Task<Result<(), ApiError>> {
        let conversation = message.channel_id;
        let Some(peer) = self.peer_of(conversation) else { return Task::ready(Err(gone())) };
        let me = self.me.id;
        let mut mine: Vec<String> = message.reactions.iter().filter(|r| r.user_ids.contains(&me)).map(|r| r.emoji.clone()).collect();
        match mine.iter().position(|e| *e == emoji) {
            Some(i) => {
                mine.remove(i);
            }
            None => mine.push(emoji),
        }
        let id = message.id;
        if mine.is_empty() {
            let api = self.api.clone();
            return cx.spawn(async move |_, _| core::run(async move { api.react_dm(id, None).await.map(|_| ()) }).await);
        }
        self.send_sealed(
            move |v| v.seal_reactions(conversation, peer, id, &mine),
            move |api, sealed| Box::pin(async move { api.react_dm(id, Some(&sealed)).await.map(|_| ()) }),
            cx,
        )
    }

    pub fn set_blocked(&mut self, user: UserId, blocked: bool, cx: &mut Context<Self>) {
        self.call(
            cx,
            move |api| Box::pin(async move { api.set_blocked(user, blocked).await }),
            |s, list, cx| {
                s.blocked = list.into_iter().collect();
                cx.notify();
            },
        );
    }

    // Calls.

    /// A fresh media key for a call to `conversation`, and the same key sealed for the other person.
    pub fn new_call_key(&mut self, conversation: ConversationId) -> Result<([u8; 32], Sealed), ApiError> {
        let peer = self.peer_of(conversation).ok_or_else(gone)?;
        let key = crypto::random::<32>();
        let sealed = self.vault.seal_call_key(conversation, peer, &key).map_err(seal_failed)?;
        Ok((key, sealed))
    }

    /// The media key of a call you were rung for, or are in.
    pub fn call_key(&mut self, conversation: ConversationId) -> Option<[u8; 32]> {
        let call = self.calls.get(&conversation)?.clone();
        self.vault.open_call_key(&call)
    }

    /// After a reconnect: calls that ended meanwhile, and rings that started.
    pub(super) fn calls_reconciled(&mut self, before: HashMap<ConversationId, CallInfo>, cx: &mut Context<Self>) {
        for id in before.keys().filter(|id| !self.calls.contains_key(id)) {
            cx.emit(SessionEvent::CallChanged(*id));
        }
        let me = self.me.id;
        for (id, call) in &self.calls {
            if !before.contains_key(id) && call.state == CallPhase::Ringing && call.callee_id == me {
                cx.emit(SessionEvent::Ringing(*id));
            } else if before.get(id) != Some(call) {
                cx.emit(SessionEvent::CallChanged(*id));
            }
        }
    }

    // Pushes.

    /// Handles a push about private conversations and calls; false if it is about something else.
    pub(super) fn on_private_push(&mut self, kind: &str, v: &Value, cx: &mut Context<Self>) -> bool {
        let field = |key: &str| v.get(key).cloned().unwrap_or(Value::Null);
        let int = |key: &str| v.get(key).and_then(Value::as_i64).unwrap_or(-1);
        match kind {
            "keys:changed" => {
                if let Ok(key) = serde_json::from_value::<PublicKeyInfo>(field("key")) {
                    let mine = key.user_id == self.me.id;
                    self.vault.key_changed(key);
                    if mine && !self.vault.ready() {
                        // Made on another computer: ask for it, so the recovery key can open it here.
                        self.start_private(cx);
                    }
                }
            }
            "dm:message" => {
                let (Ok(m), Ok(info)) = (serde_json::from_value::<SealedMessage>(field("message")), serde_json::from_value::<Conversation>(field("conversation"))) else {
                    return true;
                };
                let unknown = self.vault.unknown(m.sender_key.into_iter().chain(m.recipient_key));
                if unknown.is_empty() {
                    self.dm_arrived(m, info, cx);
                } else {
                    // Sealed to a key not seen yet: fetch it, then open the message.
                    let api = self.api.clone();
                    cx.spawn(async move |this, cx| {
                        let keys = core::run(async move { api.lookup_keys(&unknown).await }).await.unwrap_or_default();
                        let _ = this.update(cx, |s, cx| {
                            s.vault.learn(keys);
                            s.dm_arrived(m, info, cx);
                        });
                    })
                    .detach();
                }
            }
            "dm:updated" => {
                if let Ok(m) = serde_json::from_value::<SealedMessage>(field("message"))
                    && let Some(peer) = self.peer_of(m.conversation_id)
                {
                    let opened = self.vault.open_message(&m, peer);
                    if let Some(dm) = self.dms.get_mut(&m.conversation_id)
                        && dm.info.last.as_ref().is_some_and(|l| l.id == m.id)
                    {
                        dm.info.last = Some(m.clone());
                        let mut dm = dm.clone();
                        self.preview(&mut dm, &m);
                        self.dms.insert(m.conversation_id, dm);
                    }
                    if let Some(log) = self.chats.get_mut(&Room::Dm(m.conversation_id)) {
                        log.push(LogOp::Updated(opened));
                    }
                }
            }
            "dm:deleted" => {
                if let Some(log) = self.chats.get_mut(&Room::Dm(int("conversationId"))) {
                    log.push(LogOp::Deleted(int("id")));
                }
            }
            "dm:reactions" => {
                let (id, conversation) = (int("id"), int("conversationId"));
                if let (Some(peer), Some(sealed)) = (self.peer_of(conversation), list_of::<SealedReaction>(field("reactions"))) {
                    let reactions = self.vault.open_reactions(conversation, peer, id, &sealed);
                    if let Some(log) = self.chats.get_mut(&Room::Dm(conversation)) {
                        log.push(LogOp::Reactions(id, reactions));
                    }
                }
            }
            "dm:read" => {
                let conversation = int("conversationId");
                if let Some(dm) = self.dms.get_mut(&conversation) {
                    dm.info.last_read_id = dm.info.last_read_id.max(int("lastReadId"));
                    self.recount(conversation);
                }
            }
            "dm:blocks" => {
                if let Some(list) = list_of::<UserId>(field("blocked")) {
                    self.blocked = list.into_iter().collect();
                }
            }
            "call:state" => {
                let Ok(call) = serde_json::from_value::<CallInfo>(field("call")) else { return true };
                let id = call.conversation_id;
                if call.state == CallPhase::Ended {
                    self.call_outcomes.insert(id, call.outcome.clone().unwrap_or_default());
                    self.calls.remove(&id);
                    self.call_rosters.remove(&id);
                } else {
                    let fresh = !self.calls.contains_key(&id);
                    let ringing_me = call.state == CallPhase::Ringing && call.callee_id == self.me.id;
                    self.calls.insert(id, call);
                    if fresh && ringing_me {
                        cx.emit(SessionEvent::Ringing(id));
                    }
                }
                cx.emit(SessionEvent::CallChanged(id));
            }
            "call:roster" => {
                let id = int("conversationId");
                if let Some(roster) = list_of::<Member>(field("roster")) {
                    if self.calls.contains_key(&id) || !roster.is_empty() {
                        self.call_rosters.insert(id, roster);
                    } else {
                        self.call_rosters.remove(&id);
                    }
                    cx.emit(SessionEvent::Roster(Place::Call(id)));
                }
            }
            _ => return false,
        }
        cx.notify();
        true
    }

    fn dm_arrived(&mut self, m: SealedMessage, info: Conversation, cx: &mut Context<Self>) {
        let me = self.me.id;
        let id = info.id;
        let peer = info.peer(me);
        let room = Room::Dm(id);
        let mine = m.user_id == me;
        let away = self.viewing != Some(room);
        let mut dm = self.dms.remove(&id).unwrap_or(Dm { peer, preview: String::new(), preview_mine: false, info: info.clone() });
        dm.info.last_at = info.last_at;
        dm.info.last = Some(m.clone());
        // A call both of you were on is not news to either; a missed one is.
        let answered = m.meta.as_ref().is_some_and(|c| c.outcome == "ended");
        if !mine && away && !answered {
            dm.info.unread += 1;
        }
        self.preview(&mut dm, &m);
        self.dms.insert(id, dm);
        self.vault.pin(peer);
        let opened = self.vault.open_message(&m, peer);
        if let Some(log) = self.chats.get_mut(&room) {
            log.push(LogOp::Added(opened.clone()));
        }
        if !mine && !away {
            self.mark_read(id, cx);
        }
        cx.emit(SessionEvent::Message(room, Box::new(opened)));
        cx.notify();
    }
}

/// One line for the conversation list.
pub fn preview_of(m: &Message, sender: &str, mine: bool) -> String {
    let p = m.private.clone().unwrap_or_default();
    let text = if let Some(call) = &p.call {
        crate::dm::call_line(call, sender, mine)
    } else if p.unreadable {
        tr!("Message protected with a key this computer doesn't have", "Mensagem protegida com uma chave que este computador não tem").into()
    } else if m.body.trim().is_empty() {
        p.file.map(|f| format!("📎 {}", f.name)).unwrap_or_default()
    } else {
        m.body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().chars().take(80).collect()
    };
    if mine && p.call.is_none() { trf!("You: {}", "Você: {}", text) } else { text }
}
