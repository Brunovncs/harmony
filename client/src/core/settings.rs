//! `settings.json`, in the same place and with the same keys the Electron client used, so an
//! upgrade keeps the server, the sign-in and every preference. Keys this client does not know
//! are kept as they were. The passwords and sign-ins in it are sealed for this Windows user (see
//! `secret`); the plain ones an older client wrote are read and sealed on the spot.

use super::api::normalize_base;
use super::secret;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// The server in use, and who you are on it. With several saved servers these always mirror
    /// the active one, so the old keys keep meaning what they did.
    pub server_url: String,
    pub username: String,
    /// The server's own password, not the account's.
    pub password: String,
    pub session_token: String,
    /// Every server you connected to, in the rail's order.
    pub saved_servers: Vec<SavedServer>,
    /// The names you go by across those servers, one per username, in the account menu's order.
    pub accounts: Vec<Account>,
    /// The username whose servers the rail shows.
    pub active_account: String,
    pub remember_account: bool,
    pub media_cache_mb: u64,
    pub resolution: String,
    pub framerate: u32,
    pub priority: String,
    pub audio_input_id: String,
    pub voice_input_id: String,
    pub voice_output_id: String,
    pub voice_camera_id: String,
    pub mic_gain: u32,
    pub mic_sensitivity: u32,
    pub voice_sounds: bool,
    pub theme: String,
    pub custom_theme: Option<CustomTheme>,
    pub show_members: bool,
    pub soundpad_volume: u32,
    pub recent_emoji: Vec<String>,
    pub ui_scale: u32,
    pub mention_sound: bool,
    /// Every cue's volume, as a percentage: 100 is as designed, up to 200 for a noisy room.
    pub sound_volume: u32,
    /// Global hotkeys by action (`mute`, `deafen`), as Electron accelerators (`Ctrl+Shift+M`);
    /// empty is unbound.
    pub hotkeys: BTreeMap<String, String>,
    /// Soundboard hotkeys by server address, then by clip id: a clip id only means something on
    /// the server that gave it.
    pub clip_hotkeys: BTreeMap<String, BTreeMap<String, String>>,
    pub clips_enabled: bool,
    /// Text channels you silenced, by server address: no unread mark and no sound for them. Yours
    /// alone and on this computer only; a channel id only means something on its server.
    pub muted_channels: BTreeMap<String, Vec<i64>>,
    /// A short tone when a message arrives in a channel you are not looking at.
    pub message_sound: bool,
    pub hardware_encoding: String,
    pub gpu_preference: String,
    pub window_audio_fallback: String,
    // New in the native client.
    /// Asks GitHub for a newer version at start and every few hours.
    pub check_updates: bool,
    pub noise_suppression: bool,
    pub echo_cancellation: bool,
    pub camera_background: Background,
    pub custom_backgrounds: Vec<String>,
    /// "en", "pt", or empty to follow the system.
    pub language: String,
    /// Closing the window leaves Harmony running in the notification area.
    pub close_to_tray: bool,
    /// The notice saying so has been shown, the first time the window closed to it.
    pub tray_notice_seen: bool,
    /// Clicking a voice channel asks before joining it.
    pub confirm_voice_join: bool,
    /// Every voice channel is entered with the microphone off, switching between them included.
    pub join_muted: bool,
}

/// A server in the rail: where it is, who you are there, and its name and picture as last seen,
/// so the rail draws before anything answers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SavedServer {
    pub url: String,
    pub password: String,
    pub username: String,
    pub session_token: String,
    pub name: String,
    /// The picture's upload hash, kept in the media cache; empty for none.
    pub icon: String,
}

impl SavedServer {
    /// The address without its scheme, as people type it.
    pub fn host(&self) -> String {
        let base = normalize_base(&self.url);
        base.split_once("://").map(|(_, h)| h.to_string()).unwrap_or(base)
    }

    fn is(&self, url: &str, username: &str) -> bool {
        self.username == username && same_server(&self.url, url)
    }
}

/// A name you go by, and the saved servers you are signed in to with it. Servers do not share
/// accounts, so this is only how this computer groups them; the name and picture are what the
/// server last used gave.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Account {
    pub username: String,
    pub display_name: String,
    /// The picture's upload hash, and the server it was seen on: an upload belongs to its server.
    pub avatar: String,
    pub avatar_server: String,
    /// Where switching to this account goes.
    pub last_server: String,
}

impl Account {
    pub fn name(&self) -> &str {
        if self.display_name.is_empty() { &self.username } else { &self.display_name }
    }

    /// The picture to keep once the server at `url` shows `avatar`: a server with none leaves
    /// alone one that another server showed.
    fn picture_after<'a>(&'a self, url: &'a str, avatar: &'a str) -> (&'a str, &'a str) {
        match avatar.is_empty() {
            false => (avatar, url),
            true if same_server(&self.avatar_server, url) => ("", ""),
            true => (&self.avatar, &self.avatar_server),
        }
    }
}

