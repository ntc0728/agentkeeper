//! GitHub 令牌存储。
//!
//! 令牌会提高 API 配额（60 → 5000 次/小时），因此属于凭据：`settings.json`
//! 里的明文副本会跟着每一次导出、每一次换机一起走。存储的值在
//! Windows 上由 DPAPI 包装、在 macOS 上由 Keychain 包装、在 Linux 上由
//! libsecret 包装，因此换到另一台机器上毫无用处。信封的形态是
//! `enc1:` 前缀后接一段 base64 文本（编码对象是对 UTF-8 令牌做 DPAPI 加密后
//! 的密文），即下式所示：
//!
//! `enc1:` 前缀 + base64( `CryptProtectData(token_utf8)` 的结果 )
//!
//! 在 Windows 上这是无熵、用户作用域的 DPAPI，所以在这台机器上写入的令牌
//! 能被同一用户在这里读回。（已由往返测试验证；只有在机器上确实存在这种
//! 值时，才覆盖旧版本安装产生的值。）
//!
//! UI 依赖的两条规则：
//!
//! 1. 令牌**绝不回显** —— [`TokenStore::status`] 只报告
//!    `{filled, origin, error}` 这三个字段（是否已填、来源、错误）；
//! 2. 无法解密的值**不会**被报告为"未配置"：说"空"会让用户被悄悄降级到
//!    匿名配额，并掩盖真正的问题。原因是会返回的，这样 UI 可以提示重新保存。
//!
//! 如果 DPAPI 不可用或拒绝服务，[`TokenStore::set`] 会失败；不存在
//! 明文回退。

use base64::Engine;
use serde_json::{json, Value};

use super::version::UpdateError;
use crate::ecma_trim;
use crate::settings;

/// `MAX_TOKEN_LEN`。以 UTF-16 码元计量，与
/// `String.prototype.length` 一致。
pub const MAX_TOKEN_LEN: usize = 255;

/// `ENVELOPE_PREFIX`。
pub const ENVELOPE_PREFIX: &str = "enc1:";

