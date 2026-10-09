//! Private conversations on this computer: whose keys are whose, whether this computer can open
//! your messages, and turning sealed messages into ones the chat can draw (and back).
//!
//! Plain logic over `core::crypto` and `core::keystore`, with no networking and no UI, so the
//! session drives it and the tests can too.

use crate::core::api::Sealed;
use crate::core::crypto::{self, Identity, PairKey, RecoveryKey, aad};
use crate::core::keystore::KeyFile;
use crate::core::types::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::rc::Rc;

/// Whether this computer can take part in private conversations yet.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyState {
    /// Asking the server what keys there are.
    Loading,
    /// This computer has the account's key: everything works.
    Ready,
    /// The account has a key that this computer does not: the recovery key opens it here.
    Locked,
    /// The server could not be asked; tried again on the next connect.
    Failed(String),
    /// The server is older than private conversations.
    Unsupported,
}

/// What to do after the server said what keys there are.
#[derive(Debug, PartialEq)]
pub enum Next {
    Nothing,
    /// The account has no key at all yet: make one and publish it.
    Create,
}

/// What a sealed message says inside.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Payload {
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<SealedFile>,
}

/// Why something could not be sealed for the other person.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SealError {
    /// This computer can't seal anything yet: no key, or locked.
    NoKey,
    /// The other person has no key yet.
    PeerHasNoKey,
}

impl SealError {
    pub fn code(self) -> ErrorCode {
        match self {
            SealError::NoKey => ErrorCode::NoKey,
            SealError::PeerHasNoKey => ErrorCode::PeerHasNoKey,
        }
    }
}

pub struct Vault {
    pub state: KeyState,
    me: UserId,
    identity: Option<(KeyId, Identity)>,
    /// The account's key as the server has it, with its wrapped private half: what the recovery
    /// key opens on a computer that does not have it yet.
    on_server: Option<OwnKey>,
    file: KeyFile,
    /// Every public key seen, by id: history needs keys since replaced.
    keys: HashMap<KeyId, PublicKeyInfo>,
    /// Each person's current key.
    current: HashMap<UserId, KeyId>,
    /// Agreed keys, by (conversation, my key, their key): an X25519 agreement per message would be
    /// wasted work for the same pair every time.
    pairs: HashMap<(ConversationId, KeyId, KeyId), Rc<PairKey>>,
}

impl Vault {
    pub fn new(file: KeyFile, me: UserId) -> Vault {
        let identity = file.identity();
        Vault { state: KeyState::Loading, me, identity, on_server: None, file, keys: HashMap::new(), current: HashMap::new(), pairs: HashMap::new() }
    }

    pub fn ready(&self) -> bool {
        self.state == KeyState::Ready
    }

    pub fn my_key(&self) -> Option<KeyId> {
        self.identity.as_ref().map(|(id, _)| *id)
    }

    /// What the server said about everybody's keys and your own.
    pub fn on_keys(&mut self, reply: KeysReply) -> Next {
        self.learn(reply.keys.iter().cloned());
        for k in &reply.keys {
            self.current.insert(k.user_id, k.id);
        }
        self.on_server = reply.mine.clone();
        match reply.mine {
            None => {
                // The server has no key for this account (a new one, or a server that forgot):
                // whatever this computer kept belongs to nothing any more.
                if self.identity.is_some() {
                    self.identity = None;
                    self.file.forget_identity();
                }
                Next::Create
            }
            Some(mine) => {
                self.state = match &self.identity {
                    Some((id, identity)) if *id == mine.id && identity.public_b64() == mine.public_key => KeyState::Ready,
                    _ => KeyState::Locked,
                };
                Next::Nothing
            }
        }
    }

    /// Makes a new identity key and its recovery key: the wrapping to publish, and the key to show.
    pub fn create() -> (Identity, RecoveryKey, String) {
        let identity = Identity::generate();
        let recovery = RecoveryKey::generate();
        let wrapped = crypto::wrap(&identity, &recovery);
        (identity, recovery, wrapped)
    }

