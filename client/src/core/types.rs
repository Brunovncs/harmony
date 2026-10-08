//! What the server sends, as the server shapes it (`server/src/*.js`, `publicUser`,
//! `publicChannel`, `publicMessage` and friends). Field names follow the JSON.
//!
//! A newer server may send values this client has never heard of, so lists are read element by
//! element (an entry that does not parse is skipped, not the whole list) and enums fall back to
//! a variant that is safe to show.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub type UserId = i64;
pub type ChannelId = i64;
pub type MessageId = i64;

/// The `error` codes the server answers with, and the client's own for failures that never got
/// an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    PasswordRequired,
    BadPassword,
    BadCredentials,
    WeakPassword,
    LoginRequired,
    Forbidden,
    NoSuchFile,
    NoSuchChannel,
    ChannelFull,
    NicknameTaken,
    InvalidNickname,
    BadCharacters,
    TooLong,
    LockedOut,
    OwnerOnly,
    LastOwner,
    CannotRemoveOwner,
    NoSuchUser,
    NoSuchMember,
    NotInChannel,
    NoSuchGroup,
    NoSuchMessage,
    NoSuchEmoji,
    NoSuchClip,
    NoSuchUpload,
    ServerFull,
    FileTooLarge,
    AvatarTooLarge,
    PictureTooLarge,
    EmojiTooLarge,
    ClipTooLarge,
    TooManyEmojis,
    NameTaken,
    InvalidName,
    NotAnImage,
    NotAudio,
    TypeNotAllowed,
    EmptyFile,
    EmptyMessage,
    MediaServerDown,
    BadOrder,
    ServerError,
    UnknownType,
    /// The socket is down, or went down before the reply.
    Offline,
    Timeout,
    Unreachable,
    /// MediaMTX refused the token.
    Unauthorized,
    /// Nothing is published on that path yet.
    NotLive,
    MediaError,
    MediaUnreachable,
    BadReply,
    BadHash,
    Corrupt,
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    pub fn parse(code: &str) -> ErrorCode {
        serde_json::from_value(Value::String(code.into())).unwrap_or(ErrorCode::Unknown)
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v = serde_json::to_value(self).unwrap_or(Value::Null);
        f.write_str(v.as_str().unwrap_or("unknown"))
    }
}

/// The entries of a JSON list that parse as `T`; the rest are logged and skipped.
pub fn parse_list<T: DeserializeOwned>(items: Vec<Value>) -> Vec<T> {
    items
        .into_iter()
        .filter_map(|v| match serde_json::from_value(v) {
            Ok(x) => Some(x),
            Err(e) => {
                log::warn!("skipped a {} this client does not understand: {e}", std::any::type_name::<T>());
                None
            }
        })
        .collect()
}

/// A pushed list, or `None` when the value is not a list at all (keep what you had).
pub fn list_of<T: DeserializeOwned>(v: Value) -> Option<Vec<T>> {
    match v {
        Value::Array(items) => Some(parse_list(items)),
        _ => None,
    }
}

pub fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    Ok(list_of(Value::deserialize(d)?).unwrap_or_default())
}

fn lenient_opt<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Option<T>, D::Error> {
    Ok(serde_json::from_value(Value::deserialize(d)?).ok())
}

