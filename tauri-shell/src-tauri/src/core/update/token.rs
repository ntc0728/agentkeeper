//! GitHub token storage — Rust port of `core/update/token.js`.
//!
//! The token raises the API quota (60 → 5000 requests/hour) and is therefore a
//! credential: a plaintext copy in `settings.json` travels with every backup,
//! export and machine migration. Electron's `safeStorage` wraps it in DPAPI on
//! Windows / Keychain on macOS / libsecret on Linux, so the stored value is
//! useless on another machine. This port writes the *same* envelope:
//!
//! ```text
//! enc1: + base64( CryptProtectData(token_utf8) )
//! ```
//!
//! On Windows `safeStorage` is DPAPI with no entropy and user scope, so a value
//! written by the Electron build is readable here and vice versa — same user,
//! same machine. (Verified by a round-trip test; reading a value produced by a
//! *real* Electron install is only covered if such a value exists on the box —
//! see the stage report.)
//!
//! Two rules from the JS that the UI depends on:
//!
//! 1. the token is **never echoed back** — [`TokenStore::status`] reports
//!    `{filled, origin, error}` only;
//! 2. an undecryptable value is **not** reported as "not configured": saying
//!    "empty" would silently downgrade the user to the anonymous quota and hide
//!    the real problem. The reason is returned so the UI can ask for a re-save.
//!
//! If DPAPI is unavailable or refuses, [`TokenStore::set`] fails; there is no
//! plaintext fallback (`token.js:19-21`).

use base64::Engine;
use serde_json::{json, Value};

use super::version::UpdateError;
use crate::core::ecma_trim;
use crate::core::settings;

/// `MAX_TOKEN_LEN` (`token.js:26`). Measured in UTF-16 code units, like
/// `String.prototype.length`.
pub const MAX_TOKEN_LEN: usize = 255;

/// `ENVELOPE_PREFIX` (`token.js:27`).
pub const ENVELOPE_PREFIX: &str = "enc1:";

/// `validateToken` (`token.js:30`).
///
/// Accepts only the character set GitHub actually issues, because the token
/// goes straight into an `Authorization` header. An absent value is `None`,
/// which is the `null` case — `set(None)` clears, so this error is only what a
/// direct call sees.
pub fn validate_token(raw: Option<&str>) -> Result<String, UpdateError> {
    let token = ecma_trim(raw.unwrap_or(""));
    if token.is_empty() {
        return Err(UpdateError::new("token cannot be empty (pass null to clear it)", 400));
    }
    if token.encode_utf16().count() > MAX_TOKEN_LEN {
        return Err(UpdateError::new(
            format!("token is too long (at most {} characters)", MAX_TOKEN_LEN),
            400,
        ));
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(UpdateError::new(
            "token may only contain letters, digits, underscores and hyphens",
            400,
        ));
    }
    Ok(token.to_string())
}