/// Whether two addresses are the same server, however they were typed.
pub fn same_server(a: &str, b: &str) -> bool {
    !a.trim().is_empty() && normalize_base(a).eq_ignore_ascii_case(&normalize_base(b))
}

impl Settings {
    /// That server, as any of your accounts saved it.
    pub fn saved_server(&self, url: &str) -> Option<&SavedServer> {
        self.saved_servers.iter().find(|s| same_server(&s.url, url))
    }

    pub fn saved_server_as(&self, url: &str, username: &str) -> Option<&SavedServer> {
        self.saved_servers.iter().find(|s| s.is(url, username))
    }

    /// The saved server the top-level keys point at.
    pub fn active_server(&self) -> Option<usize> {
        self.saved_servers.iter().position(|s| s.is(&self.server_url, &self.username))
    }

    pub fn account(&self, username: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.username == username)
    }

    /// An account's servers, with their places in `saved_servers`.
    pub fn account_servers<'a>(&'a self, username: &'a str) -> impl Iterator<Item = (usize, &'a SavedServer)> + 'a {
        self.saved_servers.iter().enumerate().filter(move |(_, s)| s.username == username)
    }

    /// Where switching to an account goes: the server it last used, or else its first.
    pub fn account_home(&self, username: &str) -> Option<String> {
        let last = self.account(username).map(|a| a.last_server.as_str()).unwrap_or_default();
        let servers: Vec<&String> = self.account_servers(username).map(|(_, s)| &s.url).collect();
        servers.iter().find(|url| same_server(url, last)).or(servers.first()).map(|url| url.to_string())
    }

    /// After a sign-in: adds the server or refreshes it where it is, and makes it the active one,
    /// with its account.
    pub fn remember_server(&mut self, server: SavedServer) {
        match self.saved_servers.iter_mut().find(|s| s.is(&server.url, &server.username)) {
            Some(s) => *s = server.clone(),
            None => self.saved_servers.push(server.clone()),
        }
        self.tidy_accounts();
        self.use_server(&server);
    }

    /// Points the top-level keys at a saved server; false when there is no such server.
    pub fn switch_server(&mut self, url: &str, username: &str) -> bool {
        let Some(server) = self.saved_server_as(url, username).cloned() else { return false };
        self.use_server(&server);
        true
    }

    /// Takes a server off the list. When it was the active one the top-level keys are cleared,
    /// and an account left with no servers goes with it.
    pub fn forget_server(&mut self, url: &str, username: &str) -> Option<SavedServer> {
        let i = self.saved_servers.iter().position(|s| s.is(url, username))?;
        if self.active_server() == Some(i) {
            self.clear_active();
        }
        let gone = self.saved_servers.remove(i);
        self.tidy_accounts();
        Some(gone)
    }

    /// Takes an account and all its servers off, for signing out of every one of them.
    pub fn forget_account(&mut self, username: &str) -> Vec<SavedServer> {
        if self.active_server().is_some() && self.username == username {
            self.clear_active();
        }
        let (gone, kept) = std::mem::take(&mut self.saved_servers).into_iter().partition(|s| s.username == username);
        self.saved_servers = kept;
        self.tidy_accounts();
        gone
    }

    /// Moves a server to `to`, a place in `saved_servers`.
    pub fn move_server(&mut self, url: &str, username: &str, to: usize) {
        let Some(from) = self.saved_servers.iter().position(|s| s.is(url, username)) else { return };
        let s = self.saved_servers.remove(from);
        self.saved_servers.insert(to.min(self.saved_servers.len()), s);
    }

    /// Drops a sign-in the server refused.
    pub fn forget_token(&mut self, url: &str, username: &str) {
        if let Some(s) = self.saved_servers.iter_mut().find(|s| s.is(url, username)) {
            s.session_token.clear();
        }
        if self.username == username && same_server(&self.server_url, url) {
            self.session_token.clear();
        }
    }

    /// Whether every account's copy of a server has this name and picture.
    pub fn knows_server(&self, url: &str, name: &str, icon: &str) -> bool {
        !self.saved_servers.iter().any(|s| same_server(&s.url, url) && (s.name != name || s.icon != icon))
    }

    /// What a server last said about itself, into every account's copy of it.
    pub fn note_server(&mut self, url: &str, name: &str, icon: &str) {
        for s in self.saved_servers.iter_mut().filter(|s| same_server(&s.url, url)) {
            (s.name, s.icon) = (name.into(), icon.into());
        }
    }

    /// Whether an account has this name and picture, as the server at `url` shows them.
    pub fn knows_identity(&self, username: &str, url: &str, display_name: &str, avatar: &str) -> bool {
        self.account(username).is_none_or(|a| {
            let (avatar, server) = a.picture_after(url, avatar);
            a.display_name == display_name && a.avatar == avatar && a.avatar_server == server
        })
    }

    /// Your name and picture as the server in use shows them.
    pub fn note_identity(&mut self, username: &str, url: &str, display_name: &str, avatar: &str) {
        if let Some(a) = self.accounts.iter_mut().find(|a| a.username == username) {
            let (avatar, server) = a.picture_after(url, avatar);
            (a.display_name, a.avatar, a.avatar_server) = (display_name.into(), avatar.to_string(), server.to_string());
        }
    }

    fn use_server(&mut self, s: &SavedServer) {
        self.server_url = s.url.clone();
        self.password = s.password.clone();
        self.username = s.username.clone();
        self.session_token = s.session_token.clone();
        self.active_account = s.username.clone();
        if let Some(a) = self.accounts.iter_mut().find(|a| a.username == s.username) {
            a.last_server = s.url.clone();
        }
    }

    fn clear_active(&mut self) {
        self.server_url.clear();
        self.password.clear();
        self.username.clear();
        self.session_token.clear();
    }

    /// One account per username among the saved servers, in the order they first appear, and the
    /// active one among them: the server in use's, or else the one picked before, or the first.
    fn tidy_accounts(&mut self) {
        let mut names: Vec<&str> = Vec::new();
        for s in &self.saved_servers {
            if !names.contains(&s.username.as_str()) {
                names.push(&s.username);
            }
        }
        let mut seen: Vec<String> = Vec::new();
        self.accounts.retain(|a| {
            let keep = names.contains(&a.username.as_str()) && !seen.contains(&a.username);
            seen.push(a.username.clone());
            keep
        });
        for name in names {
            if self.accounts.iter().all(|a| a.username != name) {
                let last_server = self.saved_servers.iter().find(|s| s.username == name).map(|s| s.url.clone()).unwrap_or_default();
                self.accounts.push(Account { username: name.into(), last_server, ..Default::default() });
            }
        }
        if let Some(i) = self.active_server() {
            self.active_account = self.saved_servers[i].username.clone();
        } else if self.account(&self.active_account).is_none() {
            self.active_account = self.accounts.first().map(|a| a.username.clone()).unwrap_or_default();
        }
    }

    /// Carries changes made through the top-level keys (signing out, a new server password)
    /// into the saved server they describe.
    fn sync_active(&mut self) {
        let Some(i) = self.active_server() else { return };
        let s = &mut self.saved_servers[i];
        s.password = self.password.clone();
        s.username = self.username.clone();
        s.session_token = self.session_token.clone();
    }

    /// The combination bound to an action, or "" for none. Clips are looked up on `server`.
    pub fn hotkey(&self, server: &str, action: HotkeyAction) -> &str {
        match action {
            HotkeyAction::Mute => self.hotkeys.get("mute"),
            HotkeyAction::Deafen => self.hotkeys.get("deafen"),
            HotkeyAction::Clip(id) => self.server_clip_hotkeys(server).and_then(|m| m.get(&id.to_string())),
        }
        .map_or("", String::as_str)
    }

    /// Whether you silenced this channel on `server`.
    pub fn channel_muted(&self, server: &str, channel: i64) -> bool {
        self.muted_channels.iter().any(|(url, ids)| same_server(url, server) && ids.contains(&channel))
    }

    pub fn set_channel_muted(&mut self, server: &str, channel: i64, muted: bool) {
        let url = self
            .muted_channels
            .keys()
            .find(|url| same_server(url, server))
            .cloned()
            .or_else(|| self.saved_server(server).map(|s| s.url.clone()))
            .unwrap_or_else(|| server.to_string());
        let ids = self.muted_channels.entry(url.clone()).or_default();
        ids.retain(|&id| id != channel);
        if muted {
            ids.push(channel);
        }
        if ids.is_empty() {
            self.muted_channels.remove(&url);
        }
    }

    /// A server's soundboard hotkeys, by clip id.
    pub fn server_clip_hotkeys(&self, server: &str) -> Option<&BTreeMap<String, String>> {
        self.clip_hotkeys.iter().find(|(url, _)| same_server(url, server)).map(|(_, m)| m)
    }

    /// Binds `combo` to an action, or unbinds it when empty. One combination does one thing:
    /// binding one that is taken moves it here.
    pub fn bind_hotkey(&mut self, server: &str, action: HotkeyAction, combo: &str) {
        let taken = |c: &str| !combo.is_empty() && c.eq_ignore_ascii_case(combo);
        let (name, clip) = match action {
            HotkeyAction::Mute => ("mute", String::new()),
            HotkeyAction::Deafen => ("deafen", String::new()),
            HotkeyAction::Clip(id) => ("", id.to_string()),
        };
        for (k, v) in self.hotkeys.iter_mut() {
            if k != name && taken(v) {
                v.clear();
            }
        }
        let url = self
            .clip_hotkeys
            .keys()
            .find(|url| same_server(url, server))
            .cloned()
            .or_else(|| self.saved_server(server).map(|s| s.url.clone()))
            .unwrap_or_else(|| server.to_string());
        let clips = self.clip_hotkeys.entry(url.clone()).or_default();
        clips.retain(|id, v| *id == clip || !taken(v));
        if !name.is_empty() {
            self.hotkeys.insert(name.into(), combo.into());
        } else if combo.is_empty() {
            clips.remove(&clip);
        } else {
            clips.insert(clip, combo.into());
        }
        if self.clip_hotkeys.get(&url).is_some_and(|m| m.is_empty()) {
            self.clip_hotkeys.remove(&url);
        }
    }
}

