//! Every HTTP call to the control server and to MediaMTX's WHIP/WHEP endpoints.

use super::types::*;
use parking_lot::RwLock;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ApiError {
    pub status: u16,
    /// The server's `error` field, or one of ours.
    pub code: ErrorCode,
    pub message: String,
    pub body: Value,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    fn unreachable(base: &str) -> ApiError {
        ApiError {
            status: 0,
            code: ErrorCode::Unreachable,
            message: trf!(
                "Can't reach {}. Check the address and that the server is running.",
                "Não foi possível acessar {}. Confira o endereço e se o servidor está rodando.",
                base
            ),
            body: Value::Null,
        }
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;

/// Whether signing in to this address would send the password readable to anyone on the way: plain
/// HTTP to a host on the internet. A LAN, this computer and a Tailscale network (WireGuard
/// underneath) are not that.
pub fn sends_in_the_clear(address: &str) -> bool {
    use std::net::IpAddr;
    let Ok(url) = url::Url::parse(&normalize_base(address)) else { return false };
    if url.scheme() != "http" {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !nearby(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => !nearby(IpAddr::V6(ip)),
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            let local = [".local", ".lan", ".home", ".internal", ".home.arpa", ".ts.net"];
            !(name == "localhost" || !name.contains('.') || local.iter().any(|s| name.ends_with(s)))
        }
        None => false,
    }
}

/// Loopback, private, link-local, or Tailscale's 100.64.0.0/10 and fd7a:115c:a1e0::/48.
fn nearby(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || (a == 100 && (64..128).contains(&b))
        }
        std::net::IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| nearby(v4.into()))
        }
    }
}

/// Adds `http://` when there is no scheme and drops trailing slashes, as the old client did.
pub fn normalize_base(url: &str) -> String {
    let url = url.trim();
    let with_scheme = if url.contains("://") { url.to_string() } else { format!("http://{url}") };
    with_scheme.trim_end_matches('/').to_string()
}

#[derive(Default)]
struct Credentials {
    base: String,
    password: String,
    token: String,
}

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    creds: Arc<RwLock<Credentials>>,
}

impl Api {
    pub fn new() -> Api {
        let http = reqwest::Client::builder().user_agent(concat!("Harmony/", env!("CARGO_PKG_VERSION"))).build().expect("http client");
        Api { http, creds: Default::default() }
    }

    pub fn set_server(&self, base: &str, password: &str) {
        let mut c = self.creds.write();
        c.base = normalize_base(base);
        c.password = password.to_string();
    }

    pub fn set_token(&self, token: &str) {
        self.creds.write().token = token.to_string();
    }

    pub fn token(&self) -> String {
        self.creds.read().token.clone()
    }

