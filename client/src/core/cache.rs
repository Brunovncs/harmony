//! A content-addressed disk cache for avatars, attachments, emoji, soundpad clips and server
//! pictures. Every upload is named by its sha256, so a file is fetched once, checked against its
//! name, and kept until the budget forces it out (least recently used first, never anything in
//! the keep set).

use super::api::{Api, ApiError};
use super::types::ErrorCode;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    content_type: String,
    bytes: u64,
    used_at: u64,
}

struct Inner {
    index: HashMap<String, Entry>,
    keep: HashSet<String>,
    /// A file came or went: the index goes to disk at the next flush.
    dirty: bool,
    /// Only recency changed, which is not worth a write every few seconds; it goes at exit.
    touched: bool,
    budget: u64,
}

type Flights = Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>;

#[derive(Clone)]
pub struct Cache {
    dir: PathBuf,
    inner: Arc<Mutex<Inner>>,
    /// One download per hash at a time.
    flights: Flights,
    /// One index write at a time.
    writing: Arc<Mutex<()>>,
}

pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn budget(mb: u64) -> u64 {
    mb.max(32) * 1024 * 1024
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Takes a hash out of the in-flight map when its download ends, however it ends.
struct Flight<'a> {
    flights: &'a Flights,
    hash: &'a str,
    lock: Arc<AsyncMutex<()>>,
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut flights = self.flights.lock();
        if flights.get(self.hash).is_some_and(|l| Arc::ptr_eq(l, &self.lock)) {
            flights.remove(self.hash);
        }
    }
}

impl Cache {
    /// Opens the cache, then checks the files against the index on a thread of its own.
    pub fn open(dir: PathBuf, budget_mb: u64) -> Cache {
        let cache = Cache::load(dir, budget_mb);
        let c = cache.clone();
        if let Err(e) = std::thread::Builder::new().name("harmony-cache".into()).spawn(move || c.reconcile()) {
            log::warn!("cache check did not start: {e}");
        }
        cache
    }

    fn load(dir: PathBuf, budget_mb: u64) -> Cache {
        let _ = std::fs::create_dir_all(&dir);
        let index = match std::fs::read_to_string(dir.join("index.json")) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                log::warn!("cache index did not parse, rebuilding it from the files: {e}");
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        Cache {
            dir,
            inner: Arc::new(Mutex::new(Inner { index, keep: HashSet::new(), dirty: false, touched: false, budget: budget(budget_mb) })),
            flights: Default::default(),
            writing: Default::default(),
        }
    }

    fn path(&self, hash: &str) -> PathBuf {
        self.dir.join(&hash[0..2]).join(&hash[2..4]).join(hash)
    }

    /// Hashes that stay whatever the budget says: avatars, emoji, soundpad clips.
    pub fn set_keep(&self, keep: impl IntoIterator<Item = String>) {
        self.inner.lock().keep = keep.into_iter().collect();
    }

    /// The cache every server shares, opened the first time it is asked for: a second one over the
    /// same files would have two indexes overwriting each other. Later calls only move the budget.
    pub fn shared(budget_mb: u64) -> Cache {
        static SHARED: OnceLock<Cache> = OnceLock::new();
        let cache = SHARED.get_or_init(|| Cache::open(super::settings::data_dir().join("media"), budget_mb));
        cache.inner.lock().budget = budget(budget_mb);
        cache.clone()
    }

    /// The bytes and content type, from disk or else from the server.
    pub async fn get(&self, api: &Api, hash: &str) -> Result<(Arc<Vec<u8>>, String), ApiError> {
        self.fetch(hash, || api.download(hash)).await
    }