/// What a global hotkey does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    Mute,
    Deafen,
    /// Plays a soundboard clip, by id, on the server it belongs to.
    Clip(i64),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CustomTheme {
    pub bg: String,
    pub surface: String,
    pub text: String,
    pub accent: String,
}

/// What replaces the room behind you on camera.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Background {
    #[default]
    None,
    Blur {
        strength: u32,
    },
    /// One of the pictures that ship with Harmony, by name.
    Builtin {
        name: String,
    },
    /// A picture the person added, by path.
    Image {
        path: String,
    },
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            server_url: String::new(),
            username: String::new(),
            password: String::new(),
            session_token: String::new(),
            saved_servers: Vec::new(),
            accounts: Vec::new(),
            active_account: String::new(),
            remember_account: true,
            media_cache_mb: 512,
            resolution: String::new(),
            framerate: 0,
            priority: "sharp".into(),
            audio_input_id: String::new(),
            voice_input_id: String::new(),
            voice_output_id: String::new(),
            voice_camera_id: String::new(),
            mic_gain: 100,
            mic_sensitivity: 0,
            voice_sounds: true,
            theme: "midnight".into(),
            custom_theme: None,
            show_members: true,
            soundpad_volume: 100,
            recent_emoji: Vec::new(),
            ui_scale: 100,
            mention_sound: true,
            sound_volume: 100,
            hotkeys: BTreeMap::new(),
            clip_hotkeys: BTreeMap::new(),
            clips_enabled: false,
            muted_channels: BTreeMap::new(),
            message_sound: true,
            hardware_encoding: "auto".into(),
            gpu_preference: "auto".into(),
            window_audio_fallback: "silent".into(),
            check_updates: true,
            noise_suppression: true,
            echo_cancellation: true,
            camera_background: Background::None,
            custom_backgrounds: Vec::new(),
            language: String::new(),
            close_to_tray: true,
            tray_notice_seen: false,
            confirm_voice_join: true,
            join_muted: true,
        }
    }
}