    /// The server took a key this computer made: it is the account's key from now on.
    pub fn adopt(&mut self, key: OwnKey, identity: Identity, recovery: &RecoveryKey, saved: bool) {
        self.file.set_identity(key.id, &identity);
        self.file.set_recovery(recovery, saved);
        self.learn([PublicKeyInfo { id: key.id, user_id: self.me, public_key: key.public_key.clone(), created_at: 0 }]);
        self.current.insert(self.me, key.id);
        self.identity = Some((key.id, identity));
        self.on_server = Some(key);
        self.state = KeyState::Ready;
    }

    /// Opens the account's key with the recovery key typed in. False if it is not the right one.
    pub fn unlock(&mut self, typed: &str) -> bool {
        let (Some(recovery), Some(mine)) = (RecoveryKey::parse(typed), self.on_server.clone()) else { return false };
        let Some(public) = crypto::public_key(&mine.public_key) else { return false };
        let Some(identity) = crypto::unwrap(&mine.wrapped, &public, &recovery) else { return false };
        self.adopt(mine, identity, &recovery, true);
        true
    }

    /// The recovery key, as shown, if this computer made or was given it.
    pub fn recovery(&self) -> Option<String> {
        self.file.recovery().map(|r| r.display())
    }

    /// A recovery key was made here and nobody has said it is kept somewhere yet.
    pub fn needs_backup(&self) -> bool {
        self.ready() && !self.file.recovery_saved()
    }

    pub fn mark_backed_up(&mut self) {
        self.file.mark_recovery_saved();
    }

    /// A new recovery key for the key this computer has, and its wrapping to send to the server.
    pub fn new_recovery(&self) -> Option<(KeyId, RecoveryKey, String)> {
        let (id, identity) = self.identity.as_ref()?;
        let recovery = RecoveryKey::generate();
        let wrapped = crypto::wrap(identity, &recovery);
        Some((*id, recovery, wrapped))
    }

    pub fn set_recovery(&mut self, recovery: &RecoveryKey, saved: bool) {
        self.file.set_recovery(recovery, saved);
    }

    // The key directory.

    /// Public keys from anywhere the server sends them.
    pub fn learn(&mut self, keys: impl IntoIterator<Item = PublicKeyInfo>) {
        for k in keys {
            self.keys.insert(k.id, k);
        }
    }

    /// Somebody published a new key (a `keys:changed` push).
    pub fn key_changed(&mut self, key: PublicKeyInfo) {
        let (user, id) = (key.user_id, key.id);
        self.learn([key]);
        if self.current.get(&user).is_none_or(|c| *c < id) {
            self.current.insert(user, id);
        }
        // Your own account has a new key, made on another computer: this one's is spent.
        if user == self.me && self.my_key() != Some(id) {
            self.state = KeyState::Locked;
        }
    }