    /// As `get`, with `download` fetching the file when it is not here yet.
    pub async fn fetch<F>(&self, hash: &str, download: impl FnOnce() -> F) -> Result<(Arc<Vec<u8>>, String), ApiError>
    where
        F: Future<Output = Result<(Vec<u8>, String), ApiError>>,
    {
        if !is_hash(hash) {
            return Err(ApiError {
                status: 0,
                code: ErrorCode::BadHash,
                message: tr!("Not a file name.", "Não é um nome de arquivo.").into(),
                body: Default::default(),
            });
        }
        if let Some(hit) = self.read(hash) {
            return Ok(hit);
        }
        let lock = self.flights.lock().entry(hash.to_string()).or_default().clone();
        let _flight = Flight { flights: &self.flights, hash, lock: lock.clone() };
        let _guard = lock.lock().await;
        if let Some(hit) = self.read(hash) {
            return Ok(hit);
        }
        let (bytes, ct) = download().await?;
        let got = hex::encode(Sha256::digest(&bytes));
        if got != hash {
            return Err(ApiError {
                status: 0,
                code: ErrorCode::Corrupt,
                message: tr!("The file arrived damaged.", "O arquivo chegou corrompido.").into(),
                body: Default::default(),
            });
        }
        self.write(hash, &bytes, &ct);
        Ok((Arc::new(bytes), ct))
    }

    pub fn read(&self, hash: &str) -> Option<(Arc<Vec<u8>>, String)> {
        let ct = {
            let mut inner = self.inner.lock();
            let e = inner.index.get_mut(hash)?;
            e.used_at = now();
            let ct = e.content_type.clone();
            inner.touched = true;
            ct
        };
        match std::fs::read(self.path(hash)) {
            Ok(b) => Some((Arc::new(b), ct)),
            Err(_) => {
                let mut inner = self.inner.lock();
                inner.index.remove(hash);
                inner.dirty = true;
                None
            }
        }
    }

    /// Stores bytes we already have, e.g. right after uploading them.
    pub fn insert(&self, bytes: &[u8], content_type: &str) -> String {
        let hash = hex::encode(Sha256::digest(bytes));
        self.write(&hash, bytes, content_type);
        hash
    }

    fn write(&self, hash: &str, bytes: &[u8], ct: &str) {
        let path = self.path(hash);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let part = path.with_extension("part");
        if std::fs::write(&part, bytes).is_ok() && std::fs::rename(&part, &path).is_ok() {
            let mut inner = self.inner.lock();
            inner.index.insert(hash.to_string(), Entry { content_type: ct.to_string(), bytes: bytes.len() as u64, used_at: now() });
            inner.dirty = true;
        }
        self.evict();
    }

    fn evict(&self) {
        let victims: Vec<String> = {
            let inner = self.inner.lock();
            let mut total: u64 = inner.index.values().map(|e| e.bytes).sum();
            if total <= inner.budget {
                return;
            }
            let mut by_age: Vec<(&String, &Entry)> = inner.index.iter().filter(|(h, _)| !inner.keep.contains(*h)).collect();
            by_age.sort_by_key(|(_, e)| e.used_at);
            let mut out = Vec::new();
            for (h, e) in by_age {
                if total <= inner.budget {
                    break;
                }
                total -= e.bytes;
                out.push(h.clone());
            }
            out
        };
        for h in victims {
            let _ = std::fs::remove_file(self.path(&h));
            let mut inner = self.inner.lock();
            inner.index.remove(&h);
            inner.dirty = true;
        }
    }

    /// Brings the index back in step with the files after a crash or a lost index: files it
    /// never heard of are adopted (so the budget sees them), leftovers of interrupted writes are
    /// deleted, and entries whose file is gone are dropped.
    fn reconcile(&self) {
        let found = scan(&self.dir);
        {
            let mut inner = self.inner.lock();
            let on_disk: HashSet<&str> = found.iter().map(|(h, ..)| h.as_str()).collect();
            // Checked again now, not trusted from the scan: a download may have landed since.
            let gone: Vec<String> =
                inner.index.keys().filter(|h| !on_disk.contains(h.as_str()) && !self.path(h).exists()).cloned().collect();
            let mut changed = !gone.is_empty();
            for h in gone {
                inner.index.remove(&h);
            }
            for (h, bytes, at) in &found {
                if !inner.index.contains_key(h) {
                    inner.index.insert(h.clone(), Entry { content_type: "application/octet-stream".into(), bytes: *bytes, used_at: *at });
                    changed = true;
                }
            }
            inner.dirty |= changed;
        }
        self.evict();
    }

    /// Writes the index if a file came or went. Called every few seconds, off the UI thread.
    pub fn flush(&self) {
        self.write_index(false);
    }