/// `%APPDATA%\Harmony`, where Electron kept `userData`. `HARMONY_DATA_DIR` moves it, for
/// running more than one copy side by side.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HARMONY_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("Harmony")
}

pub struct Store {
    path: PathBuf,
    /// The file as read, so keys this client does not know survive a save.
    raw: Map<String, Value>,
    /// Keys whose value did not parse, with the default used instead. The file keeps what it
    /// had until the setting is actually changed.
    unread: Map<String, Value>,
    /// Each secret's sealed form, so a save (one per slider step) neither calls DPAPI again nor
    /// rewrites a secret that did not change.
    sealed: HashMap<String, String>,
    pub values: Settings,
}

/// Visits every secret in the file's shape: the server password and the sign-in, at the top and
/// in each saved server.
fn each_secret(map: &mut Map<String, Value>, f: &mut impl FnMut(&mut String)) {
    let mut visit = |m: &mut Map<String, Value>| {
        for key in ["password", "sessionToken"] {
            if let Some(Value::String(s)) = m.get_mut(key) {
                f(s);
            }
        }
    };
    visit(map);
    if let Some(Value::Array(list)) = map.get_mut("savedServers") {
        for s in list.iter_mut().filter_map(Value::as_object_mut) {
            visit(s);
        }
    }
}

/// The file with its secrets opened, and whether any was still unsealed. One that does not open
/// (sealed by another Windows user, or damaged) reads as empty: signing in again replaces it.
fn open_secrets(raw: &Map<String, Value>, sealed: &mut HashMap<String, String>) -> (Map<String, Value>, bool) {
    let mut opened = raw.clone();
    let mut unsealed = false;
    each_secret(&mut opened, &mut |s| {
        if !secret::is_sealed(s) {
            unsealed |= !s.is_empty();
            return;
        }
        match secret::open(s) {
            Some(plain) => {
                sealed.insert(plain.clone(), std::mem::take(s));
                *s = plain;
            }
            None => {
                log::warn!("settings.json: a saved password or sign-in did not open; it is dropped");
                s.clear();
            }
        }
    });
    (opened, unsealed)
}

