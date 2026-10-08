//! Whether a newer Harmony is out, asked of GitHub's API, and putting it in place of the running
//! copy. A release carries `harmony-<version>-windows-x64.exe` and that file's SHA-256; the new
//! program is downloaded next to the running one, checked, and swapped in: Windows lets a running
//! program be renamed though not overwritten, so the running copy steps aside to `harmony.exe.old`
//! and the next start deletes it.

use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const RELEASES_API: &str = "https://api.github.com/repos/Brunovncs/harmony/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/Brunovncs/harmony/releases/latest";

/// Where to ask instead of GitHub: a local server, to try an update end to end.
const URL_VAR: &str = "HARMONY_UPDATE_URL";
/// The version to claim instead of this one, to try updating from an older one.
const PRETEND_VAR: &str = "HARMONY_PRETEND_VERSION";
/// Set on the copy an update starts, while the one it replaces is still quitting.
pub const RELAUNCH_VAR: &str = "HARMONY_RELAUNCHED";

const ASSET_SUFFIX: &str = "-windows-x64.exe";

/// The latest release, as far as an update needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    /// Its page on GitHub, with the notes and every download.
    pub page: String,
    pub notes: String,
    /// The Windows program and the file holding its SHA-256, when the release has them.
    pub exe: Option<(String, String)>,
}

pub fn api_url() -> String {
    std::env::var(URL_VAR).unwrap_or_else(|_| RELEASES_API.to_string())
}

pub fn current() -> String {
    std::env::var(PRETEND_VAR).unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

fn numbers(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect()
}

/// `candidate` is a later version than `current` (4.10.0 after 4.9.2; a pre-release suffix is
/// ignored).
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (a, b) = (numbers(candidate), numbers(current));
    !a.is_empty() && a > b
}

pub fn parse(json: &[u8]) -> Result<Release, String> {
    let v: serde_json::Value = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    let tag = v["tag_name"].as_str().ok_or(tr!("the release has no tag", "a versão não tem tag"))?;
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let url_of = |suffix: &str| {
        assets
            .iter()
            .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(suffix)))
            .and_then(|a| a["browser_download_url"].as_str())
            .map(String::from)
    };
    let exe = if cfg!(windows) { url_of(ASSET_SUFFIX).zip(url_of(&format!("{ASSET_SUFFIX}.sha256"))) } else { None };
    Ok(Release {
        version: tag.trim_start_matches('v').to_string(),
        page: v["html_url"].as_str().unwrap_or(RELEASES_PAGE).to_string(),
        notes: v["body"].as_str().unwrap_or_default().trim().to_string(),
        exe,
    })
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("Harmony/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

async fn fetch(http: &reqwest::Client, url: &str) -> Result<reqwest::Response, String> {
    let res = http.get(url).header("Accept", "application/vnd.github+json").send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(trf!("{} answered {}", "{} respondeu {}", res.url().host_str().unwrap_or("?"), res.status().as_u16()));
    }
    Ok(res)
}

/// The latest release, if it is newer than this one. GitHub's "latest" is never a draft or a
/// pre-release. Runs on the network runtime.
pub async fn check() -> Result<Option<Release>, String> {
    let http = client()?;
    let res = http.get(api_url()).header("Accept", "application/vnd.github+json").send().await.map_err(|e| e.to_string())?;
    // GitHub answers 404 while the repository has no release yet: nothing to update to.
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !res.status().is_success() {
        return Err(trf!("{} answered {}", "{} respondeu {}", res.url().host_str().unwrap_or("?"), res.status().as_u16()));
    }
    let body = res.bytes().await.map_err(|e| e.to_string())?;
    let r = parse(&body)?;
    Ok(is_newer(&r.version, &current()).then_some(r))
}

/// `harmony.exe.<ext>`, beside the program.
pub fn beside(exe: &Path, ext: &str) -> PathBuf {
    let mut s = OsString::from(exe.as_os_str());
    s.push(".");
    s.push(ext);
    s.into()
}