    /// At exit: also writes when only recency changed.
    pub fn flush_all(&self) {
        self.write_index(true);
    }

    fn write_index(&self, with_reads: bool) {
        let _one = self.writing.lock();
        let text = {
            let mut inner = self.inner.lock();
            if !(inner.dirty || with_reads && inner.touched) {
                return;
            }
            inner.dirty = false;
            inner.touched = false;
            serde_json::to_string(&inner.index).unwrap_or_default()
        };
        // Through a temporary file, so a crash mid-write leaves the old index rather than half a new one.
        let tmp = self.dir.join("index.json.tmp");
        if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, self.dir.join("index.json"))) {
            log::warn!("cache index not saved: {e}");
            self.inner.lock().dirty = true;
        }
    }
}

/// Every cached file under `dir` (`ab/cd/abcd…`), as (hash, size, modified), deleting the
/// `.part` files interrupted writes leave behind.
fn scan(dir: &Path) -> Vec<(String, u64, u64)> {
    let entries = |d: &Path| std::fs::read_dir(d).into_iter().flatten().flatten().collect::<Vec<_>>();
    let mut out = Vec::new();
    for a in entries(dir).into_iter().filter(|e| e.path().is_dir()) {
        for b in entries(&a.path()).into_iter().filter(|e| e.path().is_dir()) {
            for f in entries(&b.path()) {
                let path = f.path();
                let name = f.file_name().to_string_lossy().to_string();
                if path.extension().is_some_and(|x| x == "part") {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                let Ok(meta) = f.metadata() else { continue };
                let prefix = format!("{}{}", a.file_name().to_string_lossy(), b.file_name().to_string_lossy());
                if meta.is_file() && is_hash(&name) && name.starts_with(&prefix) {
                    let at =
                        meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0);
                    out.push((name, meta.len(), at));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("harmony-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn reads_do_not_dirty_the_index_and_writes_go_through_a_temporary_file() {
        let dir = temp_dir("flush");
        let cache = Cache::load(dir.clone(), 64);
        let hash = cache.insert(b"hello", "text/plain");
        cache.flush();
        assert!(dir.join("index.json").exists() && !dir.join("index.json.tmp").exists());
        assert!(cache.read(&hash).is_some());
        let inner = cache.inner.lock();
        assert!(!inner.dirty && inner.touched);
        drop(inner);
        let again = Cache::load(dir.clone(), 64);
        assert_eq!(again.read(&hash).map(|(b, ct)| (b.to_vec(), ct)), Some((b"hello".to_vec(), "text/plain".to_string())));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn files_the_index_lost_are_adopted_and_leftovers_cleared() {
        let dir = temp_dir("reconcile");
        let cache = Cache::load(dir.clone(), 64);
        let kept = cache.insert(b"kept", "image/png");
        let orphan = cache.insert(b"orphan", "image/png");
        let part = cache.path(&kept).with_extension("part");
        std::fs::write(&part, b"half").unwrap();
        std::fs::write(dir.join("index.json"), "{ not json").unwrap();
        std::fs::remove_file(cache.path(&kept)).unwrap();

        let reopened = Cache::load(dir.clone(), 64);
        assert!(reopened.inner.lock().index.is_empty());
        reopened.inner.lock().index.insert(kept.clone(), Entry { content_type: "image/png".into(), bytes: 4, used_at: 0 });
        reopened.reconcile();
        let inner = reopened.inner.lock();
        assert!(inner.index.contains_key(&orphan) && !inner.index.contains_key(&kept) && inner.dirty);
        assert_eq!(inner.index[&orphan].bytes, 6);
        assert!(!part.exists());
        drop(inner);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_flight_entry_goes_away_even_when_the_download_fails() {
        let dir = temp_dir("flight");
        let cache = Cache::load(dir.clone(), 64);
        let hash = "a".repeat(64);
        {
            let lock = cache.flights.lock().entry(hash.clone()).or_default().clone();
            let _flight = Flight { flights: &cache.flights, hash: &hash, lock };
            assert_eq!(cache.flights.lock().len(), 1);
        }
        assert!(cache.flights.lock().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