/// Reads each known key on its own, so one bad value costs that setting and not the rest.
fn read_leniently(raw: &Map<String, Value>) -> (Settings, Map<String, Value>) {
    let Ok(Value::Object(mut merged)) = serde_json::to_value(Settings::default()) else { return Default::default() };
    let mut unread = Map::new();
    for (k, v) in raw {
        let Some(default) = merged.insert(k.clone(), v.clone()) else {
            merged.remove(k);
            continue;
        };
        if serde_json::from_value::<Settings>(Value::Object(merged.clone())).is_err() {
            log::warn!("settings.json: {k} did not parse; using the default");
            merged.insert(k.clone(), default.clone());
            unread.insert(k.clone(), default);
        }
    }
    (serde_json::from_value(Value::Object(merged)).unwrap_or_default(), unread)
}

impl Store {
    pub fn load() -> Store {
        Store::load_from(data_dir().join("settings.json"))
    }

    pub fn load_from(path: PathBuf) -> Store {
        let raw: Map<String, Value> = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let mut sealed = HashMap::new();
        let (opened, unsealed) = open_secrets(&raw, &mut sealed);
        let (mut values, unread) = read_leniently(&opened);
        values.ui_scale = values.ui_scale.clamp(70, 180);
        values.sound_volume = values.sound_volume.min(200);
        if values.framerate == 0 {
            values.framerate = 30;
        }
        // A file from before the rail knew one server; it becomes the first saved one.
        if values.saved_servers.is_empty() && !values.server_url.trim().is_empty() {
            values.saved_servers.push(SavedServer {
                url: values.server_url.clone(),
                password: values.password.clone(),
                username: values.username.clone(),
                session_token: values.session_token.clone(),
                ..Default::default()
            });
        }
        // A file from before accounts gets one per username it signed in with.
        values.tidy_accounts();
        // What a save would write for the unread keys, so they are left alone until changed.
        let unread = match serde_json::to_value(&values) {
            Ok(Value::Object(now)) => unread.keys().filter_map(|k| Some((k.clone(), now.get(k)?.clone()))).collect(),
            _ => unread,
        };
        let mut store = Store { path, raw, unread, sealed, values };
        // Secrets an older client left in the clear are not left there until some setting changes.
        if unsealed && secret::SEALS {
            store.save();
        }
        store
    }