/// `envToken` (`token.js:40`): the first non-empty of `GITHUB_TOKEN`,
/// `AGENTKEEPER_GITHUB_TOKEN`.
pub fn env_token() -> Option<String> {
    for name in ["GITHUB_TOKEN", "AGENTKEEPER_GITHUB_TOKEN"] {
        if let Ok(value) = std::env::var(name) {
            let trimmed = ecma_trim(&value);
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Result of [`TokenStore::set`]; `changed` lets the caller drop a stale
/// rate-limit cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenSetResult {
    pub saved: bool,
    pub changed: bool,
}

enum Decoded {
    Token(String),
    /// The reason to show in `status().error`.
    Failed(String),
}

/// `createTokenStore({ store, safeStorage })`.
#[derive(Debug, Clone)]
pub struct TokenStore {
    root: String,
    /// Test-only: pretend the crypto backend is missing/failing. The JS test
    /// harness injects a stub for exactly this case.
    #[cfg(test)]
    broken_backend: bool,
}

impl TokenStore {
    pub fn new(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            #[cfg(test)]
            broken_backend: false,
        }
    }

    #[cfg(test)]
    fn with_broken_backend(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            broken_backend: true,
        }
    }

    /// `encryptionAvailable` (`token.js:56`): a real probe, not `cfg!(windows)`.
    /// Electron asks OSCrypt; the honest Rust equivalent is to try DPAPI.
    pub fn encryption_available(&self) -> bool {
        #[cfg(test)]
        if self.broken_backend {
            return false;
        }
        #[cfg(windows)]
        {
            dpapi_encrypt(b"1").is_ok()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    fn raw_value(&self) -> String {
        settings::read(&self.root)
            .get("updateToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    /// `envelope()` (`token.js:65`): only a prefixed value counts as stored.
    fn envelope(&self) -> String {
        let value = self.raw_value();
        if value.starts_with(ENVELOPE_PREFIX) {
            value
        } else {
            String::new()
        }
    }

    /// `decode` (`token.js:70`).
    fn decode(&self, value: &str) -> Decoded {
        if value.is_empty() {
            return Decoded::Failed(String::new());
        }
        #[cfg(test)]
        if self.broken_backend {
            return Decoded::Failed("secure storage on this machine is unavailable (the saved token cannot be decrypted)".to_string());
        }
        #[cfg(not(windows))]
        {
            let _ = value;
            return Decoded::Failed(
                "secure storage on this machine is unavailable (the saved token cannot be decrypted)".to_string(),
            );
        }
        #[cfg(windows)]
        {
            // `Buffer.from(str, 'base64')` is lenient in Node; a strict decode
            // error is reported the same way a DPAPI failure is, which is what
            // the user sees either way (the value is unusable).
            let cipher = match base64::engine::general_purpose::STANDARD
                .decode(&value[ENVELOPE_PREFIX.len()..])
            {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Decoded::Failed(format!("the saved token could not be decrypted ({})", error))
                }
            };
            match dpapi_decrypt(&cipher) {
                Ok(plain) => match String::from_utf8(plain) {
                    Ok(token) => Decoded::Token(token),
                    Err(error) => {
                        Decoded::Failed(format!("the saved token could not be decrypted ({})", error))
                    }
                },
                Err(error) => Decoded::Failed(format!("the saved token could not be decrypted ({})", error)),
            }
        }
    }

    /// Token saved through the UI only (no env fallback).
    pub fn stored_token(&self) -> Option<String> {
        match self.decode(&self.envelope()) {
            Decoded::Token(token) => Some(token),
            Decoded::Failed(_) => None,
        }
    }

    /// Effective token: a UI-saved value wins over the environment, because
    /// saving was the last explicit action (`token.js:87`).
    pub fn effective(&self) -> Option<String> {
        if let Decoded::Token(token) = self.decode(&self.envelope()) {
            return Some(token);
        }
        env_token()
    }

    /// Save (`Some(token)`) or clear (`None`).
    pub fn set(&self, token: Option<&str>) -> Result<TokenSetResult, UpdateError> {
        let Some(token) = token else {
            let changed = !self.envelope().is_empty();
            let saved = settings::save(&self.root, &json!({ "updateToken": "" }));
            return Ok(TokenSetResult {
                saved: is_saved(&saved),
                changed,
            });
        };

        let value = validate_token(Some(token))?;
        if !self.encryption_available() {
            // `本机安全存储不可用，已拒绝以明文保存令牌` (token.js:107). There
            // is deliberately no fallback to a plaintext settings entry.
            return Err(UpdateError::new(
                "secure storage on this machine is unavailable; refusing to save the token in plain text",
                400,
            ));
        }
        let sealed = dpapi_encrypt(value.as_bytes())
            .map_err(|error| UpdateError::new(format!("failed to encrypt the token: {}", error), 500))?;
        let sealed = base64::engine::general_purpose::STANDARD.encode(sealed);

        // Read the previous value *before* writing, so re-saving the same
        // token reports `changed: false` (token.js:115-116).
        let previous = self.stored_token();
        let changed = previous.as_deref() != Some(value.as_str());
        let saved = settings::save(
            &self.root,
            &json!({ "updateToken": format!("{}{}", ENVELOPE_PREFIX, sealed) }),
        );
        Ok(TokenSetResult {
            saved: is_saved(&saved),
            changed,
        })
    }

    /// The shape the UI receives. Never the secret itself.
    pub fn status(&self) -> Value {
        let value = self.envelope();
        if !value.is_empty() {
            match self.decode(&value) {
                Decoded::Token(_) => {
                    return json!({ "filled": true, "origin": "stored", "error": Value::Null })
                }
                Decoded::Failed(error) => {
                    let has_env = env_token().is_some();
                    return json!({
                        "filled": has_env,
                        "origin": if has_env { json!("env") } else { Value::Null },
                        // `(result && result.error) || '无法解密'`.
                        "error": if error.is_empty() { "cannot decrypt".to_string() } else { error },
                    });
                }
            }
        }
        if env_token().is_some() {
            return json!({ "filled": true, "origin": "env", "error": Value::Null });
        }
        json!({ "filled": false, "origin": Value::Null, "error": Value::Null })
    }
}

/// `saved && saved.ok !== false` (`token.js:103`).
fn is_saved(result: &Value) -> bool {
    result.get("ok") != Some(&Value::Bool(false))
}

/* ------------------------------------------------------------------ */
/* DPAPI                                                               */
/* ------------------------------------------------------------------ */

/// `CryptProtectData` with an empty description, no entropy, user scope — the
/// exact call Electron's `safeStorage` makes on Windows, which is what makes
/// the ciphertext interchangeable.
#[cfg(windows)]
fn dpapi_encrypt(plain: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};

    if plain.is_empty() {
        return Err("DPAPI refuses empty input".to_string());
    }

    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut output,
        );
        if ok == 0 {
            return Err(format!(
                "DPAPI encryption failed ({})",
                std::io::Error::last_os_error()
            ));
        }
        let sealed =
            std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData as _);
        Ok(sealed)
    }
}