    pub fn base(&self) -> String {
        self.creds.read().base.clone()
    }

    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let c = self.creds.read();
        let mut rb = rb;
        if !c.password.is_empty() {
            rb = rb.header("X-Harmony-Password", &c.password);
        }
        if !c.token.is_empty() {
            rb = rb.bearer_auth(&c.token);
        }
        rb
    }

    async fn send<T: DeserializeOwned>(&self, rb: reqwest::RequestBuilder, timeout: Duration) -> Result<T> {
        let base = self.base();
        let res = self.authed(rb).timeout(timeout).send().await.map_err(|_| ApiError::unreachable(&base))?;
        let status = res.status().as_u16();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let code = body.get("error").and_then(Value::as_str).map(ErrorCode::parse).unwrap_or(ErrorCode::Unknown);
            let message = message_for(code, status, &body);
            return Err(ApiError { status, code, message, body });
        }
        serde_json::from_value(body.clone()).map_err(|e| ApiError {
            status,
            code: ErrorCode::BadReply,
            message: trf!(
                "The server sent something this client does not understand ({}).",
                "O servidor enviou algo que este cliente não entende ({}).",
                e
            ),
            body,
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base());
        self.send(self.http.get(url), Duration::from_secs(10)).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: impl Serialize) -> Result<T> {
        let url = format!("{}{path}", self.base());
        self.send(self.http.post(url).json(&body), Duration::from_secs(10)).await
    }

    // Health, accounts and the server itself.

    pub async fn health(&self) -> Result<Health> {
        self.get("/api/health").await
    }

    pub async fn register(&self, nickname: &str, password: &str, owner_key: Option<&str>) -> Result<AuthReply> {
        self.post("/api/accounts/register", json!({ "nickname": nickname, "password": password, "ownerKey": owner_key })).await
    }

    pub async fn login(&self, nickname: &str, password: &str, owner_key: Option<&str>) -> Result<AuthReply> {
        self.post("/api/accounts/login", json!({ "nickname": nickname, "password": password, "ownerKey": owner_key })).await
    }

    pub async fn logout(&self) -> Result<Value> {
        self.post("/api/accounts/logout", json!({})).await
    }

    pub async fn me(&self) -> Result<User> {
        #[derive(serde::Deserialize)]
        struct R {
            user: User,
        }
        Ok(self.get::<R>("/api/accounts/me").await?.user)
    }

    pub async fn set_avatar(&self, hash: Option<&str>) -> Result<User> {
        Ok(self.post::<UserReply>("/api/accounts/avatar", json!({ "hash": hash })).await?.user)
    }

    pub async fn set_display_name(&self, name: &str) -> Result<User> {
        Ok(self.post::<UserReply>("/api/accounts/display-name", json!({ "displayName": name })).await?.user)
    }

    /// Every session of the account ends, this one included; the reply is this one's new token.
    pub async fn change_password(&self, current: &str, password: &str) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct R {
            token: String,
        }
        Ok(self.post::<R>("/api/accounts/password", json!({ "current": current, "password": password })).await?.token)
    }

    pub async fn users(&self) -> Result<Vec<User>> {
        #[derive(serde::Deserialize)]
        struct R {
            #[serde(deserialize_with = "lenient")]
            users: Vec<User>,
        }
        Ok(self.get::<R>("/api/accounts").await?.users)
    }

    pub async fn set_role(&self, user: UserId, role: Role) -> Result<User> {
        Ok(self.post::<UserReply>(&format!("/api/accounts/{user}/role"), json!({ "role": role })).await?.user)
    }

    pub async fn delete_account(&self, user: UserId) -> Result<Value> {
        self.post(&format!("/api/accounts/{user}/delete"), json!({})).await
    }

    pub async fn server(&self) -> Result<ServerInfo> {
        Ok(self.get::<ServerReply>("/api/server").await?.server)
    }

    /// `password: None` leaves it alone; `Some("")` removes it.
    pub async fn update_server(&self, name: &str, password: Option<&str>) -> Result<ServerInfo> {
        let mut body = json!({ "name": name });
        if let Some(p) = password {
            body["password"] = json!(p);
        }
        Ok(self.post::<ServerReply>("/api/server", body).await?.server)
    }

    /// `None` takes the picture off.
    pub async fn set_server_icon(&self, hash: Option<&str>) -> Result<ServerInfo> {
        Ok(self.post::<ServerReply>("/api/server", json!({ "iconHash": hash })).await?.server)
    }

    pub async fn storage(&self) -> Result<Storage> {
        self.get("/api/server/storage").await
    }

    // The flat namespace.

    pub async fn session(&self, username: &str, camera: bool) -> Result<SessionReply> {
        let mut body = json!({ "username": username });
        if camera {
            body["kind"] = json!("camera");
        }
        self.post("/api/session", body).await
    }

    pub async fn release_session(&self, username: &str, token: &str) -> Result<Value> {
        self.post("/api/session/release", json!({ "username": username, "token": token })).await
    }

    pub async fn streams(&self) -> Result<(Vec<LiveStream>, Vec<IceServer>)> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct R {
            streams: Vec<LiveStream>,
            #[serde(default)]
            ice_servers: Vec<IceServer>,
        }
        let r: R = self.get("/api/streams").await?;
        Ok((r.streams, r.ice_servers))
    }

    // Channels.

    pub async fn channels(&self) -> Result<Snapshot> {
        self.get("/api/channels").await
    }

    pub async fn create_channel(&self, kind: ChannelKind, name: &str, password: Option<&str>) -> Result<Channel> {
        Ok(self.post::<ChannelReply>("/api/channels", json!({ "kind": kind, "name": name, "password": password })).await?.channel)
    }

    pub async fn update_channel(&self, id: ChannelId, name: &str, password: Option<&str>) -> Result<Channel> {
        let mut body = json!({ "name": name });
        if let Some(p) = password {
            body["password"] = json!(p);
        }
        Ok(self.post::<ChannelReply>(&format!("/api/channels/{id}"), body).await?.channel)
    }

    pub async fn set_channel_mic_locked(&self, id: ChannelId, locked: bool) -> Result<Channel> {
        Ok(self.post::<ChannelReply>(&format!("/api/channels/{id}"), json!({ "micLocked": locked })).await?.channel)
    }

    pub async fn delete_channel(&self, id: ChannelId) -> Result<Value> {
        self.post(&format!("/api/channels/{id}/delete"), json!({})).await
    }

    pub async fn create_group(&self, name: &str) -> Result<Value> {
        self.post("/api/channels/groups", json!({ "name": name })).await
    }

    pub async fn rename_group(&self, id: i64, name: &str) -> Result<Value> {
        self.post(&format!("/api/channels/groups/{id}"), json!({ "name": name })).await
    }

    pub async fn delete_group(&self, id: i64) -> Result<Value> {
        self.post(&format!("/api/channels/groups/{id}/delete"), json!({})).await
    }

    /// Positions follow list order. `channels` is every channel, in drawing order.
    pub async fn arrange(&self, groups: &[i64], channels: &[(ChannelId, Option<i64>)]) -> Result<Value> {
        let channels: Vec<Value> = channels.iter().map(|(id, g)| json!({ "id": id, "groupId": g })).collect();
        self.post("/api/channels/arrange", json!({ "groups": groups, "channels": channels })).await
    }

    // Chat.

    pub async fn messages(&self, channel: ChannelId, before: Option<MessageId>) -> Result<History> {
        let q = before.map(|b| format!("?before={b}")).unwrap_or_default();
        self.get(&format!("/api/channels/{channel}/messages{q}")).await
    }

    pub async fn send_message(&self, channel: ChannelId, body: &str, attachment: Option<(&str, &str)>) -> Result<Message> {
        let mut b = json!({ "body": body });
        if let Some((hash, name)) = attachment {
            b["attachmentHash"] = json!(hash);
            b["attachmentName"] = json!(name);
        }
        Ok(self.post::<MessageReply>(&format!("/api/channels/{channel}/messages"), b).await?.message)
    }

    pub async fn pin(&self, id: MessageId, pinned: bool) -> Result<Message> {
        Ok(self.post::<MessageReply>(&format!("/api/messages/{id}/pin"), json!({ "pinned": pinned })).await?.message)
    }

    pub async fn edit(&self, id: MessageId, body: &str) -> Result<Message> {
        Ok(self.post::<MessageReply>(&format!("/api/messages/{id}/edit"), json!({ "body": body })).await?.message)
    }

    pub async fn replace_attachment(&self, id: MessageId, hash: Option<&str>, name: Option<&str>) -> Result<Message> {
        Ok(self.post::<MessageReply>(&format!("/api/messages/{id}/attachment"), json!({ "hash": hash, "name": name })).await?.message)
    }

    pub async fn delete_message(&self, id: MessageId) -> Result<Value> {
        self.post(&format!("/api/messages/{id}/delete"), json!({})).await
    }

    pub async fn react(&self, id: MessageId, emoji: &str, on: bool) -> Result<Value> {
        self.post(&format!("/api/messages/{id}/react"), json!({ "emoji": emoji, "on": on })).await
    }

    pub async fn search(&self, channel: ChannelId, q: &str) -> Result<SearchReply> {
        let q = url::form_urlencoded::byte_serialize(q.as_bytes()).collect::<String>();
        self.get(&format!("/api/channels/{channel}/search?q={q}")).await
    }

    // Files.

    pub async fn upload(&self, bytes: Vec<u8>, content_type: &str) -> Result<Upload> {
        let url = format!("{}/api/uploads", self.base());
        self.send(self.http.post(url).header("Content-Type", content_type).body(bytes), Duration::from_secs(60)).await
    }

    pub async fn download(&self, hash: &str) -> Result<(Vec<u8>, String)> {
        self.file(&format!("/api/uploads/{hash}")).await
    }

    /// The server's picture, which needs no account: the sign-in screen shows it. A server of the
    /// Electron line keeps its one picture at `/api/server/logo` instead.
    pub async fn server_icon(&self, hash: &str) -> Result<(Vec<u8>, String)> {
        match self.file(&format!("/api/server/icon/{hash}")).await {
            Err(e) if e.status == 404 => self.file("/api/server/logo").await,
            got => got,
        }
    }

    async fn file(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let base = self.base();
        let url = format!("{base}{path}");
        let res =
            self.authed(self.http.get(url)).timeout(Duration::from_secs(30)).send().await.map_err(|_| ApiError::unreachable(&base))?;
        let status = res.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(ApiError {
                status,
                code: ErrorCode::NoSuchFile,
                message: message_for(ErrorCode::NoSuchFile, status, &Value::Null),
                body: Value::Null,
            });
        }
        let ct = res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
        let bytes = res.bytes().await.map_err(|_| ApiError::unreachable(&base))?;
        Ok((bytes.to_vec(), ct))
    }

    // Emoji and soundpad.

    pub async fn emojis(&self) -> Result<Vec<CustomEmoji>> {
        #[derive(serde::Deserialize)]
        struct R {
            #[serde(deserialize_with = "lenient")]
            emojis: Vec<CustomEmoji>,
        }
        Ok(self.get::<R>("/api/emojis").await?.emojis)
    }

    pub async fn add_emoji(&self, name: &str, hash: &str) -> Result<Value> {
        self.post("/api/emojis", json!({ "name": name, "hash": hash })).await
    }

    pub async fn delete_emoji(&self, id: i64) -> Result<Value> {
        self.post(&format!("/api/emojis/{id}/delete"), json!({})).await
    }

    pub async fn soundpad(&self) -> Result<Vec<Clip>> {
        #[derive(serde::Deserialize)]
        struct R {
            #[serde(deserialize_with = "lenient")]
            clips: Vec<Clip>,
        }
        Ok(self.get::<R>("/api/soundpad").await?.clips)
    }

    pub async fn add_clip(&self, name: &str, emoji: Option<&str>, hash: &str) -> Result<Value> {
        self.post("/api/soundpad", json!({ "name": name, "emoji": emoji, "hash": hash })).await
    }

    pub async fn rename_clip(&self, id: i64, name: &str, emoji: Option<&str>) -> Result<Value> {
        self.post(&format!("/api/soundpad/{id}/rename"), json!({ "name": name, "emoji": emoji })).await
    }

    pub async fn delete_clip(&self, id: i64) -> Result<Value> {
        self.post(&format!("/api/soundpad/{id}/delete"), json!({})).await
    }

    pub async fn reorder_clips(&self, ids: &[i64]) -> Result<Value> {
        self.post("/api/soundpad/reorder", json!({ "ids": ids })).await
    }

    // WHIP and WHEP.

    /// POSTs a complete offer and returns the answer and the session's resource URL.
    pub async fn sdp_exchange(&self, url: &str, offer: &str) -> Result<(String, Option<String>)> {
        let res = self
            .http
            .post(url)
            .header("Content-Type", "application/sdp")
            .body(offer.to_string())
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|_| ApiError {
                status: 0,
                code: ErrorCode::MediaUnreachable,
                message: tr!("Can't reach the media server.", "Não foi possível acessar o servidor de mídia.").into(),
                body: Value::Null,
            })?;
        let status = res.status().as_u16();
        let location = res.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string);
        let text = res.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            let code = match status {
                401 | 403 => ErrorCode::Unauthorized,
                404 => ErrorCode::NotLive,
                _ => ErrorCode::MediaError,
            };
            return Err(ApiError {
                status,
                code,
                message: trf!("The media server said {}.", "O servidor de mídia respondeu {}.", status),
                body: Value::String(text),
            });
        }
        let resource = location.and_then(|l| url::Url::parse(url).ok()?.join(&l).ok()).map(|u| u.to_string());
        Ok((text, resource))
    }

    /// Ends a WHIP or WHEP session. Best effort.
    pub async fn delete_resource(&self, url: &str) {
        let _ = self.http.delete(url).timeout(Duration::from_secs(5)).send().await;
    }
}