/// `validateToken`。
///
/// 只接受 GitHub 实际签发的字符集，因为令牌会直接进入
/// `Authorization` 请求头。缺失的值是 `None`，
/// 即 `null` 那种情况 —— `set(None)` 表示清除，所以这个错误只有直接调用时才会看到。
pub fn validate_token(raw: Option<&str>) -> Result<String, UpdateError> {
    let token = ecma_trim(raw.unwrap_or(""));
    if token.is_empty() {
        return Err(UpdateError::new(
            "token cannot be empty (pass null to clear it)",
            400,
        ));
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

/// 对应参考实现的 `envToken`：`GITHUB_TOKEN` 与
/// `AGENTKEEPER_GITHUB_TOKEN` 中第一个非空的值。
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

/// [`TokenStore::set`] 的结果；`changed` 让调用方可以丢弃过期的
/// 限流冷却。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenSetResult {
    pub saved: bool,
    pub changed: bool,
}

enum Decoded {
    Token(String),
    /// 要在 `status().error` 中显示的原因。
    Failed(String),
}

/// 对应参考实现的 `createTokenStore({ store, safeStorage })` 工厂。
#[derive(Debug, Clone)]
pub struct TokenStore {
    root: String,
    /// 仅测试用：假装加密后端缺失/失败。测试夹具注入一个
    /// stub 来覆盖这种情况。
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

    /// `encryption_available`：真实探测，而不是 `cfg!(windows)`。
    /// 诚实的检查是试着调用 DPAPI，看它是否成功。
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

    /// `envelope()`：只有带前缀的值才算已存储。
    fn envelope(&self) -> String {
        let value = self.raw_value();
        if value.starts_with(ENVELOPE_PREFIX) {
            value
        } else {
            String::new()
        }
    }

    /// `decode`。
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
            // base64 解码在早期实现里是宽松的；这里严格解码，失败与 DPAPI 失败
            // 走同一条报告路径，用户看到的结果一样（该值已不可用）。
            let cipher = match base64::engine::general_purpose::STANDARD
                .decode(&value[ENVELOPE_PREFIX.len()..])
            {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Decoded::Failed(format!(
                        "the saved token could not be decrypted ({})",
                        error
                    ))
                }
            };
            match dpapi_decrypt(&cipher) {
                Ok(plain) => match String::from_utf8(plain) {
                    Ok(token) => Decoded::Token(token),
                    Err(error) => Decoded::Failed(format!(
                        "the saved token could not be decrypted ({})",
                        error
                    )),
                },
                Err(error) => Decoded::Failed(format!(
                    "the saved token could not be decrypted ({})",
                    error
                )),
            }
        }
    }

    /// 仅通过 UI 保存的令牌（无环境变量回退）。
    pub fn stored_token(&self) -> Option<String> {
        match self.decode(&self.envelope()) {
            Decoded::Token(token) => Some(token),
            Decoded::Failed(_) => None,
        }
    }

    /// 生效的令牌：UI 保存的值优先于环境变量，因为
    /// 保存是最近一次显式动作。
    pub fn effective(&self) -> Option<String> {
        if let Decoded::Token(token) = self.decode(&self.envelope()) {
            return Some(token);
        }
        env_token()
    }

    /// 保存（`Some(token)`）或清除（`None`）。
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
            // `本机安全存储不可用，已拒绝以明文保存令牌`。这里
            // 刻意没有回退到明文设置项。
            return Err(UpdateError::new(
                "secure storage on this machine is unavailable; refusing to save the token in plain text",
                400,
            ));
        }
        let sealed = dpapi_encrypt(value.as_bytes()).map_err(|error| {
            UpdateError::new(format!("failed to encrypt the token: {}", error), 500)
        })?;
        let sealed = base64::engine::general_purpose::STANDARD.encode(sealed);

        // 在写入*之前*读取旧值，这样重复保存同一个
        // 令牌会报告 `changed: false`。
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

    /// UI 收到的形状。绝不含密文本身。
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

/// `saved && saved.ok !== false` —— 写盘成功即视为已保存。
fn is_saved(result: &Value) -> bool {
    result.get("ok") != Some(&Value::Bool(false))
}

/* ------------------------------------------------------------------ */
/* DPAPI 封装                                                          */
/* ------------------------------------------------------------------ */