    /// The key ids among these that are not known yet.
    pub fn unknown(&self, ids: impl IntoIterator<Item = KeyId>) -> Vec<KeyId> {
        let mut out: Vec<KeyId> = ids.into_iter().filter(|id| !self.keys.contains_key(id)).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub fn peer_has_key(&self, peer: UserId) -> bool {
        self.current.contains_key(&peer)
    }

    fn public_of(&self, key: KeyId) -> Option<[u8; 32]> {
        crypto::public_key(&self.keys.get(&key)?.public_key)
    }

    fn pair(&mut self, conversation: ConversationId, mine: KeyId, peer: UserId, theirs: KeyId) -> Option<Rc<PairKey>> {
        if let Some(k) = self.pairs.get(&(conversation, mine, theirs)) {
            return Some(k.clone());
        }
        let (id, identity) = self.identity.as_ref().filter(|(id, _)| *id == mine)?;
        let _ = id;
        let public = self.public_of(theirs)?;
        let key = Rc::new(crypto::pair_key(identity, self.me, &public, peer, conversation)?);
        self.pairs.insert((conversation, mine, theirs), key.clone());
        Some(key)
    }

    /// Seals something for the other person, to both your current keys.
    pub fn seal(&mut self, conversation: ConversationId, peer: UserId, aad: &str, plain: &[u8]) -> Result<Sealed, SealError> {
        let mine = self.my_key().filter(|_| self.ready()).ok_or(SealError::NoKey)?;
        let theirs = *self.current.get(&peer).ok_or(SealError::PeerHasNoKey)?;
        let key = self.pair(conversation, mine, peer, theirs).ok_or(SealError::PeerHasNoKey)?;
        Ok(Sealed { text: key.seal(aad, plain), sender_key: mine, recipient_key: theirs })
    }

    /// Opens something sealed between you and `peer`; `from_me` says which of the two keys is yours.
    fn open(&mut self, conversation: ConversationId, peer: UserId, (sender_key, recipient_key): (KeyId, KeyId), from_me: bool, aad: &str, sealed: &str) -> Option<Vec<u8>> {
        let (mine, theirs) = if from_me { (sender_key, recipient_key) } else { (recipient_key, sender_key) };
        self.pair(conversation, mine, peer, theirs)?.open(aad, sealed)
    }

    pub fn seal_message(&mut self, conversation: ConversationId, peer: UserId, payload: &Payload) -> Result<Sealed, SealError> {
        let json = crypto::pad(serde_json::to_vec(payload).unwrap_or_default());
        self.seal(conversation, peer, &aad::message(conversation, self.me), &json)
    }

    pub fn seal_reactions(&mut self, conversation: ConversationId, peer: UserId, message: MessageId, emoji: &[String]) -> Result<Sealed, SealError> {
        let json = crypto::pad(serde_json::to_vec(emoji).unwrap_or_default());
        self.seal(conversation, peer, &aad::reaction(conversation, message, self.me), &json)
    }

    /// A call's media key, sealed by the caller.
    pub fn seal_call_key(&mut self, conversation: ConversationId, peer: UserId, key: &[u8; 32]) -> Result<Sealed, SealError> {
        self.seal(conversation, peer, &aad::call(conversation, self.me), key)
    }

    pub fn open_call_key(&mut self, call: &CallInfo) -> Option<[u8; 32]> {
        let from_me = call.caller_id == self.me;
        let peer = if from_me { call.callee_id } else { call.caller_id };
        let plain = self.open(call.conversation_id, peer, (call.sender_key, call.recipient_key), from_me, &aad::call(call.conversation_id, call.caller_id), &call.sealed_key)?;
        plain.try_into().ok()
    }

    /// A sealed message as the chat draws it. One that doesn't open here still shows, saying so.
    pub fn open_message(&mut self, m: &SealedMessage, peer: UserId) -> Message {
        let from_me = m.user_id == self.me;
        let mut private = Private::default();
        let mut body = String::new();
        if m.kind == "call" {
            private.call = m.meta.clone();
        } else {
            let payload = match (m.sender_key, m.recipient_key, &m.sealed) {
                (Some(s), Some(r), Some(sealed)) => self
                    .open(m.conversation_id, peer, (s, r), from_me, &aad::message(m.conversation_id, m.user_id), sealed)
                    .and_then(|plain| serde_json::from_slice::<Payload>(&plain).ok()),
                _ => None,
            };
            match payload {
                Some(p) => {
                    body = p.body;
                    private.file = p.file;
                }
                None => private.unreadable = true,
            }
        }
        let media_type = private.file.as_ref().map(|f| media_type_of(&f.mime));
        Message {
            id: m.id,
            channel_id: m.conversation_id,
            user_id: Some(m.user_id),
            nickname: None,
            body,
            attachment_hash: m.attachment_hash.clone().filter(|_| private.file.is_some()),
            attachment_name: private.file.as_ref().map(|f| f.name.clone()),
            media_type,
            pinned: false,
            edited_at: m.edited_at,
            reactions: self.open_reactions(m.conversation_id, peer, m.id, &m.reactions),
            mentions: Vec::new(),
            mentions_everyone: false,
            created_at: m.created_at,
            private: Some(private),
        }
    }

    /// Everybody's sealed reactions to a message, opened and counted as a channel's are.
    pub fn open_reactions(&mut self, conversation: ConversationId, peer: UserId, message: MessageId, sealed: &[SealedReaction]) -> Vec<Reaction> {
        let mut out: Vec<Reaction> = Vec::new();
        for r in sealed {
            let from_me = r.user_id == self.me;
            let Some(plain) = self.open(conversation, peer, (r.sender_key, r.recipient_key), from_me, &aad::reaction(conversation, message, r.user_id), &r.sealed) else {
                continue;
            };
            let Ok(emoji) = serde_json::from_slice::<Vec<String>>(&plain) else { continue };
            for e in emoji.into_iter().take(20) {
                match out.iter_mut().find(|x| x.emoji == e) {
                    Some(x) if !x.user_ids.contains(&r.user_id) => {
                        x.count += 1;
                        x.user_ids.push(r.user_id);
                    }
                    Some(_) => {}
                    None => out.push(Reaction { emoji: e, count: 1, user_ids: vec![r.user_id] }),
                }
            }
        }
        out
    }

    // Trust.

    /// The safety number between you and `peer`, with your current keys.
    pub fn safety_number(&self, peer: UserId) -> Option<String> {
        let (_, mine) = self.identity.as_ref()?;
        let theirs = self.public_of(*self.current.get(&peer)?)?;
        Some(crypto::safety_number((self.me, &mine.public()), (peer, &theirs)))
    }

    fn current_public(&self, peer: UserId) -> Option<&str> {
        Some(self.keys.get(self.current.get(&peer)?)?.public_key.as_str())
    }

    /// Remembers `peer`'s key the first time it is seen, so a later change can be noticed.
    pub fn pin(&mut self, peer: UserId) {
        if self.file.pin(peer).is_none()
            && let Some(k) = self.current_public(peer).map(str::to_string)
        {
            self.file.set_pin(peer, &k);
        }
    }

    /// `peer`'s key is not the one first seen for them. Usually a reinstall without the recovery
    /// key; it is also what somebody in the middle would look like, so it is said, not hidden.
    pub fn key_changed_for(&self, peer: UserId) -> bool {
        match (self.file.pin(peer), self.current_public(peer)) {
            (Some(pinned), Some(now)) => pinned != now,
            _ => false,
        }
    }

    /// The change was seen and accepted: this key is the one to compare against from now on.
    pub fn accept_key(&mut self, peer: UserId) {
        if let Some(k) = self.current_public(peer).map(str::to_string) {
            self.file.set_pin(peer, &k);
            if self.file.verified(peer).is_some_and(|v| v != k) {
                self.file.set_verified(peer, None);
            }
        }
    }

    pub fn verified(&self, peer: UserId) -> bool {
        match (self.file.verified(peer), self.current_public(peer)) {
            (Some(v), Some(now)) => v == now,
            _ => false,
        }
    }

    pub fn set_verified(&mut self, peer: UserId, on: bool) {
        let key = if on { self.current_public(peer).map(str::to_string) } else { None };
        self.file.set_verified(peer, key.as_deref());
        if let Some(k) = key {
            self.file.set_pin(peer, &k);
        }
    }
}

/// What a sealed attachment is drawn as, from the type it was sealed with.
pub fn media_type_of(mime: &str) -> MediaType {
    match mime.split('/').next().unwrap_or("") {
        "image" => MediaType::Image,
        "video" => MediaType::Video,
        "audio" => MediaType::Audio,
        _ => MediaType::File,
    }
}

/// A line about a call, as it reads in the conversation.
pub fn call_line(meta: &CallMeta, caller: &str, from_me: bool) -> String {
    match meta.outcome.as_str() {
        "ended" => {
            let secs = meta.duration_ms.unwrap_or(0) / 1000;
            let length = if secs < 60 {
                trf!("{} s", "{} s", secs.max(1))
            } else if secs < 3600 {
                trf!("{} min", "{} min", secs / 60)
            } else {
                trf!("{} h {} min", "{} h {} min", secs / 3600, (secs % 3600) / 60)
            };
            trf!("Call · {}", "Chamada · {}", length)
        }
        "declined" if from_me => tr!("Your call was declined", "Sua chamada foi recusada").into(),
        "declined" => trf!("You declined a call from {}", "Você recusou uma chamada de {}", caller),
        _ if from_me => tr!("Call not answered", "Chamada não atendida").into(),
        _ => trf!("Missed call from {}", "Chamada perdida de {}", caller),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file() -> KeyFile {
        KeyFile::open_at(std::env::temp_dir().join(format!("harmony-vault-{}.json", hex::encode(crypto::random::<8>()))))
    }

    /// Two people with keys published, and the directory each sees.
    fn pair_up() -> (Vault, Vault) {
        let mut a = Vault::new(temp_file(), 1);
        let mut b = Vault::new(temp_file(), 2);
        assert_eq!(a.on_keys(KeysReply::default()), Next::Create);
        assert_eq!(b.on_keys(KeysReply::default()), Next::Create);
        let (ia, ra, wa) = Vault::create();
        let (ib, rb, wb) = Vault::create();
        let ka = OwnKey { id: 10, public_key: ia.public_b64(), wrapped: wa };
        let kb = OwnKey { id: 20, public_key: ib.public_b64(), wrapped: wb };
        let pa = PublicKeyInfo { id: 10, user_id: 1, public_key: ka.public_key.clone(), created_at: 0 };
        let pb = PublicKeyInfo { id: 20, user_id: 2, public_key: kb.public_key.clone(), created_at: 0 };
        a.adopt(ka, ia, &ra, false);
        b.adopt(kb, ib, &rb, false);
        a.key_changed(pb.clone());
        b.key_changed(pa.clone());
        (a, b)
    }

    fn as_sent(sealed: Sealed, id: MessageId, from: UserId) -> SealedMessage {
        SealedMessage {
            id,
            conversation_id: 5,
            user_id: from,
            kind: "text".into(),
            sender_key: Some(sealed.sender_key),
            recipient_key: Some(sealed.recipient_key),
            sealed: Some(sealed.text),
            meta: None,
            attachment_hash: None,
            reactions: Vec::new(),
            created_at: 1,
            edited_at: None,
        }
    }

    #[test]
    fn a_message_opens_on_both_sides() {
        let (mut a, mut b) = pair_up();
        let sealed = a.seal_message(5, 2, &Payload { body: "oi, tudo bem?".into(), file: None }).unwrap();
        let m = as_sent(sealed, 1, 1);
        assert_eq!(b.open_message(&m, 1).body, "oi, tudo bem?");
        assert_eq!(a.open_message(&m, 2).body, "oi, tudo bem?", "your own message opens too, on any of your computers");
    }

    #[test]
    fn a_message_moved_to_another_conversation_or_sender_does_not_open() {
        let (mut a, mut b) = pair_up();
        let sealed = a.seal_message(5, 2, &Payload { body: "x".into(), file: None }).unwrap();
        let mut m = as_sent(sealed, 1, 1);
        m.user_id = 2;
        let opened = b.open_message(&m, 1);
        assert!(opened.private.unwrap().unreadable);
        m.user_id = 1;
        m.conversation_id = 6;
        assert!(b.open_message(&m, 1).private.unwrap().unreadable);
    }

    #[test]
    fn reactions_count_up_across_both_people() {
        let (mut a, mut b) = pair_up();
        let ra = a.seal_reactions(5, 2, 9, &["👍".into(), "🔥".into()]).unwrap();
        let rb = b.seal_reactions(5, 1, 9, &["👍".into()]).unwrap();
        let sealed = [
            SealedReaction { user_id: 1, sender_key: ra.sender_key, recipient_key: ra.recipient_key, sealed: ra.text },
            SealedReaction { user_id: 2, sender_key: rb.sender_key, recipient_key: rb.recipient_key, sealed: rb.text },
        ];
        let counted = b.open_reactions(5, 1, 9, &sealed);
        assert_eq!(counted[0], Reaction { emoji: "👍".into(), count: 2, user_ids: vec![1, 2] });
        assert_eq!(counted[1].emoji, "🔥");
        assert!(b.open_reactions(5, 1, 10, &sealed).is_empty(), "moved to another message, they open as nothing");
    }

    #[test]
    fn a_call_key_reaches_the_other_side() {
        let (mut a, mut b) = pair_up();
        let key = crypto::random::<32>();
        let s = a.seal_call_key(5, 2, &key).unwrap();
        let call = CallInfo {
            conversation_id: 5,
            caller_id: 1,
            callee_id: 2,
            state: CallPhase::Ringing,
            started_at: 0,
            answered_at: None,
            sealed_key: s.text,
            sender_key: s.sender_key,
            recipient_key: s.recipient_key,
            outcome: None,
            roster: Vec::new(),
        };
        assert_eq!(b.open_call_key(&call), Some(key));
        assert_eq!(a.open_call_key(&call), Some(key));
    }

    #[test]
    fn another_computer_unlocks_with_the_recovery_key_and_only_with_it() {
        let identity = Identity::generate();
        let recovery = RecoveryKey::generate();
        let mine = OwnKey { id: 3, public_key: identity.public_b64(), wrapped: crypto::wrap(&identity, &recovery) };
        let mut v = Vault::new(temp_file(), 1);
        assert_eq!(v.on_keys(KeysReply { keys: Vec::new(), mine: Some(mine) }), Next::Nothing);
        assert_eq!(v.state, KeyState::Locked);
        assert!(!v.unlock("ABCD-EFGH"));
        assert!(!v.unlock(&RecoveryKey::generate().display()));
        assert!(v.unlock(&recovery.display().to_lowercase()));
        assert_eq!(v.state, KeyState::Ready);
        assert!(!v.needs_backup(), "they typed it in, so they have it");
    }

    #[test]
    fn a_new_key_made_elsewhere_locks_this_computer() {
        let (mut a, _) = pair_up();
        a.key_changed(PublicKeyInfo { id: 11, user_id: 1, public_key: Identity::generate().public_b64(), created_at: 0 });
        assert_eq!(a.state, KeyState::Locked);
        assert_eq!(a.seal_message(5, 2, &Payload::default()).unwrap_err(), SealError::NoKey);
    }

    #[test]
    fn a_changed_key_is_noticed_until_accepted() {
        let (mut a, _) = pair_up();
        a.pin(2);
        assert!(!a.key_changed_for(2));
        a.key_changed(PublicKeyInfo { id: 21, user_id: 2, public_key: Identity::generate().public_b64(), created_at: 0 });
        assert!(a.key_changed_for(2));
        a.accept_key(2);
        assert!(!a.key_changed_for(2));
    }

    #[test]
    fn nobody_without_a_key_can_be_written_to() {
        let (mut a, _) = pair_up();
        assert_eq!(a.seal_message(5, 99, &Payload::default()).unwrap_err(), SealError::PeerHasNoKey);
    }

    #[test]
    fn call_lines_read_naturally() {
        let ended = CallMeta { outcome: "ended".into(), duration_ms: Some(125_000) };
        assert_eq!(call_line(&ended, "Ana", true), "Call · 2 min");
        assert_eq!(call_line(&CallMeta { outcome: "missed".into(), duration_ms: None }, "Ana", false), "Missed call from Ana");
    }
}