fn lenient_rosters<'de, D: Deserializer<'de>>(d: D) -> Result<HashMap<String, Vec<Member>>, D::Error> {
    let Value::Object(map) = Value::deserialize(d)? else { return Ok(HashMap::new()) };
    Ok(map.into_iter().filter_map(|(k, v)| Some((k, list_of(v)?))).collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
    /// Also any role this client does not know: it gets no powers.
    #[default]
    #[serde(other)]
    Member,
}

impl Role {
    pub fn is_admin(self) -> bool {
        matches!(self, Role::Owner | Role::Admin)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: UserId,
    pub nickname: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub custom_name: bool,
    #[serde(default)]
    pub role: Role,
    #[serde(default)]
    pub avatar_hash: Option<String>,
    #[serde(default)]
    pub created_at: i64,
}

impl User {
    pub fn name(&self) -> &str {
        self.display_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.nickname)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Voice,
    Text,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub id: ChannelId,
    pub kind: ChannelKind,
    pub name: String,
    #[serde(default)]
    pub position: i64,
    #[serde(default)]
    pub group_id: Option<i64>,
    #[serde(default)]
    pub locked: bool,
    /// Only on the REST list; pushes leave it out.
    #[serde(default)]
    pub unlocked: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub position: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reaction {
    pub emoji: String,
    pub count: usize,
    #[serde(rename = "userIds", default)]
    pub user_ids: Vec<UserId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Image,
    Video,
    Audio,
    /// Also any kind this client does not know: it shows as a file to save.
    #[serde(other)]
    File,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: MessageId,
    pub channel_id: ChannelId,
    pub user_id: Option<UserId>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub attachment_hash: Option<String>,
    #[serde(default)]
    pub attachment_name: Option<String>,
    #[serde(default)]
    pub media_type: Option<MediaType>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub edited_at: Option<i64>,
    #[serde(default, deserialize_with = "lenient")]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub mentions: Vec<UserId>,
    #[serde(default)]
    pub mentions_everyone: bool,
    #[serde(default)]
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub emoji: Option<String>,
    pub hash: String,
    #[serde(default)]
    pub uploader: Option<String>,
    #[serde(default)]
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEmoji {
    pub id: i64,
    pub name: String,
    pub hash: String,
    #[serde(default)]
    pub uploaded_by: Option<UserId>,
    #[serde(default)]
    pub uploader: Option<String>,
    #[serde(default)]
    pub created_at: i64,
}

/// One person in a voice channel. `mid` is their slot, which names their media paths.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub mid: i64,
    pub user_id: UserId,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub deafened: bool,
    #[serde(default)]
    pub force_muted: bool,
    /// Which of their paths are live: `v` voice, `c` camera, `s` screen.
    #[serde(default)]
    pub publishing: Vec<String>,
}

impl Member {
    pub fn publishes(&self, kind: &str) -> bool {
        self.publishing.iter().any(|k| k == kind)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveStream {
    pub username: String,
    #[serde(default)]
    pub viewers: i64,
    #[serde(default)]
    pub since: Option<serde_json::Value>,
    pub whep_url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IceServer {
    #[serde(deserialize_with = "one_or_many")]
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Urls {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Urls::deserialize(d)? {
        Urls::One(s) => vec![s],
        Urls::Many(v) => v,
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    #[serde(default)]
    pub name: String,
    /// The picture's upload hash, None for none (and from servers older than pictures).
    #[serde(default)]
    pub icon_hash: Option<String>,
    #[serde(default)]
    pub password_required: bool,
    #[serde(default)]
    pub restart_required: bool,
}

/// `GET /api/health`.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub mediamtx: Option<String>,
    #[serde(default, rename = "signalingBase")]
    pub signaling_base: Option<String>,
    #[serde(default)]
    pub password_required: bool,
    #[serde(default)]
    pub authenticated: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub icon_hash: Option<String>,
    #[serde(default)]
    pub has_accounts: Option<bool>,
    #[serde(default)]
    pub needs_owner: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthReply {
    pub user: User,
    pub token: String,
    #[serde(default)]
    pub owner_claimed: bool,
    #[serde(default)]
    pub owner_key_rejected: bool,
}

/// `POST /api/session`: the claim on the flat namespace, and where voice gets its ICE servers.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReply {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub whip_url: Option<String>,
    #[serde(default)]
    pub ice_servers: Vec<IceServer>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Upload {
    pub hash: String,
}

/// `GET /api/server/storage`: the upload quota and how much of it is used.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Storage {
    pub used_bytes: u64,
    pub quota_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct History {
    #[serde(default, deserialize_with = "lenient")]
    pub messages: Vec<Message>,
    #[serde(default, deserialize_with = "lenient")]
    pub pinned: Vec<Message>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct SearchReply {
    #[serde(default, deserialize_with = "lenient")]
    pub results: Vec<Message>,
}

/// The publish URLs and read token for one voice channel, from `voice:joined` and
/// `voice:tokens`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VoiceTokens {
    pub channel_id: ChannelId,
    pub token: String,
    pub publish: Publish,
    pub whep_base: String,
    #[serde(default = "default_expiry")]
    pub expires_in_ms: u64,
}

fn default_expiry() -> u64 {
    600_000
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Publish {
    pub voice: String,
    pub cam: String,
    pub screen: String,
}

/// Everything `hello-ok` carries, and what `GET /api/channels` answers with.
#[derive(Clone, Debug, Deserialize, Default)]
pub struct Snapshot {
    #[serde(default, deserialize_with = "lenient_opt")]
    pub user: Option<User>,
    #[serde(default, deserialize_with = "lenient")]
    pub channels: Vec<Channel>,
    #[serde(default, deserialize_with = "lenient")]
    pub groups: Vec<Group>,
    #[serde(default, deserialize_with = "lenient_rosters")]
    pub rosters: HashMap<String, Vec<Member>>,
    #[serde(default, deserialize_with = "lenient")]
    pub online: Vec<UserId>,
    #[serde(default, rename = "voiceCap", deserialize_with = "lenient_opt")]
    pub voice_cap: Option<i64>,
}

/// Base 36, the way the server names channel paths (`vc-<cid36>-<mid36>-<kind>`).
pub fn base36(mut n: i64) -> String {
    if n == 0 {
        return "0".into();
    }
    let neg = n < 0;
    n = n.abs();
    let mut out = Vec::new();
    while n > 0 {
        out.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize]);
        n /= 36;
    }
    if neg {
        out.push(b'-');
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base36_matches_javascript() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(base36(1295), "zz");
    }

    #[test]
    fn messages_parse_as_the_server_writes_them() {
        let m: Message = serde_json::from_str(
            r#"{"id":3,"channelId":1,"userId":2,"nickname":"predo","body":"hi","attachmentHash":null,
            "attachmentName":null,"mediaType":null,"pinned":false,"editedAt":null,
            "reactions":[{"emoji":"👍","count":1,"userIds":[2]}],"mentions":[],"mentionsEveryone":false,"createdAt":1}"#,
        )
        .unwrap();
        assert_eq!(m.reactions[0].user_ids, vec![2]);
    }

    #[test]
    fn error_codes_parse_and_print_as_the_server_writes_them() {
        assert_eq!(ErrorCode::parse("channel_full"), ErrorCode::ChannelFull);
        assert_eq!(ErrorCode::parse("no_such_member"), ErrorCode::NoSuchMember);
        assert_eq!(ErrorCode::parse("not_an_image"), ErrorCode::NotAnImage);
        assert_eq!(ErrorCode::parse("something_new"), ErrorCode::Unknown);
        assert_eq!(ErrorCode::NoSuchChannel.to_string(), "no_such_channel");
    }

    #[test]
    fn unknown_values_fall_back_instead_of_failing() {
        let u: User = serde_json::from_str(r#"{"id":1,"nickname":"x","role":"moderator"}"#).unwrap();
        assert_eq!(u.role, Role::Member);
        let m: Message = serde_json::from_str(r#"{"id":1,"channelId":1,"userId":null,"mediaType":"hologram"}"#).unwrap();
        assert_eq!(m.media_type, Some(MediaType::File));
    }

    #[test]
    fn one_bad_entry_does_not_wipe_a_snapshot() {
        let snap: Snapshot = serde_json::from_str(
            r#"{"user":{"id":"not a number"},"voiceCap":"lots",
            "channels":[{"id":1,"kind":"text","name":"geral"},{"id":2,"kind":"stage","name":"new"},{"id":3,"kind":"voice","name":"Lobby"}],
            "groups":[{"id":1,"name":"g"},7],
            "rosters":{"3":[{"mid":0,"userId":1},{"mid":"x"}],"4":"nope"},
            "online":[1,"two",3]}"#,
        )
        .unwrap();
        assert!(snap.user.is_none() && snap.voice_cap.is_none());
        assert_eq!(snap.channels.iter().map(|c| c.id).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(snap.groups.len(), 1);
        assert_eq!(snap.rosters.len(), 1);
        assert_eq!(snap.rosters["3"].len(), 1);
        assert_eq!(snap.online, vec![1, 3]);
        let h: History = serde_json::from_str(r#"{"messages":[{"id":1,"channelId":1,"userId":2},{"id":"bad"}],"pinned":null}"#).unwrap();
        assert_eq!(h.messages.len(), 1);
        assert!(h.pinned.is_empty());
        assert!(list_of::<User>(serde_json::json!({"not": "a list"})).is_none());
    }

    #[test]
    fn ice_servers_take_one_url_or_many() {
        let a: IceServer = serde_json::from_str(r#"{"urls":"stun:x"}"#).unwrap();
        let b: IceServer = serde_json::from_str(r#"{"urls":["turn:y"],"username":"u","credential":"c"}"#).unwrap();
        assert_eq!(a.urls, vec!["stun:x"]);
        assert_eq!(b.username.as_deref(), Some("u"));
    }
}