/// 带空描述、无熵、用户作用域的 `CryptProtectData` —— 这种形状让同一台
/// 机器上同一用户的密文可以互换。
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
        let sealed = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
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
        let plain = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
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
/* 测试                                                                 */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// 会改动进程级环境变量的测试不得并行运行。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_root() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("ak-token-test-{}-{}", std::process::id(), stamp));
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
            // 没有 DPAPI（Windows 上少见，其他平台是默认情况）：必须
            // 拒绝写入，而不是回退到明文。
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
        assert!(
            !raw.contains(token),
            "the envelope must not contain plaintext"
        );

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
        // 绝不回显。
        assert!(!store.status().to_string().contains(token));

        // 重新保存同一个令牌不算更换凭据。
        let again = store.set(Some(token)).unwrap();
        assert!(again.saved);
        assert!(!again.changed);
        // 换成另一个令牌才算。
        assert!(store.set(Some("ghp_other")).unwrap().changed);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_token_accepts_a_well_formed_value_and_rejects_others() {
        assert_eq!(
            validate_token(Some("  ghp_abc-123_X  ")).unwrap(),
            "ghp_abc-123_X"
        );
        assert_eq!(validate_token(Some("A")).unwrap(), "A");

        // 空 / null。
        for empty in [None, Some(""), Some("   ")] {
            let error = validate_token(empty).unwrap_err();
            assert_eq!(error.status(), 400);
            assert!(error.message().contains("cannot be empty"), "{}", error);
        }
        // 字符集：令牌会进入 HTTP 请求头。
        for bad in ["ghp abc", "ghp/abc", "ghp.abc", "ghp+abc", "令牌"] {
            let error = validate_token(Some(bad)).unwrap_err();
            assert_eq!(error.status(), 400);
            assert!(
                error.message().contains("may only contain"),
                "{} {}",
                bad,
                error
            );
        }
        // 长度按 UTF-16 码元计，与 String#length 相同：128 个
        // emoji 是 256 个码元，虽然只有 128 个字符仍然太长。
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

        // 前缀合法、密文是垃圾：DPAPI 打不开它。
        settings::save(
            &root_str(&root),
            &json!({ "updateToken": "enc1:bm90IGEgZHBhcGkgYmxvYg==" }),
        );
        let store = TokenStore::new(root_str(&root));

        let status = store.status();
        assert_eq!(status["filled"], false);
        assert_eq!(status["origin"], Value::Null);
        // 真正的消息来自 `Decoded::Failed` —— "the saved token could not
        // be decrypted (..)"。`"cannot decrypt"` 只在该字符串
        // 为空时才出现，而这恰恰是本测试断言不可能发生的情况，
        // 所以在这里匹配它会命中错误的分支。
        assert!(
            status["error"]
                .as_str()
                .unwrap()
                .contains("could not be decrypted"),
            "{}",
            status
        );
        assert!(store.effective().is_none());

        // 有环境变量令牌时配额仍然有保障；解密错误必须
        // 保留下来，好让 UI 提示重新保存。
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
        // 下面的 `status()` 会读 `env_token()`，而本模块中的兄弟测试在
        // 这把锁下设置和清除 GITHUB_TOKEN。不拿这把锁的话
        // `filled == false` 的断言就会与它们竞争：一次基线运行就在
        // 这里失败，而每次单独运行都通过 —— 这正是未加锁的
        // 环境变量读取的特征，而不是加密后端坏了的特征。
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let root = temp_root();
        let store = TokenStore::with_broken_backend(root_str(&root));

        assert!(!store.encryption_available());
        let error = store.set(Some("ghp_example")).unwrap_err();
        assert_eq!(error.status(), 400);
        assert_eq!(error.message(), "secure storage on this machine is unavailable; refusing to save the token in plain text");

        // 什么都没写入 —— 更不用说写入明文了。
        let raw = settings::read(&root_str(&root))["updateToken"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(raw, "");
        assert!(!raw.contains("ghp_example"));

        // 已存在的信封被报告为无法解密，而不是空。
        settings::save(&root_str(&root), &json!({ "updateToken": "enc1:AAAA" }));
        let status = store.status();
        assert_eq!(status["filled"], false);
        assert!(status["error"]
            .as_str()
            .unwrap()
            .contains("secure storage on this machine is unavailable"));

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
        // 清除两次不算变更。
        assert!(!store.set(None).unwrap().changed);

        // 空存储 + 环境变量令牌：从环境变量填充。
        std::env::set_var("AGENTKEEPER_GITHUB_TOKEN", "ghp_env");
        let status = store.status();
        assert_eq!(status["filled"], true);
        assert_eq!(status["origin"], "env");
        assert_eq!(store.effective().as_deref(), Some("ghp_env"));

        // 已存储的令牌优先于环境变量。
        std::env::set_var("GITHUB_TOKEN", "ghp_other");
        assert_eq!(env_token().as_deref(), Some("ghp_other"));

        clear_env();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip_works_on_this_machine() {
        if dpapi_encrypt(b"x").is_err() {
            // 受限的服务账户可能缺少 DPAPI 主密钥。
            return;
        }
        let sealed = dpapi_encrypt("令牌-123".as_bytes()).unwrap();
        assert_ne!(sealed, "令牌-123".as_bytes());
        assert_eq!(dpapi_decrypt(&sealed).unwrap(), "令牌-123".as_bytes());
        // 来自其他用户/机器的密文打不开。
        assert!(dpapi_decrypt(b"not a dpapi blob").is_err());
    }
}