    pub fn save(&mut self) {
        self.values.sync_active();
        let Value::Object(fresh) = serde_json::to_value(&self.values).unwrap_or(Value::Null) else { return };
        // Compared with `unread` as it is, in the clear, and written sealed.
        let mut stored = fresh.clone();
        let sealed = &mut self.sealed;
        each_secret(&mut stored, &mut |s| {
            if !s.is_empty() {
                *s = sealed.entry(std::mem::take(s)).or_insert_with_key(|plain| secret::seal(plain)).clone();
            }
        });
        for (k, v) in fresh {
            if self.unread.get(&k) == Some(&v) {
                continue;
            }
            self.unread.remove(&k);
            if let Some(v) = stored.remove(&k) {
                self.raw.insert(k, v);
            }
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = serde_json::to_string_pretty(&Value::Object(self.raw.clone())).unwrap_or_default();
        // Write beside it and rename, so a crash mid-write never leaves half a file.
        let tmp = self.path.with_extension("json.part");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    pub fn update(&mut self, f: impl FnOnce(&mut Settings)) {
        f(&mut self.values);
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_survive_a_save() {
        let dir = std::env::temp_dir().join(format!("harmony-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"serverUrl":"pi.local:8080","quality":"high","uiScale":400}"#).unwrap();
        let mut store = Store::load_from(path.clone());
        assert_eq!(store.values.server_url, "pi.local:8080");
        assert_eq!(store.values.ui_scale, 180);
        store.update(|s| s.username = "predo".into());
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["quality"], "high");
        assert_eq!(back["username"], "predo");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn scratch(name: &str, contents: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("harmony-settings-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    fn server(url: &str, user: &str, token: &str, name: &str) -> SavedServer {
        SavedServer { url: url.into(), username: user.into(), session_token: token.into(), name: name.into(), ..Default::default() }
    }

    #[test]
    fn one_server_files_become_the_first_saved_server() {
        let (dir, path) = scratch(
            "migrate",
            r#"{"serverUrl":"pi.local:8080","username":"predo","sessionToken":"tok","password":"door","quality":"high"}"#,
        );
        let mut store = Store::load_from(path.clone());
        assert_eq!(store.values.saved_servers.len(), 1);
        let s = &store.values.saved_servers[0];
        assert_eq!(
            (s.url.as_str(), s.username.as_str(), s.session_token.as_str(), s.password.as_str()),
            ("pi.local:8080", "predo", "tok", "door")
        );
        assert_eq!(store.values.active_server(), Some(0));
        store.save();
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["savedServers"][0]["url"], "pi.local:8080");
        assert_eq!(back["serverUrl"], "pi.local:8080");
        assert_eq!(back["quality"], "high");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bad_value_costs_only_its_own_setting() {
        let (dir, path) = scratch(
            "lenient",
            r#"{"serverUrl":"pi.local:8080","username":"predo","sessionToken":"tok","theme":"onyx","uiScale":1.5,"micGain":null,"framerate":"x"}"#,
        );
        let mut store = Store::load_from(path.clone());
        let v = &store.values;
        assert_eq!(
            (v.server_url.as_str(), v.username.as_str(), v.session_token.as_str(), v.theme.as_str()),
            ("pi.local:8080", "predo", "tok", "onyx")
        );
        assert_eq!((v.ui_scale, v.mic_gain, v.framerate), (100, 100, 30));
        store.save();
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((back["uiScale"].clone(), back["micGain"].clone(), back["framerate"].clone()), (1.5.into(), Value::Null, "x".into()));
        assert_eq!(back["sessionToken"].as_str().and_then(secret::open).as_deref(), Some("tok"));
        store.update(|s| s.mic_gain = 80);
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((back["micGain"].clone(), back["uiScale"].clone()), (80.into(), 1.5.into()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_top_level_keys_follow_the_active_server() {
        let mut s = Settings::default();
        s.remember_server(server("pi.local:8080", "predo", "a", "Pi"));
        s.remember_server(server("http://box:9000/", "lis", "b", "Box"));
        assert_eq!((s.server_url.as_str(), s.username.as_str(), s.session_token.as_str()), ("http://box:9000/", "lis", "b"));
        assert!(s.switch_server("http://pi.local:8080", "predo"));
        assert!(!s.switch_server("pi.local:8080", "lis"), "lis has no account there");
        assert_eq!((s.username.as_str(), s.session_token.as_str()), ("predo", "a"));
        // Signing in again refreshes the entry where it is.
        s.remember_server(server("PI.local:8080/", "predo", "c", "Pi renamed"));
        assert_eq!(s.saved_servers.len(), 2);
        assert_eq!(s.saved_servers[0].name, "Pi renamed");
        // Signing out through the old keys reaches the saved server.
        s.session_token.clear();
        s.sync_active();
        assert_eq!(s.saved_servers[0].session_token, "");
        assert_eq!(s.saved_servers[1].session_token, "b");
        s.move_server("box:9000", "lis", 0);
        assert_eq!(s.saved_servers[0].name, "Box");
        s.forget_token("box:9000", "lis");
        assert_eq!(s.saved_servers[0].session_token, "");
        assert_eq!(s.forget_server("pi.local:8080", "predo").map(|s| s.name), Some("Pi renamed".into()));
        assert_eq!((s.server_url.as_str(), s.username.as_str()), ("", ""));
        assert_eq!(s.active_server(), None);
    }

    fn names(s: &Settings) -> Vec<&str> {
        s.accounts.iter().map(|a| a.username.as_str()).collect()
    }

    #[test]
    fn saved_servers_from_before_accounts_are_grouped_by_username() {
        let (dir, path) = scratch(
            "accounts",
            r#"{"serverUrl":"box:9000","username":"lis","sessionToken":"t3","quality":"high",
                "savedServers":[{"url":"pi.local:8080","username":"predo","sessionToken":"t1","name":"Pi"},
                                {"url":"pi.local:8080","username":"lis","sessionToken":"t2","name":"Pi"},
                                {"url":"box:9000","username":"lis","sessionToken":"t3","name":"Box"},
                                {"url":"home:80","username":"predo","sessionToken":"t4","name":"Home"}]}"#,
        );
        let mut store = Store::load_from(path.clone());
        let v = &store.values;
        assert_eq!(names(v), ["predo", "lis"]);
        assert_eq!(v.active_account, "lis", "the account of the server in use");
        assert_eq!(v.account_servers("predo").map(|(i, s)| (i, s.name.as_str())).collect::<Vec<_>>(), [(0, "Pi"), (3, "Home")]);
        assert_eq!(v.account_home("predo").as_deref(), Some("pi.local:8080"));
        store.save();
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["accounts"][1]["username"], "lis");
        assert_eq!(back["activeAccount"], "lis");
        assert_eq!(back["quality"], "high");
        let again = Store::load_from(path);
        assert_eq!(again.values, store.values);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bad_accounts_list_is_rebuilt_from_the_servers() {
        let (dir, path) = scratch(
            "accounts-bad",
            r#"{"serverUrl":"pi.local:8080","username":"predo","accounts":{"predo":1},"activeAccount":7,
                "savedServers":[{"url":"pi.local:8080","username":"predo"}]}"#,
        );
        let mut store = Store::load_from(path.clone());
        assert_eq!(names(&store.values), ["predo"]);
        assert_eq!(store.values.active_account, "predo");
        store.update(|s| s.note_identity("predo", "pi.local:8080", "Predo", "abc"));
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["accounts"][0]["displayName"], "Predo");
        assert_eq!(back["activeAccount"], 7, "left as it was until changed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn accounts_follow_their_servers() {
        let mut s = Settings::default();
        s.remember_server(server("pi.local:8080", "predo", "a", "Pi"));
        s.remember_server(server("box:9000", "predo", "b", "Box"));
        // The same server as another name is another account, not a replacement.
        s.remember_server(server("pi.local:8080", "lis", "c", "Pi"));
        assert_eq!(s.saved_servers.len(), 3);
        assert_eq!(names(&s), ["predo", "lis"]);
        assert_eq!(s.active_account, "lis");
        assert_eq!(s.saved_server_as("pi.local:8080", "predo").map(|s| s.session_token.as_str()), Some("a"));

        // Switching to an account goes back to where it was last.
        assert!(s.switch_server("box:9000", "predo"));
        assert_eq!((s.active_account.as_str(), s.account_home("predo").as_deref()), ("predo", Some("box:9000")));
        assert!(s.switch_server("pi.local:8080", "lis"));
        assert_eq!(s.account_home("predo").as_deref(), Some("box:9000"));

        // What a server says about itself reaches every account's copy of it.
        assert!(!s.knows_server("http://pi.local:8080/", "Pi 2", "ff"));
        s.note_server("http://pi.local:8080/", "Pi 2", "ff");
        assert!(s.knows_server("pi.local:8080", "Pi 2", "ff"));
        assert!(s.saved_servers.iter().filter(|s| s.url == "pi.local:8080").all(|s| s.name == "Pi 2"));
        assert!(!s.knows_identity("predo", "box:9000", "Predo", "aa"));
        s.note_identity("predo", "box:9000", "Predo", "aa");
        assert!(s.knows_identity("predo", "box:9000", "Predo", "aa"));
        // A server without a picture keeps the one another server showed; the one that showed it
        // can take it away.
        assert!(s.knows_identity("predo", "pi.local:8080", "Predo", ""));
        assert!(!s.knows_identity("predo", "pi.local:8080", "Predo P.", ""));
        s.note_identity("predo", "pi.local:8080", "Predo P.", "");
        assert_eq!(s.account("predo").map(|a| (a.name(), a.avatar.as_str())), Some(("Predo P.", "aa")));
        s.note_identity("predo", "box:9000", "Predo", "");
        assert_eq!(s.account("predo").map(|a| (a.avatar.as_str(), a.avatar_server.as_str())), Some(("", "")));

        // Signing out of the account in use takes its servers and leaves another account active.
        let gone = s.forget_account("lis");
        assert_eq!(gone.iter().map(|g| g.session_token.as_str()).collect::<Vec<_>>(), ["c"]);
        assert_eq!(names(&s), ["predo"]);
        assert_eq!((s.server_url.as_str(), s.session_token.as_str(), s.active_account.as_str()), ("", "", "predo"));
        s.switch_server("pi.local:8080", "predo");

        // Removing an account's last server removes the account.
        s.forget_server("pi.local:8080", "predo");
        assert_eq!(s.active_account, "predo");
        s.forget_server("box:9000", "predo");
        assert!(s.accounts.is_empty() && s.active_account.is_empty());
    }

    #[test]
    fn secrets_are_sealed_on_disk_and_read_back() {
        let (dir, path) = scratch(
            "sealed",
            r#"{"serverUrl":"pi.local:8080","username":"predo","sessionToken":"tok","password":"door","quality":"high",
                "savedServers":[{"url":"pi.local:8080","username":"predo","sessionToken":"tok","password":"door"},
                                {"url":"box:9000","username":"lis","sessionToken":"tok2","password":"","name":"Box"}]}"#,
        );
        let store = Store::load_from(path.clone());
        let v = &store.values;
        assert_eq!((v.session_token.as_str(), v.password.as_str()), ("tok", "door"));
        assert_eq!((v.saved_servers[1].session_token.as_str(), v.saved_servers[1].password.as_str()), ("tok2", ""));

        // Sealed the moment it was read, without waiting for a setting to change.
        let text = std::fs::read_to_string(&path).unwrap();
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["quality"], "high");
        assert_eq!(back["savedServers"][1]["password"], "");
        if secret::SEALS {
            assert!(!text.contains("\"tok") && !text.contains("\"door"), "{text}");
            assert!(secret::is_sealed(back["sessionToken"].as_str().unwrap()));
        }

        // Read back, and saved again without sealing anew.
        let mut again = Store::load_from(path.clone());
        assert_eq!(again.values, store.values);
        again.update(|s| s.mic_gain = 80);
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["sessionToken"], back["sessionToken"]);
        assert_eq!(after["savedServers"][1]["sessionToken"], back["savedServers"][1]["sessionToken"]);
        again.update(|s| s.session_token = "new".into());
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["savedServers"][0]["sessionToken"].as_str().and_then(secret::open).as_deref(), Some("new"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_secret_that_does_not_open_costs_only_itself() {
        let (dir, path) = scratch(
            "unopened",
            r#"{"serverUrl":"pi.local:8080","username":"predo","sessionToken":"dpapi:00ff","password":"door","theme":"onyx"}"#,
        );
        let store = Store::load_from(path);
        let v = &store.values;
        assert_eq!((v.session_token.as_str(), v.password.as_str(), v.theme.as_str()), ("", "door", "onyx"));
        assert_eq!(v.saved_servers[0].session_token, "");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hotkeys_load_from_the_old_clients_file_and_a_bad_one_costs_only_itself() {
        let (dir, path) = scratch(
            "hotkeys",
            r#"{"serverUrl":"pi.local:8080","soundVolume":350,"hotkeys":{"mute":"Ctrl+Shift+M","deafen":"","pushToTalk":"F13"},
                "clipHotkeys":{"pi.local:8080":{"7":"Alt+1","9":"F7"}}}"#,
        );
        let mut store = Store::load_from(path.clone());
        let v = &store.values;
        assert_eq!(v.sound_volume, 200);
        assert_eq!(v.hotkey("http://pi.local:8080/", HotkeyAction::Mute), "Ctrl+Shift+M");
        assert_eq!(v.hotkey("", HotkeyAction::Deafen), "");
        assert_eq!(v.hotkey("PI.local:8080", HotkeyAction::Clip(7)), "Alt+1");
        assert_eq!(v.hotkey("box:9000", HotkeyAction::Clip(7)), "", "clips belong to their server");
        store.update(|s| s.bind_hotkey("pi.local:8080", HotkeyAction::Deafen, "F7"));
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["hotkeys"]["pushToTalk"], "F13", "actions this client does not know are kept");
        assert_eq!(back["clipHotkeys"]["pi.local:8080"], serde_json::json!({ "7": "Alt+1" }));
        let _ = std::fs::remove_dir_all(dir);

        let (dir, path) = scratch("hotkeys-bad", r#"{"theme":"onyx","hotkeys":["Ctrl+M"],"clipHotkeys":{"pi":{"7":3}}}"#);
        let mut store = Store::load_from(path.clone());
        assert_eq!(store.values.theme, "onyx");
        assert!(store.values.hotkeys.is_empty() && store.values.clip_hotkeys.is_empty());
        store.update(|s| s.mic_gain = 90);
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["hotkeys"], serde_json::json!(["Ctrl+M"]), "left as it was until changed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn one_combination_does_one_thing() {
        let mut s = Settings::default();
        s.remember_server(server("http://pi.local:8080", "predo", "a", "Pi"));
        let pi = "pi.local:8080";
        s.bind_hotkey(pi, HotkeyAction::Mute, "Ctrl+Alt+M");
        s.bind_hotkey(pi, HotkeyAction::Clip(3), "Ctrl+Alt+D");
        s.bind_hotkey("box:9000", HotkeyAction::Clip(3), "Ctrl+Alt+B");
        // Keyed by the address the server was saved under.
        assert!(s.clip_hotkeys.contains_key("http://pi.local:8080"));
        assert_eq!(s.hotkey(pi, HotkeyAction::Clip(3)), "Ctrl+Alt+D");
        assert_eq!(s.hotkey("box:9000", HotkeyAction::Clip(3)), "Ctrl+Alt+B");

        // Taking a clip's combination moves it off the clip, and a mute's off mute.
        s.bind_hotkey(pi, HotkeyAction::Deafen, "ctrl+alt+d");
        assert_eq!(s.hotkey(pi, HotkeyAction::Clip(3)), "");
        s.bind_hotkey(pi, HotkeyAction::Clip(4), "Ctrl+Alt+M");
        assert_eq!((s.hotkey(pi, HotkeyAction::Mute), s.hotkey(pi, HotkeyAction::Clip(4))), ("", "Ctrl+Alt+M"));
        // Another server's clips are left alone: they are not registered while this one is open.
        assert_eq!(s.hotkey("box:9000", HotkeyAction::Clip(3)), "Ctrl+Alt+B");

        s.bind_hotkey(pi, HotkeyAction::Clip(4), "");
        assert!(s.server_clip_hotkeys(pi).is_none(), "an emptied server is dropped");
        s.bind_hotkey(pi, HotkeyAction::Deafen, "");
        assert_eq!(s.hotkey(pi, HotkeyAction::Deafen), "");
    }

    #[test]
    fn backgrounds_round_trip() {
        let b = Background::Blur { strength: 12 };
        let s = serde_json::to_string(&b).unwrap();
        assert_eq!(s, r#"{"kind":"blur","strength":12}"#);
        assert_eq!(serde_json::from_str::<Background>(&s).unwrap(), b);
    }

    #[test]
    fn a_muted_channel_belongs_to_its_server() {
        let mut s = Settings::default();
        s.set_channel_muted("https://a.example:8444", 3, true);
        assert!(s.channel_muted("https://A.example:8444/", 3), "however the address was typed");
        assert!(!s.channel_muted("https://b.example", 3), "channel 3 elsewhere is another channel");
        s.set_channel_muted("https://a.example:8444", 3, true);
        assert_eq!(s.muted_channels.values().flatten().count(), 1, "muting twice is once");
        s.set_channel_muted("https://a.example:8444", 3, false);
        assert!(s.muted_channels.is_empty(), "nothing left behind for a server with none");
    }
}
