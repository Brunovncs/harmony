//! Your private-conversation keys on this computer: one small file per server and account, under
//! `keys\` beside the settings. The identity key and the recovery key in it are sealed with DPAPI
//! for this Windows user (see `secret`), like the passwords in the settings file.
//!
//! Also kept here, because they are facts about what this computer has seen rather than settings:
//! the key each person had the first time you talked to them (to notice when it changes) and the
//! ones you have checked with them in person (the safety number).
//!
//! A file of its own, not a part of `settings.json`: that file is rewritten on every slider step,
//! and this one should be written only when a key actually changes.

use super::api::normalize_base;
use super::crypto::{Identity, RecoveryKey};
use super::secret;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use zeroize::Zeroizing;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
struct Data {
    /// Which server and account this is, for a person looking in the folder.
    server: String,
    username: String,
    key_id: i64,
    /// The identity key's private half, hex, sealed.
    secret: String,
    /// The recovery key as shown, sealed, so it can be shown again from the settings.
    recovery: String,
    /// The person said they put the recovery key somewhere safe.
    recovery_saved: bool,
    /// By user id: the public key first seen for them.
    pins: BTreeMap<i64, String>,
    /// By user id: the public key whose safety number they compared with you.
    verified: BTreeMap<i64, String>,
}

pub struct KeyFile {
    path: PathBuf,
    data: Data,
}

impl KeyFile {
    pub fn open(server: &str, username: &str) -> KeyFile {
        let digest = Sha256::digest(format!("{}\n{}", normalize_base(server).to_lowercase(), username.to_lowercase()));
        let path = super::settings::data_dir().join("keys").join(format!("{}.json", hex::encode(&digest[..12])));
        let mut file = KeyFile::open_at(path);
        file.data.server = normalize_base(server);
        file.data.username = username.to_string();
        file
    }

    pub fn open_at(path: PathBuf) -> KeyFile {
        let data = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        KeyFile { path, data }
    }

    /// The identity key and its id on the server, if this computer has it and it opens.
    pub fn identity(&self) -> Option<(i64, Identity)> {
        if self.data.key_id == 0 {
            return None;
        }
        let hexed = Zeroizing::new(secret::open(&self.data.secret)?);
        let bytes = Zeroizing::new(hex::decode(hexed.as_str()).ok()?);
        let secret: [u8; 32] = bytes.as_slice().try_into().ok()?;
        Some((self.data.key_id, Identity::from_secret(Zeroizing::new(secret))))
    }

    pub fn set_identity(&mut self, key_id: i64, identity: &Identity) {
        self.data.key_id = key_id;
        self.data.secret = secret::seal(&Zeroizing::new(hex::encode(identity.secret_bytes().as_slice())));
        self.save();
    }

    /// Forgets the identity key here, as when the server says this account has a newer one.
    pub fn forget_identity(&mut self) {
        self.data.key_id = 0;
        self.data.secret.clear();
        self.data.recovery.clear();
        self.data.recovery_saved = false;
        self.save();
    }

    pub fn recovery(&self) -> Option<RecoveryKey> {
        RecoveryKey::parse(&Zeroizing::new(secret::open(&self.data.recovery)?))
    }

    pub fn set_recovery(&mut self, recovery: &RecoveryKey, saved: bool) {
        self.data.recovery = secret::seal(&Zeroizing::new(recovery.display()));
        self.data.recovery_saved = saved;
        self.save();
    }

    pub fn recovery_saved(&self) -> bool {
        self.data.recovery_saved || self.data.recovery.is_empty()
    }

    pub fn mark_recovery_saved(&mut self) {
        self.data.recovery_saved = true;
        self.save();
    }

    pub fn pin(&self, user: i64) -> Option<&str> {
        self.data.pins.get(&user).map(String::as_str)
    }

    pub fn set_pin(&mut self, user: i64, key: &str) {
        if self.data.pins.get(&user).map(String::as_str) != Some(key) {
            self.data.pins.insert(user, key.to_string());
            self.save();
        }
    }

    pub fn verified(&self, user: i64) -> Option<&str> {
        self.data.verified.get(&user).map(String::as_str)
    }

    pub fn set_verified(&mut self, user: i64, key: Option<&str>) {
        match key {
            Some(k) => self.data.verified.insert(user, k.to_string()),
            None => self.data.verified.remove(&user),
        };
        self.save();
    }

    /// Written whole to a file beside it, then moved over it, so a crash halfway never leaves a
    /// key file that does not read.
    fn save(&self) {
        let Ok(json) = serde_json::to_vec_pretty(&self.data) else { return };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let temp = self.path.with_extension("json.part");
        let written = std::fs::write(&temp, json).and_then(|_| std::fs::rename(&temp, &self.path));
        if let Err(e) = written {
            log::warn!("could not save the private-message keys: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file() -> PathBuf {
        std::env::temp_dir().join(format!("harmony-keys-{}.json", hex::encode(super::super::crypto::random::<8>())))
    }

    #[test]
    fn keys_survive_a_restart_and_stay_sealed_on_disk() {
        let path = temp_file();
        let id = Identity::generate();
        let r = RecoveryKey::generate();
        {
            let mut f = KeyFile::open_at(path.clone());
            assert!(f.identity().is_none());
            f.set_identity(42, &id);
            f.set_recovery(&r, false);
            f.set_pin(7, "abc");
        }
        let f = KeyFile::open_at(path.clone());
        let (key_id, back) = f.identity().unwrap();
        assert_eq!(key_id, 42);
        assert_eq!(back.public(), id.public());
        assert_eq!(f.recovery().unwrap().display(), r.display());
        assert!(!f.recovery_saved());
        assert_eq!(f.pin(7), Some("abc"));
        let on_disk = std::fs::read_to_string(&path).unwrap();
        if secret::SEALS {
            assert!(!on_disk.contains(&r.display()));
            assert!(!on_disk.contains(&hex::encode(id.secret_bytes().as_slice())));
        }
        let mut f = f;
        f.forget_identity();
        assert!(KeyFile::open_at(path.clone()).identity().is_none());
        let _ = std::fs::remove_file(path);
    }
}
