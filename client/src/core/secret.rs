//! Secrets at rest. On Windows they are sealed with DPAPI for the signed-in Windows user, so a
//! copy of `settings.json` on another account or computer holds no usable password or sign-in.
//! Elsewhere they are kept as they are.

const PREFIX: &str = "dpapi:";

/// Whether `seal` really seals here.
pub const SEALS: bool = cfg!(windows);

/// What the file holds for a secret. Empty stays empty, so a missing secret looks missing; and
/// if sealing fails the value is kept as it is rather than lost with the sign-in it carries.
pub fn seal(plain: &str) -> String {
    if plain.is_empty() || !SEALS {
        return plain.to_string();
    }
    match dpapi::protect(plain.as_bytes()) {
        Some(blob) => format!("{PREFIX}{}", hex::encode(blob)),
        None => {
            log::warn!("settings: a secret could not be sealed; it is kept unsealed");
            plain.to_string()
        }
    }
}

pub fn is_sealed(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

/// The secret behind what the file holds. Anything unsealed is a value from before sealing and is
/// taken as it is; `None` when a sealed one does not open (another user's, or damaged).
pub fn open(stored: &str) -> Option<String> {
    let Some(blob) = stored.strip_prefix(PREFIX) else { return Some(stored.to_string()) };
    String::from_utf8(dpapi::unprotect(&hex::decode(blob).ok()?)?).ok()
}

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData};

    pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, true)
    }

    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, false)
    }

    fn run(data: &[u8], seal: bool) -> Option<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(data.len()).ok()?, pbData: data.as_ptr().cast_mut() };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        use std::ptr::{null, null_mut};
        // SAFETY: `input` points at `data`, alive for the call, and DPAPI only reads it. On success
        // the system allocates `out.pbData`, which is copied out and then handed back to LocalFree.
        let ok = unsafe {
            if seal {
                CryptProtectData(&input, null(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&input, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            }
        };
        if ok == 0 || out.pbData.is_null() {
            return None;
        }
        // SAFETY: as above; `out` is the system's buffer of `cbData` bytes until LocalFree.
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe { LocalFree(out.pbData.cast()) };
        Some(bytes)
    }
}

#[cfg(not(windows))]
mod dpapi {
    pub fn protect(_: &[u8]) -> Option<Vec<u8>> {
        None
    }

    pub fn unprotect(_: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_round_trip_and_old_plain_values_read_as_they_are() {
        let sealed = seal("hunter22");
        assert_eq!(open(&sealed).as_deref(), Some("hunter22"));
        assert_eq!(is_sealed(&sealed), SEALS);
        if SEALS {
            assert!(!sealed.contains("hunter22"));
        }
        assert_eq!(seal(""), "");
        assert_eq!(open("from before").as_deref(), Some("from before"));
        assert_eq!(open("dpapi:00ff"), None);
        assert_eq!(open("dpapi:not hex"), None);
    }
}