impl Default for Api {
    fn default() -> Self {
        Api::new()
    }
}

#[derive(serde::Deserialize)]
struct UserReply {
    user: User,
}

#[derive(serde::Deserialize)]
struct ServerReply {
    server: ServerInfo,
}

#[derive(serde::Deserialize)]
struct ChannelReply {
    channel: Channel,
}

#[derive(serde::Deserialize)]
struct MessageReply {
    message: Message,
}

/// The server's message for a failure: ours, in the reader's language, for every code we know;
/// the server's own (in English) for one we don't; the status when there is neither.
fn message_for(code: ErrorCode, status: u16, body: &Value) -> String {
    friendly(code, body)
        .or_else(|| body.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| trf!("The server answered {}.", "O servidor respondeu {}.", status))
}

/// What to tell people about a code the server (HTTP or socket) refuses with.
pub fn friendly(code: ErrorCode, body: &Value) -> Option<String> {
    use ErrorCode as E;
    let text = match code {
        E::PasswordRequired => tr!("This server needs a password.", "Este servidor pede uma senha."),
        E::BadPassword => tr!("Wrong server password.", "Senha do servidor incorreta."),
        E::BadCredentials => tr!("Wrong username or password.", "Nome de usuário ou senha incorretos."),
        E::WeakPassword => tr!("Use at least 6 characters.", "Use pelo menos 6 caracteres."),
        E::LoginRequired => tr!("Sign in first.", "Entre na sua conta primeiro."),
        E::LockedOut => {
            let secs = body.get("retryAfterSec").and_then(Value::as_u64).unwrap_or(60);
            return Some(if secs < 90 {
                trf!("Too many wrong passwords. Try again in {} seconds.", "Senha errada vezes demais. Tente de novo em {} segundos.", secs)
            } else {
                trf!(
                    "Too many wrong passwords. Try again in {} minutes.",
                    "Senha errada vezes demais. Tente de novo em {} minutos.",
                    secs.div_ceil(60)
                )
            });
        }
        E::Forbidden => tr!("You are not allowed to do that.", "Você não tem permissão para fazer isso."),
        E::OwnerOnly => tr!("Only the owner can do that.", "Só o dono pode fazer isso."),
        E::LastOwner => tr!("The server needs at least one owner.", "O servidor precisa de pelo menos um dono."),
        E::CannotRemoveOwner => tr!("The owner's account can't be deleted.", "A conta do dono não pode ser apagada."),
        E::NicknameTaken => tr!("That username is taken. Try another one.", "Esse nome de usuário já existe. Tente outro."),
        E::InvalidNickname => tr!(
            "Use 2 to 20 letters, digits, - or _, starting with a letter or digit.",
            "Use de 2 a 20 letras, números, - ou _, começando com letra ou número."
        ),
        E::TooLong => tr!("That name is too long. 32 characters at most.", "Esse nome é longo demais. No máximo 32 caracteres."),
        E::BadCharacters => tr!("That name has characters that can't be shown.", "Esse nome tem caracteres que não dá para mostrar."),
        E::InvalidName => tr!("Give it a name of up to 32 characters.", "Dê um nome de até 32 caracteres."),
        E::NameTaken => tr!("Something else already has that name.", "Já existe outro item com esse nome."),
        E::NoSuchUser => tr!("That account is gone.", "Essa conta não existe mais."),
        E::NoSuchMember => tr!("They already left the channel.", "Essa pessoa já saiu do canal."),
        E::NotInChannel => tr!("You are not in that voice channel any more.", "Você não está mais nesse canal de voz."),
        E::NoSuchChannel => tr!("That channel is gone.", "Esse canal não existe mais."),
        E::NoSuchGroup => tr!("That group is gone.", "Esse grupo não existe mais."),
        E::NoSuchMessage => tr!("That message is gone.", "Essa mensagem não existe mais."),
        E::NoSuchEmoji => tr!("That emoji is gone.", "Esse emoji não existe mais."),
        E::NoSuchClip => tr!("That sound is gone.", "Esse som não existe mais."),
        E::NoSuchFile | E::NoSuchUpload | E::BadHash => {
            tr!("That file is gone from the server.", "Esse arquivo não existe mais no servidor.")
        }
        E::ChannelFull => tr!("That voice channel is full.", "Esse canal de voz está cheio."),
        E::ServerFull => tr!("The server is out of space for files.", "O servidor ficou sem espaço para arquivos."),
        E::FileTooLarge => tr!("Files can be up to 25 MB.", "Os arquivos podem ter até 25 MB."),
        E::AvatarTooLarge | E::PictureTooLarge | E::EmojiTooLarge => {
            tr!("That picture is too big. 256 KB at most.", "Essa imagem é grande demais. No máximo 256 KB.")
        }
        E::ClipTooLarge => tr!("Sounds can be up to 2 MB.", "Os sons podem ter até 2 MB."),
        E::TooManyEmojis => {
            tr!("This server has all the custom emoji it can hold.", "Este servidor já tem o máximo de emojis personalizados.")
        }
        E::NotAnImage => tr!("That file is not a picture.", "Esse arquivo não é uma imagem."),
        E::NotAudio => tr!("That file is not audio.", "Esse arquivo não é de áudio."),
        E::TypeNotAllowed => tr!("The server does not take that kind of file.", "O servidor não aceita esse tipo de arquivo."),
        E::EmptyFile => tr!("That file is empty.", "Esse arquivo está vazio."),
        E::EmptyMessage => tr!("The message would be empty. Write something first.", "A mensagem ficaria vazia. Escreva algo antes."),
        E::MediaServerDown => {
            tr!(
                "The media server is not answering. Try again in a moment.",
                "O servidor de mídia não está respondendo. Tente de novo daqui a pouco."
            )
        }
        E::BadOrder => tr!("The list changed meanwhile. Try again.", "A lista mudou nesse meio-tempo. Tente de novo."),
        E::ServerError => tr!("Something went wrong on the server. Try again.", "Algo deu errado no servidor. Tente de novo."),
        E::UnknownType => tr!("This server is too old for that. Update it.", "Este servidor é antigo demais para isso. Atualize-o."),
        E::Offline => {
            tr!("Not connected to the server. Try again once it's back.", "Sem conexão com o servidor. Tente de novo quando ela voltar.")
        }
        E::Timeout => tr!("The server didn't answer. Try again.", "O servidor não respondeu. Tente de novo."),
        _ => return None,
    };
    Some(text.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bases_get_a_scheme_and_lose_trailing_slashes() {
        assert_eq!(normalize_base("pi.local:8080/"), "http://pi.local:8080");
        assert_eq!(normalize_base(" https://h.example.com// "), "https://h.example.com");
    }

    #[test]
    fn only_plain_http_across_the_internet_is_in_the_clear() {
        for open in ["harmony.example.com:8080", "http://203.0.113.7:8080", "http://[2001:db8::1]:8080", "100.128.0.1"] {
            assert!(sends_in_the_clear(open), "{open}");
        }
        for fine in [
            "https://harmony.example.com",
            "pi.local:8080",
            "rockpi:8080",
            "localhost:8080",
            "127.0.0.1:8080",
            "192.168.0.20:8080",
            "10.1.2.3",
            "172.20.0.5",
            "169.254.1.1",
            "100.64.0.1",
            "100.127.255.254",
            "box.tail1234.ts.net",
            "http://[::1]:8080",
            "http://[fd7a:115c:a1e0::1]:8080",
            "",
        ] {
            assert!(!sends_in_the_clear(fine), "{fine}");
        }
    }
}