/// Downloads the release's program to `to` and checks it against the SHA-256 published with it,
/// so a broken or swapped download is never run. `progress` hears the fraction done.
pub async fn download(r: &Release, to: &Path, mut progress: impl FnMut(f32)) -> Result<(), String> {
    let (url, sha_url) = r.exe.as_ref().ok_or(tr!("this release has no Windows program", "esta versão não tem o programa para Windows"))?;
    let http = client()?;
    let sums = fetch(&http, sha_url).await?.text().await.map_err(|e| e.to_string())?;
    let expected = sums.split_whitespace().next().unwrap_or_default().to_lowercase();
    if expected.len() != 64 {
        return Err(tr!("the checksum could not be read", "não foi possível ler o checksum").into());
    }
    let mut res = fetch(&http, url).await?;
    let total = res.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let mut hash = Sha256::new();
    let mut got = 0u64;
    let result = async {
        while let Some(chunk) = res.chunk().await.map_err(|e| e.to_string())? {
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            hash.update(&chunk);
            got += chunk.len() as u64;
            if total > 0 {
                progress(got as f32 / total as f32);
            }
        }
        file.sync_all().map_err(|e| e.to_string())?;
        if hex::encode(hash.finalize()) != expected {
            return Err(tr!("the download does not match its checksum", "o download não confere com o checksum").to_string());
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        drop(file);
        let _ = std::fs::remove_file(to);
    }
    result
}

/// Puts `new` where `exe` is, keeping the running copy as `.old` until the next start.
pub fn swap(exe: &Path, new: &Path) -> Result<(), String> {
    let old = beside(exe, "old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("{}: {e}", exe.display()))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(format!("{}: {e}", exe.display()));
    }
    Ok(())
}

/// Starts the program at `exe` again, with the arguments this one got. The new copy waits for
/// this one to quit instead of handing over to it as a second start would.
pub fn relaunch(exe: &Path) -> Result<(), String> {
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env_remove(PRETEND_VAR)
        .env(RELAUNCH_VAR, "1")
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Deletes what an update left beside the program; true when the copy it replaced was there,
/// which means this start is the first after an update. The old copy may take a moment to exit.
pub fn clean_up(exe: &Path) -> bool {
    let _ = std::fs::remove_file(beside(exe, "new"));
    let old = beside(exe, "old");
    for _ in 0..20 {
        if !old.exists() {
            return false;
        }
        if std::fs::remove_file(&old).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(is_newer("4.0.1", "4.0.0"));
        assert!(is_newer("v4.10.0", "4.9.9"));
        assert!(is_newer("5.0.0", "4.99.0"));
        assert!(!is_newer("4.0.0", "4.0.0"));
        assert!(!is_newer("3.9.9", "4.0.0"));
        assert!(!is_newer("garbage", "4.0.0"));
    }

    #[test]
    fn reads_a_release() {
        let json = br#"{"tag_name":"v4.1.0","html_url":"https://example/v4.1.0","body":" Faster joins. ","assets":[
            {"name":"harmony-4.1.0-windows-x64.exe","browser_download_url":"https://example/exe"},
            {"name":"harmony-4.1.0-windows-x64.exe.sha256","browser_download_url":"https://example/sha"}]}"#;
        let r = parse(json).unwrap();
        assert_eq!(r.version, "4.1.0");
        assert_eq!(r.page, "https://example/v4.1.0");
        assert_eq!(r.notes, "Faster joins.");
        if cfg!(windows) {
            assert_eq!(r.exe, Some(("https://example/exe".into(), "https://example/sha".into())));
        }
        let without = parse(br#"{"tag_name":"v4.0.1","assets":[]}"#).unwrap();
        assert_eq!(without.exe, None);
        assert_eq!(without.page, RELEASES_PAGE);
    }

    #[test]
    fn swaps_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("harmony-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("harmony.exe");
        std::fs::write(&exe, "old").unwrap();
        let new = beside(&exe, "new");
        std::fs::write(&new, "new").unwrap();
        swap(&exe, &new).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert!(clean_up(&exe));
        assert!(!beside(&exe, "old").exists());
        assert!(!clean_up(&exe));
        let _ = std::fs::remove_dir_all(dir);
    }
}