#[cfg(windows)]
fn dpapi_decrypt(cipher: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    if cipher.is_empty() {
        return Err("DPAPI refuses empty cipher text".to_string());
    }

    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: cipher.len() as u32,
            pbData: cipher.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let mut description: *mut u16 = std::ptr::null_mut();
        let ok = CryptUnprotectData(
            &input,
            &mut description,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut output,
        );
        if ok == 0 {
            return Err(format!(
                "DPAPI decryption failed ({})",
                std::io::Error::last_os_error()
            ));
        }
        if !description.is_null() {
            LocalFree(description as _);
        }
        let plain =
            std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData as _);
        Ok(plain)
    }
}

#[cfg(not(windows))]
fn dpapi_encrypt(_plain: &[u8]) -> Result<Vec<u8>, String> {
    Err("secure storage is unavailable (no DPAPI on this platform)".to_string())
}

#[cfg(not(windows))]
fn dpapi_decrypt(_cipher: &[u8]) -> Result<Vec<u8>, String> {
    Err("secure storage is unavailable (no DPAPI on this platform)".to_string())
}

/* ------------------------------------------------------------------ */
/* tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Tests that touch process-wide env vars must not run in parallel.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_root() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ak-token-test-{}-{}",
            std::process::id(),
            stamp
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn root_str(path: &PathBuf) -> String {
        path.to_string_lossy().into_owned()
    }

    fn clear_env() {
        std::env::remove_var("GITHUB_TOKEN");
        std::env::remove_var("AGENTKEEPER_GITHUB_TOKEN");
    }

    #[test]
    fn envelope_is_base64_of_the_ciphertext() {
        let root = temp_root();
        let store = TokenStore::new(root_str(&root));
        if !store.encryption_available() {
            // No DPAPI (unusual on Windows, the default elsewhere): the write
            // must be refused rather than falling back to plaintext.
            let error = store.set(Some("ghp_example")).unwrap_err();
            assert_eq!(error.status(), 400);
            assert!(error.message().contains("refusing"), "{}", error);
            let _ = std::fs::remove_dir_all(&root);
            return;
        }

        let token = "ghp_ABCdef0123456789_-";
        let result = store.set(Some(token)).unwrap();
        assert!(result.saved);
        assert!(result.changed);

        let raw = settings::read(&root_str(&root))["updateToken"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(raw.starts_with(ENVELOPE_PREFIX), "{}", raw);
        assert!(!raw.contains(token), "the envelope must not contain plaintext");

        let cipher = base64::engine::general_purpose::STANDARD
            .decode(&raw[ENVELOPE_PREFIX.len()..])
            .expect("envelope carries standard base64");
        assert!(!cipher.is_empty());
        assert_eq!(dpapi_decrypt(&cipher).unwrap(), token.as_bytes());

        assert_eq!(store.stored_token().as_deref(), Some(token));
        assert_eq!(store.effective().as_deref(), Some(token));
        assert_eq!(store.status()["filled"], true);
        assert_eq!(store.status()["origin"], "stored");
        assert_eq!(store.status()["error"], Value::Null);
        // Never echoed.
        assert!(!store.status().to_string().contains(token));

        // Re-saving the same token is not a credential switch.
        let again = store.set(Some(token)).unwrap();
        assert!(again.saved);
        assert!(!again.changed);
        // A different token is.
        assert!(store.set(Some("ghp_other")).unwrap().changed);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_token_accepts_and_rejects_like_the_js() {
        assert_eq!(validate_token(Some("  ghp_abc-123_X  ")).unwrap(), "ghp_abc-123_X");
        assert_eq!(validate_token(Some("A")).unwrap(), "A");

        // Empty / null.
        for empty in [None, Some(""), Some("   ")] {
            let error = validate_token(empty).unwrap_err();
            assert_eq!(error.status(), 400);
            assert!(error.message().contains("cannot be empty"), "{}", error);
        }
        // Charset: a token enters an HTTP header.
        for bad in ["ghp abc", "ghp/abc", "ghp.abc", "ghp+abc", "令牌"] {
            let error = validate_token(Some(bad)).unwrap_err();
            assert_eq!(error.status(), 400);
            assert!(error.message().contains("may only contain"), "{} {}", bad, error);
        }
        // Length is counted in UTF-16 code units, like String#length: 128
        // emoji are 256 units and too long even though they are 128 chars.
        let emoji = "\u{1F600}".repeat(128);
        let error = validate_token(Some(emoji.as_str())).unwrap_err();
        assert!(error.message().contains("too long"), "{}", error);
        let longest = "a".repeat(MAX_TOKEN_LEN);
        assert!(validate_token(Some(longest.as_str())).is_ok());
        let too_long = "a".repeat(MAX_TOKEN_LEN + 1);
        let error = validate_token(Some(too_long.as_str())).unwrap_err();
        assert!(error.message().contains("too long"), "{}", error);
    }

    #[test]
    fn undecryptable_value_is_not_reported_as_empty() {
        let root = temp_root();
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();

        // Valid prefix, garbage ciphertext: DPAPI cannot open it.
        settings::save(
            &root_str(&root),
            &json!({ "updateToken": "enc1:bm90IGEgZHBhcGkgYmxvYg==" }),
        );
        let store = TokenStore::new(root_str(&root));

        let status = store.status();
        assert_eq!(status["filled"], false);
        assert_eq!(status["origin"], Value::Null);
        // The real message is `Decoded::Failed`'s — "the saved token could not
        // be decrypted (..)". `"cannot decrypt"` only appears when that string
        // is empty, which is precisely the case this test is asserting cannot
        // happen, so matching on it here would pass on the wrong branch.
        assert!(
            status["error"]
                .as_str()
                .unwrap()
                .contains("could not be decrypted"),
            "{}",
            status
        );
        assert!(store.effective().is_none());

        // With an env token the quota is still covered; the decrypt error must
        // survive so the UI can offer a re-save.
        std::env::set_var("GITHUB_TOKEN", "ghp_from_env");
        let status = store.status();
        assert_eq!(status["filled"], true);
        assert_eq!(status["origin"], "env");
        assert!(status["error"]
            .as_str()
            .unwrap()
            .contains("could not be decrypted"));
        assert_eq!(store.effective().as_deref(), Some("ghp_from_env"));

        clear_env();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_to_write_without_a_crypto_backend() {
        // `status()` below reads `env_token()`, and the sibling tests in this
        // module set and clear GITHUB_TOKEN under this lock. Without taking it
        // the `filled == false` assertion raced them: one baseline run failed
        // here and every isolated run passed — the signature of an unlocked
        // environment read, not of a broken crypto backend.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let root = temp_root();
        let store = TokenStore::with_broken_backend(root_str(&root));

        assert!(!store.encryption_available());
        let error = store.set(Some("ghp_example")).unwrap_err();
        assert_eq!(error.status(), 400);
        assert_eq!(error.message(), "secure storage on this machine is unavailable; refusing to save the token in plain text");

        // Nothing was written — certainly nothing in plaintext.
        let raw = settings::read(&root_str(&root))["updateToken"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(raw, "");
        assert!(!raw.contains("ghp_example"));

        // A pre-existing envelope is reported as undecryptable, not as empty.
        settings::save(&root_str(&root), &json!({ "updateToken": "enc1:AAAA" }));
        let status = store.status();
        assert_eq!(status["filled"], false);
        assert!(status["error"].as_str().unwrap().contains("secure storage on this machine is unavailable"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clearing_reports_changed_and_env_fills_the_gap() {
        let root = temp_root();
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        settings::save(&root_str(&root), &json!({ "updateToken": "enc1:AAAA" }));
        let store = TokenStore::new(root_str(&root));

        let cleared = store.set(None).unwrap();
        assert!(cleared.saved);
        assert!(cleared.changed);
        assert_eq!(
            settings::read(&root_str(&root))["updateToken"].as_str(),
            Some("")
        );
        // Clearing twice is not a change.
        assert!(!store.set(None).unwrap().changed);

        // Empty store + env token: filled from the environment.
        std::env::set_var("AGENTKEEPER_GITHUB_TOKEN", "ghp_env");
        let status = store.status();
        assert_eq!(status["filled"], true);
        assert_eq!(status["origin"], "env");
        assert_eq!(store.effective().as_deref(), Some("ghp_env"));

        // A stored token wins over the environment.
        std::env::set_var("GITHUB_TOKEN", "ghp_other");
        assert_eq!(env_token().as_deref(), Some("ghp_other"));

        clear_env();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip_works_on_this_machine() {
        if dpapi_encrypt(b"x").is_err() {
            // A locked-down service account can lack a DPAPI master key.
            return;
        }
        let sealed = dpapi_encrypt("令牌-123".as_bytes()).unwrap();
        assert_ne!(sealed, "令牌-123".as_bytes());
        assert_eq!(dpapi_decrypt(&sealed).unwrap(), "令牌-123".as_bytes());
        // Ciphertext from another user/machine cannot be opened.
        assert!(dpapi_decrypt(b"not a dpapi blob").is_err());
    }
}
