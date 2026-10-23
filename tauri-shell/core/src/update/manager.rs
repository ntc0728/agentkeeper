//! The update state machine.
//!
//! Check → download → install, with the schedule and the last result persisted
//! under `<store>/update-state.json`. Every response keeps its shape because the
//! IPC layer hands these values to the renderer verbatim and
//! `src/renderer/app.js` reads named fields off them:
//!
//! | method | response |
//! |---|---|
//! | [`UpdateManager::status`] | the persisted status snapshot |
//! | [`UpdateManager::check`] | that status, `ok:false` + `reason` on failure |
//! | [`UpdateManager::save_settings`] | status |
//! | [`UpdateManager::set_token`] | status + `saved` + `changed` |
//! | [`UpdateManager::start_download`] | the task snapshot |
//! | [`UpdateManager::progress`] | task or `null` |
//! | [`UpdateManager::cancel`] | `{ canceled: bool }` |
//! | [`UpdateManager::install`] | `{ ok, restarted, path, mode }` |
//! | [`UpdateManager::skip`] / [`UpdateManager::unskip`] | status |
//!
//! The IPC layer wraps download/install failures as `{ ok: false, error }`; the
//! methods here return `Result` and the `lib.rs` command converts
//! [`UpdateError`] the same way.
//!
//! ## Fidelity
//!
//! Behaviour, field names, defaults and *when* state is written follow the
//! earlier build. Where it has a quirk, the quirk is reproduced and
//! marked `// 解析口径:`.
//!
//! * `check` writes `lastAttemptAt` **before** the network call and
//!   `lastSuccessAt`/`nextCheckAt`/`release` after it. A 404 counts as a
//!   successful check (the repo simply has no release yet).
//! * The 60 s manual throttle needs a previous *success* and compares against
//!   `lastAttemptAt`, so two manual checks after a failure both go to the
//!   network.
//! * A cooldown hit, or the throttle, never touches the state file.
//! * `state` is a flat object in `update-state.json`; skipped versions live in
//!   the same file (`skippedVersion`), not in settings. Unknown keys already in
//!   the file are preserved on the next write, exactly like the reference spread.
//! * A **cancelled** download returns out of the `try` block before the `catch`
//!   that deletes the partial file, so the partial installer stays on disk
//!   (task `path` is nulled and the next download of the same name unlinks it).
//!   Only a failed download deletes. See `run_download`.
//! * `install` verifies the path *before* checking whether a download is
//!   active, so an invalid path reports the path error even mid-download.
//! * `status()` is pure apart from reading settings/state and decrypting the
//!   token status; every field is present on every call, including before the
//!   first check.
//!
//! Deliberate, documented differences (each is a "Rust cannot do that" or an
//! internal-only detail):
//!
//! * `appVersion` comes from `env!("CARGO_PKG_VERSION")`. `Cargo.toml` and
//!   `package.json` both say `0.2.0`; they must be bumped together, otherwise
//!   the app compares against a different "current" than the release does.
//! * The built-in repository is the literal `ntc0728/agentkeeper` — the value
//!   `package.json`'s `repository.url` produces. There is no runtime
//!   `require('package.json')` to fall back from.
//! * `Retry-After` is parsed as IMF-fixdate only (see [`parse_http_date`]);
//!   GitHub sends `X-RateLimit-Reset`, so this is a fallback nobody hits.
//! * A download runs on its own thread because `core::http` is blocking while
//!   the IPC handler must return the task snapshot immediately (the renderer
//!   polls `ak:update-progress`).
//!
//! ## Assumed interfaces (owned by other modules, not implemented here)
//!
//! `core/update/http.rs` — implemented here:
//! `fetch_release(url, token, timeout_ms) -> Result<ReleaseFetch, UpdateError>`,
//! with `ReleaseFetch { status, headers, json }`. `Ok` for *any* HTTP status
//! (404/403 included); `Err` only for transport or body-parse failures. It
//! takes **no proxy setting** (see the limitation in `core::http`): `updateProxy`
//! therefore affects downloads but not release checks.
//!
//! `core/http.rs` — implemented here:
//! `download_to_file(url, dest, proxy, max_bytes, timeout_ms, on_progress)`
//! with `on_progress: &mut dyn FnMut(u64, Option<u64>) -> bool`; `false` aborts
//! and leaves the partial file in place (the reference cancel path).
//!
//! `core/update/token.rs` — implemented here: `TokenStore::new(root)`,
//! `status() -> Value`, `encryption_available() -> bool`, `effective() ->
//! Option<String>`, `set(Option<&str>) -> Result<TokenSetResult, UpdateError>`.

use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ecma_trim;
use crate::http::FetchError;
use crate::model::js_truthy;
use crate::settings as settings_store;

use super::http::{self as update_http, ReleaseFetch};
use super::token::TokenStore;
use super::version::{self, ReleaseOptions, UpdateError};

/* ------------------------------------------------------------------ */
/* constants                                                           */
/* ------------------------------------------------------------------ */

/// Two checks closer than this reuse the previous answer (manual included).
pub const MIN_CHECK_GAP_MS: i64 = 60 * 1000;

/// Retry floor when GitHub answers 403/429 without usable headers.
pub const RATE_LIMIT_FLOOR_MS: i64 = 60 * 1000;

pub const DEFAULT_MINUTES: i64 = 20;
pub const MIN_MINUTES: i64 = 1;
pub const MAX_MINUTES: i64 = 1440;

/// `USER_AGENT` — the header belongs to `update/http.rs`; kept here for the
/// tests that pin the value.
pub const USER_AGENT: &str = "AgentKeeper-updater";

const STATE_FILE: &str = "update-state.json";
const UPDATES_DIR: &str = "updates";

/// The value `package.json`'s `repository.url` resolves to. See the module
/// docs: the reference falls back to exactly this string when the file is missing.
pub const BUILT_IN_REPO: &str = "ntc0728/agentkeeper";

/// Explicit deadline for one GitHub API call (connect, TLS, request, body).
///
/// `ureq` has a *default* read timeout, so a missing value here would still
/// "work" — and then drift with a dependency bump. Naming it also documents the
/// intent: a total deadline is right for a small JSON body (a stuck peer must
/// fail the check, not park the scheduler thread) and wrong for the installer
/// stream, which gets [`DOWNLOAD_TIMEOUT_MS`] per phase instead.
pub const NETWORK_TIMEOUT_MS: u64 = 30_000;

/// Per-phase deadline for the installer download. Larger than the API call on
/// purpose: a 600 MB transfer is allowed to be slow, it is not allowed to be
/// silent. `http::download_to_file` applies this per read/write/connect, never
/// to the whole transfer (which would abort a legitimately slow download).
pub const DOWNLOAD_TIMEOUT_MS: u64 = 60_000;

/* ------------------------------------------------------------------ */
/* 值类型转换                                                            */
/* ------------------------------------------------------------------ */

fn js_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_to_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// `String(v || '')`.
fn js_string_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(v) if js_truthy(v) => js_to_string(v),
        _ => String::new(),
    }
}

/// `Number(v)` for the scalar shapes a settings file can contain. Enough for
/// `Math.round(Number(...))`; JSON settings are numbers in practice.
fn js_number(value: Option<&Value>) -> f64 {
    match value {
        None | Some(Value::Null) => 0.0,
        Some(Value::Bool(true)) => 1.0,
        Some(Value::Bool(false)) => 0.0,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let text = ecma_trim(s);
            if text.is_empty() {
                0.0
            } else {
                text.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Some(Value::Array(items)) => match items.len() {
            0 => 0.0,
            1 => js_number(Some(&items[0])),
            _ => f64::NAN,
        },
        Some(Value::Object(_)) => f64::NAN,
    }
}

/* ------------------------------------------------------------------ */
/* repo + interval normalisation                                       */
/* ------------------------------------------------------------------ */

/// `owner/repo`, the only shape GitHub accepts as a path segment.
///
/// The reference uses `/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/`; a manual check keeps
/// this dependency-free and rejects `a/b/c`, empty halves and whitespace.
pub fn normalize_repo(value: &str) -> String {
    let text = ecma_trim(value);
    let mut parts = text.split('/');
    let owner = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    let extra = parts.next();
    let allowed = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    };
    if extra.is_none() && allowed(owner) && allowed(repo) {
        text.to_string()
    } else {
        String::new()
    }
}

/// `clampMinutes` — `Math.round(Number(v))`, defaulting on non-finite input and
/// clamping to 1..=1440.
pub fn clamp_minutes(value: &Value) -> i64 {
    let n = js_number(Some(value));
    if !n.is_finite() {
        return DEFAULT_MINUTES;
    }
    // `Math.round` rounds half toward +Infinity; `f64::round` rounds away from
    // zero. The only difference is -0.5 -> -0 vs -1, and both clamp to 1.
    let rounded = n.round();
    (rounded as i64).clamp(MIN_MINUTES, MAX_MINUTES)
}

/* ------------------------------------------------------------------ */
/* the `ak:update-*` payload guard                     */
/* ------------------------------------------------------------------ */
//
// The reference validates the raw IPC payload in the *handler*, before
// `manager.saveSettings` ever runs — `saveSettings` itself only clamps and
// trims . These two functions are that guard, kept here so
// the rules are one testable unit and `lib.rs` stays a thin adapter. Without
// them a malformed repo / apiBase / proxy / interval is persisted silently and
// surfaces later as "更新检查失败" instead of an error at the click.

// The Rust engine speaks English; `toast()` runs `zhError()` → `zhText()` over
// every `{ok:false,error}` it receives, so Chinese comes from
// `src/renderer/engine-zh.js` (EXACT), never from here. That is also why these
// the `ipc.repoFormat` / `ipc.apiBaseFormat` / `ipc.proxyFormat` /
// `ipc.intervalRange` / `update.tokenFieldRequired` / `update.tokenType`
// entries hold English: an English UI must never print Chinese.
//
// `ERR_REPO_FORMAT` is deliberately the *same sentence* `discover.repoFormat`
// uses: one rule, one string, one dictionary entry.

/// `ipc.intervalRange` — `检查间隔必须是 1 ~ 1440 分钟`.
pub const ERR_INTERVAL_RANGE: &str = "check interval must be between 1 and 1440 minutes";
/// `ipc.repoFormat` — `仓库必须是 owner/repo 形式`.
pub const ERR_REPO_FORMAT: &str = "repo must be owner/repo or a github.com URL";
/// `ipc.apiBaseFormat` — `更新源必须是 http(s) 地址`.
pub const ERR_API_BASE_FORMAT: &str = "update source must be an http(s) URL";
/// `ipc.proxyFormat` — `代理必须是 auto、direct 或 http(s) 地址`.
pub const ERR_PROXY_FORMAT: &str = "proxy must be auto, direct or an http(s) URL";
/// `update.tokenType` — `token 必须是字符串或 null`.
pub const ERR_TOKEN_TYPE: &str = "token must be a string or null";
/// `update.tokenFieldRequired` — `请求缺少 token 字段（清除请传 null）`.
pub const ERR_TOKEN_FIELD_REQUIRED: &str =
    "request is missing the token field (pass null to clear)";

/// `asString(v, max)` (reference): anything that is not a string is `""`,
/// a string is sliced to `max` characters.
fn js_as_string(value: &Value, max: usize) -> String {
    match value {
        Value::String(text) => text.chars().take(max).collect(),
        _ => String::new(),
    }
}

/// The `ak:update-settings` guard.
///
/// Returns the error message the IPC layer answers with, or `None` when the
/// patch is acceptable. Keys the patch does not carry are not inspected, so a
/// partial save (only `auto`, say) is never rejected for a field it did not
/// send.
pub fn validate_settings_patch(patch: &Value) -> Option<&'static str> {
    if let Some(value) = patch.get("minutes") {
        // `Math.round(Number(v))` first: 20.6 → 21 and 0.6 → 1 both pass,
        // NaN/Infinity and anything outside 1..=1440 do not.
        let minutes = js_number(Some(value)).round();
        if !minutes.is_finite() || minutes < 1.0 || minutes > 1440.0 {
            return Some(ERR_INTERVAL_RANGE);
        }
    }
    if let Some(value) = patch.get("repo") {
        let repo = ecma_trim(&js_as_string(value, 200)).to_string();
        if !repo.is_empty() && normalize_repo(&repo).is_empty() {
            return Some(ERR_REPO_FORMAT);
        }
    }
    if let Some(value) = patch.get("apiBase") {
        let base = ecma_trim(&js_as_string(value, 300)).to_string();
        if !base.is_empty() && !(base.starts_with("http://") || base.starts_with("https://")) {
            return Some(ERR_API_BASE_FORMAT);
        }
    }
    if let Some(value) = patch.get("proxy") {
        let text = ecma_trim(&js_as_string(value, 200)).to_string();
        let proxy = if text.is_empty() {
            "auto"
        } else {
            text.as_str()
        };
        // `/^https?:\/\/\S+$/` — a pinned route must be a URL with no
        // whitespace in it.
        let pinned = proxy
            .strip_prefix("https://")
            .or_else(|| proxy.strip_prefix("http://"))
            .map(|rest| !rest.is_empty() && !rest.contains(char::is_whitespace))
            .unwrap_or(false);
        if proxy != "auto" && proxy != "direct" && !pinned {
            return Some(ERR_PROXY_FORMAT);
        }
    }
    None
}

/// The `ak:update-token` guard.
///
/// `payload` is the **whole IPC body**, not the `token` field, and that is the
/// only reason the missing-field branch works. The reference checks `'token' in p`;
/// the obvious Rust translation — `token: Option<Value>` — cannot, because
/// Tauri's `CommandItem::deserialize_option` (`tauri-2.12.4/src/ipc/command.rs:146`)
/// turns a *missing* key into `visit_none()` and `Value::Null` into
/// `visit_none()` too, so `{}` and `{token:null}` arrive identically and there
/// is nothing left to distinguish. Taking the request body keeps the two
/// cases apart all the way down, so:
///
/// * no `token` key (or a non-object body, which `asObject` collapses to `{}`)
///   → `ERR_TOKEN_FIELD_REQUIRED`;
/// * `null` → `Ok(None)`: clear;
/// * a string → `Ok(Some(..))`: replace;
/// * a number / bool / array / object → `ERR_TOKEN_TYPE`.
///
/// Clearing must be explicit. A caller that forgets the field is a caller bug,
/// and silently deleting the credential would be the worst possible way to
/// handle it.
pub fn token_arg(payload: &Value) -> Result<Option<String>, &'static str> {
    match payload.get("token") {
        None => Err(ERR_TOKEN_FIELD_REQUIRED),
        Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ERR_TOKEN_TYPE),
    }
}

/* ------------------------------------------------------------------ */
/* rate-limit deadline                                                 */
/* ------------------------------------------------------------------ */

/// "Earliest moment we may ask again" from GitHub's rate-limit headers.
///
/// `Retry-After` (seconds or HTTP date), `X-RateLimit-Reset` (epoch seconds,
/// only meaningful when remaining is 0), plus the one-minute floor on 403/429.
/// Header names are matched case-insensitively; the first value wins (the
/// takes the first element of an array).
pub fn retry_deadline(headers: &[(String, String)], status: u16, now: i64) -> i64 {
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    let mut deadline: i64 = 0;
    // `if (retryAfter)` — an empty string or "0"… wait: "0" is truthy, 0 is
    // not. Strings from a header map are always truthy unless empty, and
    // `Number('0')` is a perfectly good zero.
    if let Some(retry_after) = get("retry-after").filter(|v| !v.is_empty()) {
        let seconds = js_number(Some(&Value::String(retry_after.to_string())));
        if seconds.is_finite() {
            deadline = now + (seconds.max(0.0) * 1000.0) as i64;
        } else if let Some(date) = parse_http_date(retry_after) {
            deadline = date;
        }
    }
    if get("x-ratelimit-remaining") == Some("0") {
        let reset = js_number(Some(&Value::String(
            get("x-ratelimit-reset").unwrap_or("").to_string(),
        )));
        if reset.is_finite() && reset > 0.0 {
            deadline = deadline.max((reset * 1000.0) as i64 + 1000);
        }
    }
    if status == 403 || status == 429 {
        deadline = deadline.max(now + RATE_LIMIT_FLOOR_MS);
    }
    deadline
}

/// IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) → epoch milliseconds.
///
/// `Date.parse` accepts more spellings; this is the format RFC 7231
/// names as canonical and the one GitHub would send if it used the header.
/// `None` where `Date.parse` would return `NaN`.
fn parse_http_date(text: &str) -> Option<i64> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    if !parts[0].ends_with(',') || !parts[5].eq_ignore_ascii_case("GMT") {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month: i64 = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let clock: Vec<&str> = parts[4].split(':').collect();
    if clock.len() != 3 {
        return None;
    }
    let hour: i64 = clock[0].parse().ok()?;
    let minute: i64 = clock[1].parse().ok()?;
    let second: i64 = clock[2].parse().ok()?;
    if !(1..=31).contains(&day) || !(0..=23).contains(&hour) || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3600 + minute * 60 + second) * 1000)
}

/// Howard Hinnant's `days_from_civil` — days since 1970-01-01.
fn days_from_civil(mut year: i64, month: i64, day: i64) -> i64 {
    if month <= 2 {
        year -= 1;
    }
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/* ------------------------------------------------------------------ */
/* platform mapping                                                    */
/* ------------------------------------------------------------------ */

/// `process.platform` spelled the Node way — `installerKind`,
/// `allowedInstallerSuffixes` and the silent-install check all compare against
/// these strings.
pub fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

/// `process.arch` spelled the Node way (`x86_64` would pick the wrong asset).
pub fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/* ------------------------------------------------------------------ */
/* persisted state                                                     */
/* ------------------------------------------------------------------ */

/// `update-state.json` in memory.
///
/// Field order matters only for the file's bytes: [`State::to_value`] writes
/// the seven base keys in the reference literal's order, then `extra` (unknown keys,
/// `downloadedPath`/`downloadedVersion`) in the order they were read/added —
/// the same result as `{ ...base, ...raw }`.
#[derive(Debug, Clone)]
struct State {
    last_attempt_at: i64,
    last_success_at: i64,
    next_check_at: i64,
    retry_at: i64,
    last_error: Option<String>,
    release: Value,
    skipped_version: String,
    extra: Map<String, Value>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            last_attempt_at: 0,
            last_success_at: 0,
            next_check_at: 0,
            retry_at: 0,
            last_error: None,
            release: Value::Null,
            skipped_version: String::new(),
            extra: Map::new(),
        }
    }
}

impl State {
    fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("lastAttemptAt".into(), json!(self.last_attempt_at));
        map.insert("lastSuccessAt".into(), json!(self.last_success_at));
        map.insert("nextCheckAt".into(), json!(self.next_check_at));
        map.insert("retryAt".into(), json!(self.retry_at));
        map.insert(
            "lastError".into(),
            match &self.last_error {
                Some(text) => Value::String(text.clone()),
                None => Value::Null,
            },
        );
        map.insert("release".into(), self.release.clone());
        map.insert(
            "skippedVersion".into(),
            Value::String(self.skipped_version.clone()),
        );
        for (key, value) in &self.extra {
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }
}

fn state_path(root: &str) -> PathBuf {
    Path::new(root).join(STATE_FILE)
}

/// `readState()` — base defaults, overridden by whatever the file has.
///
/// The reference spreads the parsed object over the defaults; unknown keys therefore
/// survive and are written back. A corrupt/missing file yields the defaults
/// (a torn state file must never brick boot).
fn read_state(root: &str) -> State {
    let mut state = State::default();
    let Ok(bytes) = std::fs::read(state_path(root)) else {
        return state;
    };
    let Ok(Value::Object(raw)) = serde_json::from_slice::<Value>(&bytes) else {
        return state;
    };
    for (key, value) in raw {
        match key.as_str() {
            // `Number` fields are coerced; a hand-edited string would make the
            // strings are concatenated in the schedule
            // math. Hardening this is intentional (see the module docs).
            "lastAttemptAt" => state.last_attempt_at = value.as_i64().unwrap_or(0),
            "lastSuccessAt" => state.last_success_at = value.as_i64().unwrap_or(0),
            "nextCheckAt" => state.next_check_at = value.as_i64().unwrap_or(0),
            "retryAt" => state.retry_at = value.as_i64().unwrap_or(0),
            "lastError" => {
                state.last_error = match value {
                    Value::String(text) => Some(text),
                    Value::Null => None,
                    other => Some(js_to_string(&other)),
                }
            }
            "release" => state.release = value,
            "skippedVersion" => {
                state.skipped_version = match value {
                    Value::String(text) => text,
                    Value::Null => String::new(),
                    other => js_to_string(&other),
                }
            }
            _ => {
                state.extra.insert(key, value);
            }
        }
    }
    state
}

fn persist_state(root: &str, state: &State, log: &LogFn) {
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(&state.to_value()).unwrap_or_else(|_| "{}".to_string())
    );
    if let Err(e) = settings_store::atomic_write(&state_path(root), &body) {
        log(&format!("update state write failed: {}", e));
    }
}

/* ------------------------------------------------------------------ */
/* manager                                                             */
/* ------------------------------------------------------------------ */

/// Response of one release query.
///
/// The manager needs the status (404 = "no release yet", 403/429 = cooldown)
/// and the headers (`Retry-After`, `X-RateLimit-*`), so it uses
/// `http::fetch_release` and its [`ReleaseFetch`] rather than the body-only
/// `fetch_release_json`.
type ClockFn = Arc<dyn Fn() -> i64 + Send + Sync>;
type FetchFn = Arc<dyn Fn(&str, Option<&str>) -> Result<ReleaseFetch, UpdateError> + Send + Sync>;
type DownloadFn = Arc<
    dyn Fn(
            &str,
            &Path,
            &str,
            u64,
            u64,
            &mut dyn FnMut(u64, Option<u64>) -> bool,
        ) -> Result<u64, FetchError>
        + Send
        + Sync,
>;
/// The launcher injection point, made public so an integration test can assert
/// `install()` without actually executing a file. Production always passes
/// `None` and gets [`default_launcher`].
pub type LauncherFn = Arc<dyn Fn(&Path, bool) -> Result<String, UpdateError> + Send + Sync>;
type LaunchFn = LauncherFn;
type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// The manager's settings view (`settings()`).
#[derive(Debug, Clone)]
struct Cfg {
    auto: bool,
    minutes: i64,
    beta: bool,
    repo: String,
    api_base: String,
    proxy: String,
    silent: bool,
}

/// Constructor knobs that are not injection points (those are fields, so the
/// tests can replace one at a time).
///
/// `Debug` is hand-written: `LauncherFn` is a `dyn Fn`, which is not `Debug`.
#[derive(Clone)]
pub struct Options {
    pub platform: String,
    pub arch: String,
    pub app_version: String,
    /// `options.dir` — defaults to `<root>/updates`.
    pub dir: Option<PathBuf>,
    /// `options.maxBytes`; 0 means the reference default (`|| MAX_INSTALLER_BYTES`).
    pub max_bytes: u64,
    /// `options.launcher`. `None` =
    /// [`default_launcher`], which really executes the installer — tests set it
    /// so `install()` can be asserted without spawning a process.
    pub launcher: Option<LauncherFn>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("platform", &self.platform)
            .field("arch", &self.arch)
            .field("app_version", &self.app_version)
            .field("dir", &self.dir)
            .field("max_bytes", &self.max_bytes)
            .field("launcher", &self.launcher.is_some())
            .finish()
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            platform: node_platform().to_string(),
            arch: node_arch().to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            dir: None,
            max_bytes: 0,
            launcher: None,
        }
    }
}

struct Inner {
    state: State,
    task: Option<Value>,
}

struct Inflight {
    running: bool,
    /// The value the in-flight check resolves to. A second caller waits for it
    /// instead of starting a second request (the reference shares the promise).
    result: Option<Value>,
}

/// The update manager: one per process, like the reference `buildContext()`.
pub struct UpdateManager {
    root: String,
    update_dir: PathBuf,
    platform: String,
    arch: String,
    app_version: String,
    max_bytes: u64,

    inner: Arc<Mutex<Inner>>,
    inflight: Mutex<Inflight>,
    inflight_cv: Condvar,
    token: TokenStore,

    // Injection points, mirroring the reference constructor (`http`, `now`,
    // `launcher`, `logger`). Tests replace them after construction.
    now: ClockFn,
    fetch: FetchFn,
    downloader: DownloadFn,
    launcher: LaunchFn,
    logger: LogFn,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl UpdateManager {
    /// `createUpdateManager({ store })` — production defaults.
    pub fn new(root: &str) -> Self {
        Self::with_options(root, Options::default())
    }

    pub fn with_options(root: &str, options: Options) -> Self {
        let update_dir = options
            .dir
            .unwrap_or_else(|| Path::new(root).join(UPDATES_DIR));
        let max_bytes = if options.max_bytes == 0 {
            version::MAX_INSTALLER_BYTES
        } else {
            options.max_bytes
        };
        let state = read_state(root);
        let launcher_platform = options.platform.clone();
        let launcher_override = options.launcher.clone();

        Self {
            root: root.to_string(),
            update_dir,
            platform: options.platform,
            arch: options.arch,
            app_version: options.app_version,
            max_bytes,
            inner: Arc::new(Mutex::new(Inner { state, task: None })),
            inflight: Mutex::new(Inflight {
                running: false,
                result: None,
            }),
            inflight_cv: Condvar::new(),
            token: TokenStore::new(root),
            now: Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            }),
            fetch: Arc::new(|url, token| {
                update_http::fetch_release(url, token, NETWORK_TIMEOUT_MS)
            }),
            downloader: Arc::new(|url, dest, proxy, max_bytes, timeout_ms, on_progress| {
                crate::http::download_to_file(url, dest, proxy, max_bytes, timeout_ms, on_progress)
            }),
            // The launcher never captures `silent` from construction time: the
            // setting is read on every install, like `defaultLauncher`.
            launcher: launcher_override.unwrap_or_else(|| {
                Arc::new(move |file, silent| default_launcher(file, &launcher_platform, silent))
            }),
            logger: Arc::new(|_| {}),
        }
    }

    fn log(&self, message: &str) {
        (self.logger)(message);
    }

    fn mutate_state(&self, f: impl FnOnce(&mut State)) {
        let mut inner = lock(&self.inner);
        f(&mut inner.state);
        persist_state(&self.root, &inner.state, &self.logger);
    }

    /* ---------------- settings ---------------- */

    /// `settings()` — re-read from disk on every call.
    fn settings_cfg(&self) -> Cfg {
        let s = settings_store::read(&self.root);
        let text = |key: &str| s.get(key).and_then(|v| v.as_str()).unwrap_or("");

        let repo = normalize_repo(text("updateRepo"));
        let api_base = ecma_trim(text("updateApiBase")).trim_end_matches('/');
        let proxy = ecma_trim(text("updateProxy"));

        Cfg {
            auto: s.get("updateCheck") == Some(&Value::Bool(true)),
            minutes: clamp_minutes(s.get("updateCheckMinutes").unwrap_or(&Value::Null)),
            beta: s.get("updateBeta") == Some(&Value::Bool(true)),
            repo: if repo.is_empty() {
                BUILT_IN_REPO.to_string()
            } else {
                repo
            },
            api_base: api_base.to_string(),
            proxy: if proxy.is_empty() {
                "auto".to_string()
            } else {
                proxy.to_string()
            },
            silent: s.get("updateInstallSilent") != Some(&Value::Bool(false)),
        }
    }

    /// The `settings()` shape (also useful for tests).
    pub fn settings(&self) -> Value {
        let cfg = self.settings_cfg();
        json!({
            "auto": cfg.auto,
            "minutes": cfg.minutes,
            "beta": cfg.beta,
            "repo": cfg.repo,
            "apiBase": cfg.api_base,
            "proxy": cfg.proxy,
            "silent": cfg.silent
        })
    }

    fn api_root(&self, cfg: &Cfg) -> String {
        if cfg.api_base.is_empty() {
            version::GITHUB_API.to_string()
        } else {
            cfg.api_base.clone()
        }
    }

    pub fn download_dir(&self) -> String {
        self.update_dir.to_string_lossy().into_owned()
    }

    /* ---------------- status ---------------- */

    /// The snapshot the UI polls. Every field is present on every call.
    pub fn status(&self) -> Value {
        let cfg = self.settings_cfg();
        let (state, task) = {
            let inner = lock(&self.inner);
            (inner.state.clone(), inner.task.clone())
        };
        let checking = lock(&self.inflight).running;

        // `state.release || null` — anything falsy reads as "not checked yet".
        let release = if js_truthy(&state.release) {
            Some(&state.release)
        } else {
            None
        };
        let release_field = |key: &str| release.and_then(|r| r.get(key)).cloned();

        let tag = js_string_or_empty(release_field("tag").as_ref());
        let order = if tag.is_empty() {
            None
        } else {
            version::compare_versions(&tag, &self.app_version)
        };
        let has_update = match order {
            None => Value::Null,
            Some(ord) => Value::Bool(ord == Ordering::Greater),
        };
        let comparison = match order {
            None => Value::Null,
            Some(Ordering::Greater) => Value::String("newer".into()),
            Some(Ordering::Less) => Value::String("older".into()),
            Some(Ordering::Equal) => Value::String("same".into()),
        };

        let asset = release_field("asset").unwrap_or(Value::Null);
        let asset_ready = release.is_some()
            && js_truthy(&asset)
            && js_truthy(asset.get("url").unwrap_or(&Value::Null));

        let skipped = release.is_some() && !tag.is_empty() && tag == state.skipped_version;

        json!({
            "ok": true,
            "current": self.app_version,
            "repo": cfg.repo,
            "apiBase": cfg.api_base,
            "latest": if release.is_some() { release_field("tag").unwrap_or(Value::Null) } else { Value::Null },
            "hasUpdate": has_update,
            "comparison": comparison,
            "releaseName": release_field("name").unwrap_or(Value::Null),
            "notes": release_field("notes").unwrap_or_else(|| Value::String(String::new())),
            "publishedAt": release_field("publishedAt").unwrap_or(Value::Null),
            "pageUrl": release_field("pageUrl").unwrap_or(Value::Null),
            "prerelease": release.is_some()
                && js_truthy(release_field("prerelease").as_ref().unwrap_or(&Value::Null)),
            "asset": if asset_ready { asset } else { Value::Null },
            "downloadSupported": asset_ready,
            "installerKind": version::installer_kind(&self.platform),
            "downloadDir": self.download_dir(),
            "checked": state.last_success_at > 0,
            "checkedAt": state.last_success_at,
            "lastAttemptAt": state.last_attempt_at,
            "nextCheckAt": state.next_check_at,
            "retryAt": state.retry_at,
            "lastError": match &state.last_error {
                Some(text) => Value::String(text.clone()),
                None => Value::Null,
            },
            "checking": checking,
            "skipped": skipped,
            "skippedVersion": state.skipped_version,
            "task": task.unwrap_or(Value::Null),
            "token": self.token.status(),
            "encryption": self.token.encryption_available(),
            "settings": {
                "auto": cfg.auto,
                "minutes": cfg.minutes,
                "beta": cfg.beta,
                "proxy": cfg.proxy,
                "silent": cfg.silent
            }
        })
    }

    /// `_state()` — the persisted object, for parity tests.
    pub fn state_snapshot(&self) -> Value {
        lock(&self.inner).state.to_value()
    }

    /// `tokenStatus()` — the token envelope's own view
    /// (filled/origin/encryption), independent of a check.
    ///
    /// Kept public although only `tests/update_pipeline.rs` calls it today:
    /// it is the only way to observe a stored token without a network round,
    /// which is what makes "clear" assertable.
    pub fn token_status(&self) -> Value {
        self.token.status()
    }

    /* ---------------- schedule ---------------- */

    /// When the scheduler should next run a check (0 = due now / not scheduled).
    pub fn due_at(&self) -> i64 {
        let cfg = self.settings_cfg();
        if !cfg.auto {
            return 0;
        }
        self.next_due(&cfg)
    }

    fn next_due(&self, cfg: &Cfg) -> i64 {
        let inner = lock(&self.inner);
        if inner.state.next_check_at != 0 {
            return inner.state.next_check_at;
        }
        if inner.state.last_attempt_at != 0 {
            return inner.state.last_attempt_at + cfg.minutes * 60_000;
        }
        0
    }

    /// Scheduler tick (called every 30 s). `None` = nothing to do.
    pub fn tick(&self) -> Option<Value> {
        let cfg = self.settings_cfg();
        if !cfg.auto {
            return None;
        }
        if lock(&self.inflight).running {
            return None;
        }
        let due = self.next_due(&cfg);
        if due != 0 && (self.now)() < due {
            return None;
        }
        Some(self.check(false))
    }

    /* ---------------- check ---------------- */

    /// `check({ manual })`.
    ///
    /// Manual and scheduled checks share this path; manual ignores
    /// `nextCheckAt` but still respects the 60 s gap against `lastAttemptAt`.
    pub fn check(&self, manual: bool) -> Value {
        // Concurrent callers share the in-flight result (`if (checking) return
        // checking`). Waiting here is the blocking equivalent of awaiting the
        // same promise.
        let mut inflight = lock(&self.inflight);
        if inflight.running {
            while inflight.result.is_none() {
                inflight = self
                    .inflight_cv
                    .wait(inflight)
                    .unwrap_or_else(|e| e.into_inner());
            }
            return inflight.result.clone().unwrap_or(Value::Null);
        }
        drop(inflight);

        let cfg = self.settings_cfg();
        let started_at = (self.now)();

        {
            let inner = lock(&self.inner);
            // A cooldown answer came from GitHub; retrying early only earns
            // another 403.
            if inner.state.retry_at > started_at {
                let retry_at = inner.state.retry_at;
                drop(inner);
                let mut value = self.status();
                set_key(&mut value, "ok", Value::Bool(false));
                set_key(&mut value, "reason", json!("cooldown"));
                set_key(&mut value, "retryAt", json!(retry_at));
                return value;
            }
            // The 60 s gap only applies when a previous check *succeeded*.
            if manual
                && inner.state.last_success_at != 0
                && started_at - inner.state.last_attempt_at < MIN_CHECK_GAP_MS
            {
                drop(inner);
                let mut value = self.status();
                set_key(&mut value, "ok", Value::Bool(true));
                set_key(&mut value, "reason", json!("throttled"));
                set_key(&mut value, "reused", Value::Bool(true));
                return value;
            }
        }

        // Claim the slot. A racing caller gets the result this one produces.
        let mut inflight = lock(&self.inflight);
        if inflight.running {
            while inflight.result.is_none() {
                inflight = self
                    .inflight_cv
                    .wait(inflight)
                    .unwrap_or_else(|e| e.into_inner());
            }
            return inflight.result.clone().unwrap_or(Value::Null);
        }
        inflight.running = true;
        inflight.result = None;
        drop(inflight);

        // `writeState({ lastAttemptAt: startedAt })` happens before the fetch:
        // a crash mid-request still counts as an attempt for the scheduler.
        self.mutate_state(|state| state.last_attempt_at = started_at);

        let result = match self.fetch_release(&cfg) {
            Ok((release, retry_at)) => {
                let finished_at = (self.now)();
                self.mutate_state(|state| {
                    state.last_success_at = finished_at;
                    state.next_check_at = finished_at + cfg.minutes * 60_000;
                    state.retry_at = retry_at;
                    state.last_error = None;
                    state.release = release;
                });
                let value = self.status();
                if value.get("hasUpdate") == Some(&Value::Bool(true)) {
                    self.log(&format!(
                        "[update] found a new version {}",
                        js_string_or_empty(value.get("latest"))
                    ));
                } else {
                    self.log("[update] version check finished");
                }
                value
            }
            Err(failure) => {
                let finished_at = (self.now)();
                self.mutate_state(|state| {
                    state.next_check_at = finished_at + cfg.minutes * 60_000;
                    state.retry_at = failure.retry_at;
                    state.last_error = Some(failure.message.clone());
                });
                self.log(&format!("[update] check failed: {}", failure.message));
                let mut value = self.status();
                set_key(&mut value, "ok", Value::Bool(false));
                set_key(&mut value, "reason", json!("error"));
                set_key(&mut value, "error", Value::String(failure.message));
                value
            }
        };

        let mut inflight = lock(&self.inflight);
        inflight.running = false;
        inflight.result = Some(result.clone());
        self.inflight_cv.notify_all();
        result
    }

    fn fetch_release(&self, cfg: &Cfg) -> Result<(Value, i64), FetchFailure> {
        let base = self.api_root(cfg);
        let encoded_repo = cfg.repo.clone(); // normalizeRepo already restricts the charset
        let only_prerelease = cfg.beta && version::is_prerelease_version(&self.app_version);
        let url = if cfg.beta {
            format!("{}/repos/{}/releases?per_page=30", base, encoded_repo)
        } else {
            format!("{}/repos/{}/releases/latest", base, encoded_repo)
        };

        let token = self.token.effective();
        // `update/http.rs` deliberately takes no proxy setting (direct route
        // only); `updateProxy` therefore affects downloads but not checks.
        // Documented gap, not an oversight here.
        let response = (self.fetch)(&url, token.as_deref()).map_err(|error| FetchFailure {
            message: error.message().to_string(),
            // A transport/parse failure happens before `retryDeadline`, so the
            // 所以错误里没有 `retryAt`。
            retry_at: 0,
        })?;

        let headers: Vec<(String, String)> = response
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        let deadline = retry_deadline(&headers, response.status, (self.now)());

        if response.status == 404 {
            // A repo with no release yet, or a mirror that only implements
            // `/releases/latest`. "Nothing to upgrade to", not a failure.
            return Ok((Value::Null, deadline));
        }
        if response.status == 403 || response.status == 429 {
            return Err(FetchFailure {
                message: "GitHub API rate limit reached; checks are paused and the last result kept. Add a GitHub token in update settings to raise the limit"
                    .to_string(),
                retry_at: deadline,
            });
        }
        if !(200..300).contains(&response.status) {
            return Err(FetchFailure {
                message: format!("GitHub returned HTTP {}", response.status),
                retry_at: deadline,
            });
        }

        let payload = response.json.unwrap_or(Value::Null);
        let raw = if cfg.beta {
            version::pick_listed_release(&payload, only_prerelease).cloned()
        } else {
            Some(payload)
        };
        let Some(raw) = raw.filter(js_truthy) else {
            return Ok((Value::Null, deadline));
        };

        let release = version::release_from_api(
            &raw,
            ReleaseOptions {
                platform: &self.platform,
                arch: &self.arch,
                api_base: &cfg.api_base,
                repo: &cfg.repo,
            },
        );
        Ok((release, deadline))
    }

    /// `clearCooldown()` — a route/credential change invalidates a cooldown
    /// earned by another quota bucket.
    pub fn clear_cooldown(&self) -> Value {
        let dirty = {
            let inner = lock(&self.inner);
            inner.state.retry_at != 0 || inner.state.last_error.is_some()
        };
        if dirty {
            self.mutate_state(|state| {
                state.retry_at = 0;
                state.last_error = None;
            });
        }
        self.status()
    }

    /* ---------------- download ---------------- */

    /// `startDownload` — returns the initial task snapshot and transfers on a
    /// background thread (the renderer polls `progress`).
    ///
    /// A second call while a task is active returns the running snapshot
    /// instead of starting another transfer; URL validation happens after that
    /// check, same order as upstream.
    pub fn start_download(&self, url: &str, name: Option<&str>) -> Result<Value, UpdateError> {
        {
            let inner = lock(&self.inner);
            if let Some(task) = &inner.task {
                if task.get("active") == Some(&Value::Bool(true)) {
                    return Ok(task.clone());
                }
            }
        }

        let cfg = self.settings_cfg();
        let target = version::assert_downloadable(url, &cfg.api_base)?;

        let last_segment = target.pathname.rsplit('/').next().unwrap_or("");
        let fallback_name = decode_uri_component(last_segment)
            .map_err(|_| UpdateError::new("URI malformed", 500))?;
        // `name || fallbackName` — an empty string falls through.
        let chosen = match name {
            Some(text) if !text.is_empty() => text,
            _ => &fallback_name,
        };
        let filename = version::safe_file_name(chosen, &self.platform);

        let _ = std::fs::create_dir_all(&self.update_dir);
        let file_path = self.update_dir.join(&filename);
        // Same-version re-download overwrites; a stale copy otherwise lingers.
        let _ = std::fs::remove_file(&file_path);

        let task = json!({
            "active": true,
            "done": false,
            "canceled": false,
            "error": Value::Null,
            "filename": filename,
            "path": path_string(&file_path),
            "received": 0,
            "total": 0,
            "percent": 0
        });

        {
            let mut inner = lock(&self.inner);
            // Re-check after building: two racing clicks must not spawn two
            // transfers. (The reference would also let both through in theory; this
            // is the one place a second transfer would be strictly worse.)
            if let Some(running) = &inner.task {
                if running.get("active") == Some(&Value::Bool(true)) {
                    return Ok(running.clone());
                }
            }
            inner.task = Some(task.clone());
        }

        let inner = Arc::clone(&self.inner);
        let downloader = Arc::clone(&self.downloader);
        let logger = Arc::clone(&self.logger);
        let file_for_thread = file_path.clone();
        let url = target.href.clone();
        let proxy = cfg.proxy.clone();
        let max_bytes = self.max_bytes;
        let root = self.root.clone();

        std::thread::spawn(move || {
            run_download(
                inner,
                downloader,
                logger,
                root,
                url,
                file_for_thread,
                proxy,
                max_bytes,
            );
        });

        Ok(task)
    }

    /// `progress()` / `taskSnapshot()` — the task or `null`.
    pub fn progress(&self) -> Value {
        lock(&self.inner).task.clone().unwrap_or(Value::Null)
    }

    /// `cancel()` — only an active task can be cancelled.
    pub fn cancel(&self) -> Value {
        let mut inner = lock(&self.inner);
        if let Some(task) = inner.task.as_mut().and_then(Value::as_object_mut) {
            if task.get("active") == Some(&Value::Bool(true)) {
                task.insert("canceled".to_string(), Value::Bool(true));
                return json!({ "canceled": true });
            }
        }
        json!({ "canceled": false })
    }

    /* ---------------- install ---------------- */

    /// Validate a path the renderer wants to execute.
    ///
    /// Four conditions, all required: absolute, this platform's extension,
    /// inside the managed download directory, and the product keyword from
    /// `updateAssetKeyword` in the filename. Returns the real path.
    pub fn verify_installer(&self, raw_path: &str) -> Result<PathBuf, UpdateError> {
        let text = ecma_trim(raw_path);
        if text.is_empty() {
            return Err(UpdateError::new("installer path is empty", 400));
        }
        if !is_absolute_js(text) {
            return Err(UpdateError::new("installer path must be absolute", 400));
        }

        let ext = Path::new(text)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let allowed = version::allowed_installer_suffixes(&self.platform);
        if !allowed.contains(&ext.as_str()) {
            let list = allowed
                .iter()
                .map(|s| format!(".{}", s))
                .collect::<Vec<_>>()
                .join(" / ");
            return Err(UpdateError::new(
                format!("only {} installers may be run", list),
                400,
            ));
        }

        // `fs.realpathSync` + `statSync(...).isFile()`; both failures share one
        // message. `canonicalize` follows symlinks like realpath does.
        let Ok(real) = std::fs::canonicalize(text) else {
            return Err(UpdateError::new(
                "installer is missing or is not a file",
                400,
            ));
        };
        if !real.is_file() {
            return Err(UpdateError::new(
                "installer is missing or is not a file",
                400,
            ));
        }
        let real = strip_verbatim_prefix(real);

        // `path.relative(rootReal, real)` starts with ".." → outside.
        let root_real = if self.update_dir.exists() {
            std::fs::canonicalize(&self.update_dir)
                .map(strip_verbatim_prefix)
                .unwrap_or_else(|_| resolve_lexical(&self.update_dir))
        } else {
            resolve_lexical(&self.update_dir)
        };
        if !relative_stays_inside(&root_real, &real) {
            return Err(UpdateError::new(
                "installer is outside the managed download directory; execution refused",
                400,
            ));
        }

        // Not upstream's rule: a misconfigured `updateRepo` must not install
        // *another* project's installer.
        let settings = settings_store::read(&self.root);
        let raw_keyword = settings
            .get("updateAssetKeyword")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let keyword = if raw_keyword.is_empty() {
            "AgentKeeper"
        } else {
            raw_keyword
        };
        let keyword = ecma_trim(keyword);
        if !keyword.is_empty() {
            let base = real
                .file_name()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if !base.contains(&keyword.to_lowercase()) {
                return Err(UpdateError::new(
                    format!("installer filename does not contain \"{}\"; it looks like another project, so execution was refused", keyword),
                    400,
                ));
            }
        }

        Ok(real)
    }

    /// Launch a downloaded installer. The caller quits the app right after.
    ///
    /// Order is the reference order: verify the path, *then* refuse while a download
    /// is active.
    pub fn install(&self, raw_path: &str) -> Result<Value, UpdateError> {
        let file = self.verify_installer(raw_path)?;
        let active = {
            let inner = lock(&self.inner);
            inner
                .task
                .as_ref()
                .map(|task| task.get("active") == Some(&Value::Bool(true)))
                .unwrap_or(false)
        };
        if active {
            return Err(UpdateError::new(
                "the installer is still downloading; wait for the download to finish before installing",
                400,
            ));
        }
        let silent = self.settings_cfg().silent;
        let mode = (self.launcher)(&file, silent)?;
        self.log(&format!(
            "[update] installer launched: {}",
            path_string(&file)
        ));
        Ok(json!({
            "ok": true,
            "restarted": true,
            "path": path_string(&file),
            "mode": if mode.is_empty() { "unknown".to_string() } else { mode }
        }))
    }

    /* ---------------- settings / token / skip ---------------- */

    /// `ak:update-settings`, guard and write as one step — what `lib.rs` calls.
    ///
    /// The command functions are private to the crate, so a test cannot reach
    /// `update_save_settings`; without this the ordering — guard first, write
    /// second — is four lines nothing can turn red. It is also defence in
    /// depth: a future Rust-side caller gets the same rules, not just the
    /// renderer's path.
    pub fn save_settings_checked(&self, patch: &Value) -> Result<Value, UpdateError> {
        if let Some(message) = validate_settings_patch(patch) {
            return Err(UpdateError::new(message, 400));
        }
        self.save_settings(patch)
    }

    /// `saveSettings(patch)` — clamp/trim, persist, adjust the schedule.
    ///
    /// The IPC layer validates the raw payload first and returns
    /// `{ ok:false, error }`; this method is the manager half. Note that it
    /// *clamps* rather than rejects: `minutes: 0` becomes `1`, and a malformed
    /// `repo` is stored verbatim and only reads back as the built-in one.
    /// Rejecting is the guard's job, not this method's.
    pub fn save_settings(&self, patch: &Value) -> Result<Value, UpdateError> {
        let mut out = Map::new();
        if let Some(value) = patch.get("auto") {
            out.insert(
                "updateCheck".into(),
                Value::Bool(value == &Value::Bool(true)),
            );
        }
        if let Some(value) = patch.get("minutes") {
            out.insert("updateCheckMinutes".into(), json!(clamp_minutes(value)));
        }
        if let Some(value) = patch.get("beta") {
            out.insert(
                "updateBeta".into(),
                Value::Bool(value == &Value::Bool(true)),
            );
        }
        if let Some(value) = patch.get("repo") {
            out.insert(
                "updateRepo".into(),
                Value::String(ecma_trim(&js_string_or_empty(Some(value))).to_string()),
            );
        }
        if let Some(value) = patch.get("apiBase") {
            out.insert(
                "updateApiBase".into(),
                Value::String(ecma_trim(&js_string_or_empty(Some(value))).to_string()),
            );
        }
        if let Some(value) = patch.get("proxy") {
            let text = js_string_or_empty(Some(value));
            let proxy = ecma_trim(&text);
            out.insert(
                "updateProxy".into(),
                Value::String(if proxy.is_empty() {
                    "auto".to_string()
                } else {
                    proxy.to_string()
                }),
            );
        }
        if let Some(value) = patch.get("silent") {
            out.insert(
                "updateInstallSilent".into(),
                Value::Bool(value == &Value::Bool(true)),
            );
        }

        let result = settings_store::save(&self.root, &Value::Object(out.clone()));
        if result.get("ok") == Some(&Value::Bool(false)) {
            return Err(UpdateError::new("failed to save settings", 500));
        }

        // Flipping the switch clears the schedule: off → nothing runs; on with
        // no pending deadline → the next tick queries at once.
        if out.contains_key("updateCheck") {
            self.mutate_state(|state| state.next_check_at = 0);
        }
        if out.contains_key("updateCheckMinutes") {
            let last_attempt = lock(&self.inner).state.last_attempt_at;
            if last_attempt != 0 {
                let minutes = self.settings_cfg().minutes;
                self.mutate_state(|state| {
                    state.next_check_at = last_attempt + minutes * 60_000;
                });
            }
        }
        // A route change invalidates a cooldown earned by another bucket.
        if patch.get("proxy").is_some() {
            self.clear_cooldown();
        }

        Ok(self.status())
    }

    /// `ak:update-token`, guard and write as one step — what `lib.rs` calls.
    ///
    /// Same reason as [`UpdateManager::save_settings_checked`]. BUG-1 had two
    /// halves and this is the only seam where both are assertable: the body
    /// must survive `null`, *and* `None` must reach `set_token` untouched —
    /// the original `set_token(Some(&token))` is what made `None` unreachable.
    pub fn set_token_checked(&self, payload: &Value) -> Result<Value, UpdateError> {
        let token = token_arg(payload).map_err(|message| UpdateError::new(message, 400))?;
        self.set_token(token.as_deref())
    }

    /// `setToken(token)` — `None` clears (`token === null`).
    pub fn set_token(&self, token: Option<&str>) -> Result<Value, UpdateError> {
        let result = self.token.set(token)?;
        if result.changed {
            self.clear_cooldown();
        }
        let mut value = self.status();
        set_key(&mut value, "saved", Value::Bool(result.saved));
        set_key(&mut value, "changed", Value::Bool(result.changed));
        Ok(value)
    }

    /// "Skip this version" — remembered by version in `update-state.json`.
    pub fn skip(&self, version: Option<&str>) -> Value {
        let requested = match version {
            Some(text) if !text.is_empty() => text.to_string(),
            _ => js_string_or_empty(lock(&self.inner).state.release.get("tag")),
        };
        let tag = ecma_trim(&requested).to_string();
        self.mutate_state(|state| state.skipped_version = tag);
        self.status()
    }

    pub fn unskip(&self) -> Value {
        self.mutate_state(|state| state.skipped_version = String::new());
        self.status()
    }
}

/// The failure a release query produces, with the cooldown the reference attaches to
/// its `UpdateError` (which `UpdateError` itself cannot carry).
struct FetchFailure {
    message: String,
    retry_at: i64,
}

fn set_key(value: &mut Value, key: &str, replacement: Value) {
    if let Some(map) = value.as_object_mut() {
        map.insert(key.to_string(), replacement);
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/* ------------------------------------------------------------------ */
/* download worker                                                     */
/* ------------------------------------------------------------------ */

/// The background half of `startDownload`.
///
/// The progress callback both reports and enforces the size ceiling: the
/// throws from inside `getStream`'s callbacks, so the ceiling is checked *here*
/// with the exact same wording, not by the HTTP layer.
fn run_download(
    inner: Arc<Mutex<Inner>>,
    downloader: DownloadFn,
    logger: LogFn,
    root: String,
    url: String,
    file_path: PathBuf,
    proxy: String,
    max_bytes: u64,
) {
    let mut declared: u64 = 0;
    let mut abort_reason: Option<String> = None;

    let result = {
        let mut on_progress = |received: u64, total: Option<u64>| -> bool {
            let mut guard = lock(&inner);
            let Some(task) = guard.task.as_mut().and_then(Value::as_object_mut) else {
                return false;
            };
            if task.get("canceled") == Some(&Value::Bool(true)) {
                return false;
            }
            // Checked before the progress update, exactly as specified.
            if received > max_bytes {
                abort_reason = Some("installer exceeds the size cap; download aborted".to_string());
                return false;
            }
            if let Some(total) = total {
                if total > max_bytes {
                    let megabytes = (total as f64 / 1_048_576.0).round() as u64;
                    abort_reason = Some(format!(
                        "installer size looks wrong ({} MB); download aborted",
                        megabytes
                    ));
                    return false;
                }
                if total > 0 {
                    declared = total;
                }
            }

            // `updateProgress(received, declared)`.
            let total = total.unwrap_or(0);
            task.insert("received".into(), json!(received));
            let current_total = task.get("total").and_then(Value::as_u64).unwrap_or(0);
            if total > 0 {
                task.insert("total".into(), json!(total));
            } else if current_total == 0 {
                task.insert("total".into(), json!(received));
            }
            let total = task.get("total").and_then(Value::as_u64).unwrap_or(0);
            let percent = if total > 0 {
                ((received as f64 / total as f64) * 100.0)
                    .round()
                    .min(100.0) as u64
            } else {
                0
            };
            task.insert("percent".into(), json!(percent));
            true
        };
        (downloader)(
            &url,
            &file_path,
            &proxy,
            max_bytes,
            DOWNLOAD_TIMEOUT_MS,
            &mut on_progress,
        )
    };

    let task_canceled = || {
        lock(&inner)
            .task
            .as_ref()
            .map(|task| task.get("canceled") == Some(&Value::Bool(true)))
            .unwrap_or(false)
    };

    match result {
        Ok(size) => {
            if task_canceled() {
                finalize_task(&inner, true, false);
                return;
            }
            if declared > 0 && size != declared {
                let message = format!(
                    "installer is incomplete (expected {} bytes, got {} bytes)",
                    declared, size
                );
                set_task_error(&inner, &message);
                logger(&format!("[update] {}", message));
                finalize_task(&inner, false, false);
                return;
            }

            // `if (task) { active = false; done = true; ... }` then
            // `writeState({ downloadedPath, downloadedVersion })`, then the
            // `finally` sets active=false. The write happens before the task
            // stops reporting `active`.
            {
                let mut guard = lock(&inner);
                if let Some(task) = guard.task.as_mut().and_then(Value::as_object_mut) {
                    task.insert("done".into(), Value::Bool(true));
                    task.insert("received".into(), json!(size));
                    task.insert("total".into(), json!(size));
                    task.insert("percent".into(), json!(100));
                    task.insert("path".into(), Value::String(path_string(&file_path)));
                }
                let tag = js_string_or_empty(guard.state.release.get("tag"));
                guard.state.extra.insert(
                    "downloadedPath".into(),
                    Value::String(path_string(&file_path)),
                );
                guard
                    .state
                    .extra
                    .insert("downloadedVersion".into(), Value::String(tag));
                persist_state(&root, &guard.state, &logger);
            }
            if let Some(task) = lock(&inner).task.as_mut().and_then(Value::as_object_mut) {
                task.insert("active".into(), Value::Bool(false));
            }
            logger(&format!(
                "[update] installer ready: {}（{:.1} MB）",
                path_string(&file_path),
                size as f64 / 1_048_576.0
            ));
        }
        Err(error) => {
            if task_canceled() {
                // 解析口径: the cancel path resolves the streaming promise and
                // returns out of the `try` before the `catch` that unlinks the
                // partial file, so the partial installer survives. Only a
                // failure deletes. The task still reports path=null.
                finalize_task(&inner, true, false);
                return;
            }
            let _ = std::fs::remove_file(&file_path);
            let message = abort_reason.unwrap_or_else(|| match &error {
                FetchError::Http(code) => {
                    format!("download failed: the server returned HTTP {}", code)
                }
                FetchError::Timeout => "cannot reach the server: connection timed out".to_string(),
                // 传输层措辞由 `update/http.rs` 负责（`describeError` 的口径）；
                // 这里直传，保持单一事实来源。
                FetchError::Transport(text) => text.clone(),
            });
            set_task_error(&inner, &message);
            logger(&format!("[update] {}", message));
            finalize_task(&inner, false, false);
        }
    }
}

/// `finally` for the task: active=false, and the terminal flags.
fn finalize_task(inner: &Arc<Mutex<Inner>>, canceled: bool, success: bool) {
    let mut guard = lock(inner);
    if let Some(task) = guard.task.as_mut().and_then(Value::as_object_mut) {
        task.insert("active".into(), Value::Bool(false));
        if canceled {
            task.insert("canceled".into(), Value::Bool(true));
            task.insert("done".into(), Value::Bool(false));
            task.insert("path".into(), Value::Null);
        } else if !success {
            task.insert("path".into(), Value::Null);
        }
    }
}

fn set_task_error(inner: &Arc<Mutex<Inner>>, message: &str) {
    if let Some(task) = lock(inner).task.as_mut().and_then(Value::as_object_mut) {
        task.insert("error".into(), Value::String(message.to_string()));
    }
}

/* ------------------------------------------------------------------ */
/* launcher                                                            */
/* ------------------------------------------------------------------ */

/// Hand the installer to the OS.
///
/// Windows + silent uses NSIS `/S /R` (silent, relaunch after install). There
/// is no OS open-path API in `core/`, so every other case is a plain spawn and
/// reports `mode: "spawn"` (the renderer only displays the mode, never
/// branches on it).
fn default_launcher(file: &Path, platform: &str, silent: bool) -> Result<String, UpdateError> {
    use std::process::{Command, Stdio};

    let spawn = |args: &[&str]| -> Result<String, UpdateError> {
        let mut command = Command::new(file);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(_child) => Ok(if args.is_empty() {
                "spawn".to_string()
            } else {
                "nsis-silent".to_string()
            }),
            Err(error) if args.is_empty() => Err(UpdateError::new(
                format!("failed to launch the installer: {}", error),
                500,
            )),
            Err(error) => {
                let base = file
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Err(UpdateError::new(
                    format!(
                        "the silent install did not start ({}). You can run {} from the download directory manually",
                        error, base
                    ),
                    500,
                ))
            }
        }
    };

    if platform == "win32" && silent {
        spawn(&["/S", "/R"])
    } else {
        spawn(&[])
    }
}

/* ------------------------------------------------------------------ */
/* paths + URI helpers                                                 */
/* ------------------------------------------------------------------ */

/// `path.win32.isAbsolute` / `path.posix.isAbsolute`.
fn is_absolute_js(text: &str) -> bool {
    let bytes = text.as_bytes();
    if cfg!(windows) {
        // `\foo`, `/foo`, `\\server\share`, `C:\foo` — Node accepts a rooted
        // path without a drive, which `Path::is_absolute` rejects.
        (!bytes.is_empty() && (bytes[0] == b'/' || bytes[0] == b'\\'))
            || (bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'/' || bytes[2] == b'\\'))
    } else {
        text.starts_with('/')
    }
}

/// `fs.realpathSync` on Windows returns `C:\…`; `std::fs::canonicalize` adds
/// the verbatim prefix. Strip it so the value handed to the renderer and to
/// the launcher matches.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().into_owned();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{}", rest));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

/// `path.resolve` for a path that may not exist: lexical `.`/`..` folding.
fn resolve_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(&out))
            .unwrap_or(out)
    } else {
        out
    }
}

fn comparable_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| {
            let text = component.as_os_str().to_string_lossy().into_owned();
            if cfg!(windows) {
                text.to_lowercase()
            } else {
                text
            }
        })
        .collect()
}

/// `!relative.startsWith('..') && !path.isAbsolute(relative)`.
///
/// Two spellings of the same directory compare equal (component-wise, case
/// folded on Windows). The `..`-prefixed *filename* rejection is the 解析口径:
/// `path.relative` returns `..foo.exe` for a file literally named that, which
/// starts with `..` and is therefore refused.
fn relative_stays_inside(root: &Path, child: &Path) -> bool {
    let root_parts = comparable_components(root);
    let child_parts = comparable_components(child);
    if child_parts.len() < root_parts.len() {
        return false;
    }
    if child_parts[..root_parts.len()] != root_parts[..] {
        return false;
    }
    if child_parts.len() == root_parts.len() {
        return true;
    }
    !child_parts[root_parts.len()].starts_with("..")
}

/// `decodeURIComponent` — percent-decode, then UTF-8; malformed input errors
/// like `URIError`.
fn decode_uri_component(text: &str) -> Result<String, ()> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(());
            }
            let hi = (bytes[i + 1] as char).to_digit(16).ok_or(())?;
            let lo = (bytes[i + 2] as char).to_digit(16).ok_or(())?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

/* ------------------------------------------------------------------ */
/* process-wide singleton                                              */
/* ------------------------------------------------------------------ */

/// One manager per process, mirroring `buildContext()`.
///
/// The first root wins (production has exactly one); tests construct
/// [`UpdateManager`] directly instead.
pub fn ensure_manager(root: &str) -> &'static UpdateManager {
    static CELL: OnceLock<UpdateManager> = OnceLock::new();
    CELL.get_or_init(|| UpdateManager::new(root))
}

/* ------------------------------------------------------------------ */
/* tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::mpsc;
    use std::time::Duration;

    const NOW: i64 = 1_700_000_000_000;

    fn temp_root(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "ak-update-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    struct MockHttp {
        requests: Mutex<Vec<(String, Option<String>)>>,
        responses: Mutex<VecDeque<Result<ReleaseFetch, UpdateError>>>,
    }

    impl MockHttp {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::new()),
            })
        }

        fn push(&self, response: Result<ReleaseFetch, UpdateError>) {
            self.responses.lock().unwrap().push_back(response);
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    fn fake_response(status: u16, headers: &[(&str, &str)], json: Value) -> ReleaseFetch {
        ReleaseFetch {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>(),
            json: if (200..300).contains(&status) {
                Some(json)
            } else {
                None
            },
        }
    }

    fn manager(root: &str, app_version: &str, http: Arc<MockHttp>) -> UpdateManager {
        let mut options = Options::default();
        options.platform = "win32".into();
        options.arch = "x64".into();
        options.app_version = app_version.into();
        let mut m = UpdateManager::with_options(root, options);
        m.now = Arc::new(|| NOW);
        m.logger = Arc::new(|_| {});
        m.fetch = Arc::new(move |url, token| {
            http.requests
                .lock()
                .unwrap()
                .push((url.to_string(), token.map(|s| s.to_string())));
            match http.responses.lock().unwrap().pop_front() {
                Some(queued) => queued,
                None => Ok(fake_response(404, &[], Value::Null)),
            }
        });
        m
    }

    fn release_payload(tag: &str) -> Value {
        json!({
            "tag_name": tag,
            "name": format!("Release {}", tag),
            "body": "Release notes body",
            "published_at": "2026-01-02T03:04:05Z",
            "html_url": format!("https://github.com/acme/keeper/releases/tag/{}", tag),
            "prerelease": false,
            "assets": [
                {
                    "name": "AgentKeeper-9.9.9-setup.exe",
                    "size": 4,
                    "browser_download_url": "https://github.com/acme/keeper/releases/download/v9.9.9/AgentKeeper-9.9.9-setup.exe"
                }
            ]
        })
    }

    fn ok_release(tag: &str) -> Result<ReleaseFetch, UpdateError> {
        Ok(fake_response(200, &[], release_payload(tag)))
    }

    fn wait_inactive(m: &UpdateManager) -> Value {
        for _ in 0..4000 {
            let task = m.progress();
            if task["active"] != Value::Bool(true) {
                return task;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("download did not finish");
    }

    #[test]
    fn status_before_the_first_check_has_every_field() {
        let root = temp_root("status");
        let m = manager(&root, "0.2.0", MockHttp::new());
        let s = m.status();

        for key in [
            "ok",
            "current",
            "repo",
            "apiBase",
            "latest",
            "hasUpdate",
            "comparison",
            "releaseName",
            "notes",
            "publishedAt",
            "pageUrl",
            "prerelease",
            "asset",
            "downloadSupported",
            "installerKind",
            "downloadDir",
            "checked",
            "checkedAt",
            "lastAttemptAt",
            "nextCheckAt",
            "retryAt",
            "lastError",
            "checking",
            "skipped",
            "skippedVersion",
            "task",
            "token",
            "encryption",
            "settings",
        ] {
            assert!(s.get(key).is_some(), "status is missing {}", key);
        }

        assert_eq!(s["ok"], json!(true));
        assert_eq!(s["current"], json!("0.2.0"));
        assert_eq!(s["repo"], json!("ntc0728/agentkeeper"));
        assert_eq!(s["latest"], Value::Null);
        assert_eq!(s["hasUpdate"], Value::Null);
        assert_eq!(s["comparison"], Value::Null);
        assert_eq!(s["notes"], json!(""));
        assert_eq!(s["checked"], json!(false));
        assert_eq!(s["checkedAt"], json!(0));
        assert_eq!(s["lastAttemptAt"], json!(0));
        assert_eq!(s["nextCheckAt"], json!(0));
        assert_eq!(s["retryAt"], json!(0));
        assert_eq!(s["lastError"], Value::Null);
        assert_eq!(s["checking"], json!(false));
        assert_eq!(s["skipped"], json!(false));
        assert_eq!(s["skippedVersion"], json!(""));
        assert_eq!(s["task"], Value::Null);
        assert_eq!(s["downloadSupported"], json!(false));
        assert_eq!(s["installerKind"], json!("nsis"));
        assert_eq!(s["prerelease"], json!(false));
        // Settings defaults from `Store.defaultSettings()`.
        assert_eq!(s["settings"]["auto"], json!(true));
        assert_eq!(s["settings"]["minutes"], json!(20));
        assert_eq!(s["settings"]["beta"], json!(false));
        assert_eq!(s["settings"]["proxy"], json!("auto"));
        assert_eq!(s["settings"]["silent"], json!(true));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn invalid_repo_falls_back_to_the_built_in_repository() {
        let root = temp_root("repo");
        let m = manager(&root, "0.2.0", MockHttp::new());

        settings_store::save(&root, &json!({ "updateRepo": "not a repo" }));
        assert_eq!(m.status()["repo"], json!(BUILT_IN_REPO));

        settings_store::save(&root, &json!({ "updateRepo": "acme/keeper" }));
        assert_eq!(m.status()["repo"], json!("acme/keeper"));

        // Trailing slashes are stripped from apiBase on read.
        settings_store::save(
            &root,
            &json!({ "updateApiBase": " https://mirror.test/// " }),
        );
        assert_eq!(m.status()["apiBase"], json!("https://mirror.test"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn check_success_writes_state_and_throttles_manual_checks() {
        let root = temp_root("check-ok");
        let http = MockHttp::new();
        http.push(ok_release("v9.9.9"));
        let m = manager(&root, "1.0.0", Arc::clone(&http));

        let result = m.check(true);
        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["hasUpdate"], json!(true));
        assert_eq!(result["latest"], json!("9.9.9"));
        assert_eq!(result["comparison"], json!("newer"));
        assert_eq!(result["notes"], json!("Release notes body"));
        assert_eq!(result["checked"], json!(true));
        assert_eq!(result["checkedAt"], json!(NOW));
        assert_eq!(result["lastAttemptAt"], json!(NOW));
        assert_eq!(result["nextCheckAt"], json!(NOW + 20 * 60_000));
        assert_eq!(result["lastError"], Value::Null);
        assert_eq!(
            result["asset"]["name"],
            json!("AgentKeeper-9.9.9-setup.exe")
        );
        assert_eq!(result["downloadSupported"], json!(true));

        // The state file: exact keys, exact order, trailing newline. The first
        // key is `lastAttemptAt` and it carries the *injected* clock
        // (`writeState({ lastAttemptAt: startedAt })`, reference), not
        // wall time.
        let raw = std::fs::read_to_string(state_path(&root)).unwrap();
        assert!(raw.ends_with("}\n"));
        let first_key = raw.lines().nth(1).unwrap().trim().trim_end_matches(',');
        assert_eq!(first_key, format!("\"lastAttemptAt\": {}", NOW));
        let saved: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(saved["release"]["tag"], json!("9.9.9"));
        assert_eq!(saved["skippedVersion"], json!(""));

        // Second manual check inside 60 s reuses the answer without a request.
        let before = http.request_count();
        let throttled = m.check(true);
        assert_eq!(throttled["reason"], json!("throttled"));
        assert_eq!(throttled["reused"], json!(true));
        assert_eq!(throttled["ok"], json!(true));
        assert_eq!(
            http.request_count(),
            before,
            "throttled check must not fetch"
        );

        // A scheduled check inside the gap still runs (manual-only throttle).
        http.push(ok_release("v9.9.9"));
        let scheduled = m.check(false);
        assert_eq!(scheduled["ok"], json!(true));
        assert_eq!(http.request_count(), before + 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn check_404_is_a_successful_no_release_answer() {
        let root = temp_root("check-404");
        let http = MockHttp::new(); // empty queue → 404
        let m = manager(&root, "1.0.0", http);

        let result = m.check(true);
        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["latest"], Value::Null);
        assert_eq!(result["hasUpdate"], Value::Null);
        assert_eq!(result["comparison"], Value::Null);
        // 404 writes lastSuccessAt: it is a real answer, not a failure.
        assert_eq!(result["checked"], json!(true));
        assert_eq!(result["lastError"], Value::Null);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn check_403_sets_a_cooldown_from_the_reset_header() {
        let root = temp_root("check-403");
        let http = MockHttp::new();
        let reset = (NOW / 1000) + 120; // epoch seconds
        let reset_text = reset.to_string();
        http.push(Ok(fake_response(
            403,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", reset_text.as_str()),
            ],
            Value::Null,
        )));
        let m = manager(&root, "1.0.0", Arc::clone(&http));

        let result = m.check(true);
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["reason"], json!("error"));
        assert!(result["error"].as_str().unwrap().contains("rate limit"));
        let expected = reset * 1000 + 1000;
        assert_eq!(result["retryAt"], json!(expected));
        assert_eq!(result["lastError"], result["error"]);

        // The next check answers from the cooldown without a request.
        let count = http.request_count();
        let again = m.check(true);
        assert_eq!(again["reason"], json!("cooldown"));
        assert_eq!(again["ok"], json!(false));
        assert_eq!(again["retryAt"], json!(expected));
        assert_eq!(http.request_count(), count);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn retry_deadline_prefers_the_latest_of_headers_and_floor() {
        // Retry-After seconds.
        assert_eq!(
            retry_deadline(&[("retry-after".into(), "5".into())], 500, NOW),
            NOW + 5000
        );
        // 403 with no headers: the one-minute floor.
        assert_eq!(retry_deadline(&[], 403, NOW), NOW + 60_000);
        assert_eq!(retry_deadline(&[], 429, NOW), NOW + 60_000);
        // X-RateLimit-Reset only counts when remaining is 0.
        let reset = vec![
            ("X-RateLimit-Remaining".to_string(), "0".to_string()),
            ("X-RateLimit-Reset".to_string(), "1700000100".to_string()),
        ];
        assert_eq!(
            retry_deadline(&reset, 200, NOW),
            1_700_000_100 * 1000 + 1000
        );
        let not_zero = vec![
            ("X-RateLimit-Remaining".to_string(), "5".to_string()),
            ("X-RateLimit-Reset".to_string(), "1700000100".to_string()),
        ];
        assert_eq!(retry_deadline(&not_zero, 200, NOW), 0);
        // An HTTP date (IMF-fixdate).
        assert_eq!(
            retry_deadline(
                &[("Retry-After".into(), "Sun, 06 Nov 1994 08:49:37 GMT".into())],
                200,
                NOW
            ),
            784_111_777_000
        );
        // Unparseable value contributes nothing.
        assert_eq!(
            retry_deadline(&[("retry-after".into(), "soon".into())], 200, NOW),
            0
        );
    }

    #[test]
    fn transport_errors_keep_the_last_result_and_record_the_message() {
        let root = temp_root("check-err");
        let http = MockHttp::new();
        http.push(ok_release("v9.9.9"));
        http.push(Err(UpdateError::new(
            "cannot reach the server: connection timed out",
            502,
        )));
        let m = manager(&root, "1.0.0", http);

        let first = m.check(true);
        assert_eq!(first["latest"], json!("9.9.9"));

        let result = m.check(false);
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["reason"], json!("error"));
        assert_eq!(
            result["error"],
            json!("cannot reach the server: connection timed out")
        );
        assert_eq!(
            result["lastError"],
            json!("cannot reach the server: connection timed out")
        );
        // `retryAt` was 0 on the error object → no cooldown.
        assert_eq!(result["retryAt"], json!(0));
        // The previous release is kept ("保留上次结果").
        assert_eq!(result["latest"], json!("9.9.9"));
        assert_eq!(result["checked"], json!(true));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn beta_channel_queries_the_list_and_stays_on_prereleases() {
        let root = temp_root("beta");
        settings_store::save(
            &root,
            &json!({ "updateBeta": true, "updateRepo": "acme/keeper" }),
        );
        let http = MockHttp::new();
        http.push(Ok(fake_response(
            200,
            &[],
            json!([
                { "tag_name": "v9.9.9", "name": "final" },
                { "tag_name": "v9.9.9-beta.2", "name": "beta2" },
                { "tag_name": "v9.8.0", "name": "old" }
            ]),
        )));
        let m = manager(&root, "9.9.9-beta.1", Arc::clone(&http));

        let result = m.check(true);
        assert_eq!(result["latest"], json!("9.9.9-beta.2"));
        assert_eq!(result["hasUpdate"], json!(true));
        let url = http.requests.lock().unwrap()[0].0.clone();
        assert!(
            url.ends_with("/repos/acme/keeper/releases?per_page=30"),
            "{}",
            url
        );
        assert!(!url.contains("/latest"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn final_release_user_on_the_beta_channel_takes_the_highest() {
        let root = temp_root("beta-final");
        settings_store::save(&root, &json!({ "updateBeta": true }));
        let http = MockHttp::new();
        http.push(Ok(fake_response(
            200,
            &[],
            json!([
                { "tag_name": "v9.9.9-beta.2" },
                { "tag_name": "v9.9.9" },
                { "tag_name": "v9.8.0" }
            ]),
        )));
        let m = manager(&root, "9.8.0", http);

        assert_eq!(m.check(true)["latest"], json!("9.9.9"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skip_persists_in_the_state_file_and_unskip_clears_it() {
        let root = temp_root("skip");
        let http = MockHttp::new();
        http.push(ok_release("v9.9.9"));
        let m = manager(&root, "1.0.0", Arc::clone(&http));
        m.check(true);

        let skipped = m.skip(None);
        assert_eq!(skipped["skipped"], json!(true));
        assert_eq!(skipped["skippedVersion"], json!("9.9.9"));

        // Survives a restart: a second manager reads the same state file.
        let restarted = manager(&root, "1.0.0", Arc::clone(&http));
        assert_eq!(restarted.status()["skipped"], json!(true));
        assert_eq!(restarted.status()["latest"], json!("9.9.9"));

        assert_eq!(restarted.unskip()["skipped"], json!(false));
        // `m` is a separate manager with its own in-memory copy of the state
        // （每个 manager 在构造时只读一次 state），所以清空后的值
        // value is only observable on the instance that performed `unskip`.
        assert_eq!(restarted.status()["skippedVersion"], json!(""));

        // Skipping an explicit version (the IPC path passes `payload.version`).
        let skipped = m.skip(Some("9.8.0"));
        assert_eq!(skipped["skippedVersion"], json!("9.8.0"));
        assert_eq!(
            skipped["skipped"],
            json!(false),
            "only the cached tag matches"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn interval_and_repo_validation_rules() {
        assert_eq!(clamp_minutes(&json!(0)), 1);
        assert_eq!(clamp_minutes(&json!(1)), 1);
        assert_eq!(clamp_minutes(&json!(20.6)), 21);
        assert_eq!(clamp_minutes(&json!(1441)), 1440);
        assert_eq!(clamp_minutes(&json!("abc")), 20);
        // `Number(null) === 0`, so
        // `clampMinutes(null)` is 1, not the default. Only non-finite input
        // (NaN/Infinity, i.e. `undefined` for a missing settings key) yields
        // DEFAULT_MINUTES — and
        // `Store.defaultSettings()` always writes `updateCheckMinutes: 20`, so
        // the manager never sees that case.
        assert_eq!(clamp_minutes(&Value::Null), 1);

        assert_eq!(normalize_repo(" acme/keeper "), "acme/keeper");
        assert_eq!(normalize_repo("acme/keeper.git"), "acme/keeper.git");
        assert_eq!(normalize_repo("acme"), "");
        assert_eq!(normalize_repo("a/b/c"), "");
        assert_eq!(normalize_repo("ac me/keeper"), "");
        assert_eq!(normalize_repo("/keeper"), "");
    }

    /// The `ak:update-settings` guard.
    ///
    /// Without it `update_save_settings` clamps an unusable value into a
    /// usable-looking one and persists it, so the user learns about a bad
    /// repo/proxy only when the next check fails.
    #[test]
    fn the_settings_guard_rejects_every_value_outside_the_range() {
        // minutes: `Math.round(Number(v))` first, then 1..=1440.
        assert_eq!(validate_settings_patch(&json!({ "minutes": 20 })), None);
        assert_eq!(
            validate_settings_patch(&json!({ "minutes": 20.6 })),
            None,
            "rounds to 21"
        );
        assert_eq!(
            validate_settings_patch(&json!({ "minutes": 0.6 })),
            None,
            "rounds to 1 — a raw `0.6 < 1` test would wrongly reject this"
        );
        assert_eq!(validate_settings_patch(&json!({ "minutes": 1440 })), None);
        assert_eq!(validate_settings_patch(&json!({ "minutes": "30" })), None);
        assert_eq!(
            validate_settings_patch(&json!({ "minutes": 0 })),
            Some(ERR_INTERVAL_RANGE)
        );
        assert_eq!(
            validate_settings_patch(&json!({ "minutes": 1441 })),
            Some(ERR_INTERVAL_RANGE)
        );
        assert_eq!(
            validate_settings_patch(&json!({ "minutes": Value::Null })),
            Some(ERR_INTERVAL_RANGE),
            "Number(null) === 0"
        );
        assert_eq!(
            validate_settings_patch(&json!({ "minutes": "abc" })),
            Some(ERR_INTERVAL_RANGE),
            "Number('abc') is NaN"
        );

        // repo: trimmed; empty falls back to the built-in repository, so it is
        // legal. `asString()` of a non-string is `''` — also legal.
        assert_eq!(
            validate_settings_patch(&json!({ "repo": " acme/keeper " })),
            None
        );
        assert_eq!(validate_settings_patch(&json!({ "repo": "" })), None);
        assert_eq!(validate_settings_patch(&json!({ "repo": 5 })), None);
        assert_eq!(
            validate_settings_patch(&json!({ "repo": "acme" })),
            Some(ERR_REPO_FORMAT)
        );
        assert_eq!(
            validate_settings_patch(&json!({ "repo": "a/b/c" })),
            Some(ERR_REPO_FORMAT)
        );

        // apiBase: `^https?://` when non-empty.
        assert_eq!(
            validate_settings_patch(&json!({ "apiBase": "https://mirror.test" })),
            None
        );
        assert_eq!(
            validate_settings_patch(&json!({ "apiBase": "http://mirror.test" })),
            None
        );
        assert_eq!(validate_settings_patch(&json!({ "apiBase": "" })), None);
        assert_eq!(
            validate_settings_patch(&json!({ "apiBase": "mirror.test" })),
            Some(ERR_API_BASE_FORMAT)
        );
        assert_eq!(
            validate_settings_patch(&json!({ "apiBase": "ftp://mirror.test" })),
            Some(ERR_API_BASE_FORMAT)
        );

        // proxy: auto | direct | `^https?://\S+$`; empty degrades to auto.
        assert_eq!(validate_settings_patch(&json!({ "proxy": "auto" })), None);
        assert_eq!(validate_settings_patch(&json!({ "proxy": "direct" })), None);
        assert_eq!(validate_settings_patch(&json!({ "proxy": "" })), None);
        assert_eq!(
            validate_settings_patch(&json!({ "proxy": "http://127.0.0.1:7897" })),
            None
        );
        assert_eq!(
            validate_settings_patch(&json!({ "proxy": "https://user:pw@proxy.test:8080" })),
            None
        );
        assert_eq!(
            validate_settings_patch(&json!({ "proxy": "socks://127.0.0.1:1080" })),
            Some(ERR_PROXY_FORMAT)
        );
        assert_eq!(
            validate_settings_patch(&json!({ "proxy": "127.0.0.1:7897" })),
            Some(ERR_PROXY_FORMAT)
        );
        assert_eq!(
            validate_settings_patch(&json!({ "proxy": "http://" })),
            Some(ERR_PROXY_FORMAT),
            "the regex needs a host after the scheme"
        );

        // A key the patch does not carry is never inspected, so a partial save
        // (just `auto`, say) is not rejected for a field it did not send.
        assert_eq!(validate_settings_patch(&json!({ "auto": false })), None);
        assert_eq!(validate_settings_patch(&json!({})), None);
    }

    /// `ak:update-token` 的载荷校验 — 渲染层的"清除令牌"按钮发送 `null`，而
    /// `String` 参数连反序列化都无法接受它。
    ///
    /// The full contract (including the missing-field branch) is asserted in
    /// `tests/update_pipeline.rs::token_arg_turns_null_into_clear_and_rejects_other_types`;
    /// this one is the fast unit-level tripwire next to the code.
    #[test]
    fn the_token_argument_guard_turns_null_into_clear() {
        // `null` clears — that is the whole point of BUG-1.
        assert_eq!(token_arg(&json!({ "token": null })), Ok(None));
        assert_eq!(
            token_arg(&json!({ "token": "ghp_example" })),
            Ok(Some("ghp_example".to_string()))
        );
        assert_eq!(token_arg(&json!({ "token": 5 })), Err(ERR_TOKEN_TYPE));
        assert_eq!(token_arg(&json!({ "token": true })), Err(ERR_TOKEN_TYPE));
        // A body without the field is an error, never a silent clear.
        assert_eq!(token_arg(&json!({})), Err(ERR_TOKEN_FIELD_REQUIRED));
    }

    #[test]
    fn download_success_reports_progress_and_persists_the_path() {
        let root = temp_root("dl-ok");
        let http = MockHttp::new();
        http.push(ok_release("v9.9.9"));
        let mut m = manager(&root, "1.0.0", http);
        m.check(true);

        m.downloader = Arc::new(|_url, dest, _proxy, _max, _timeout, on_progress| {
            on_progress(0, Some(4));
            std::fs::write(dest, b"abcd").unwrap();
            on_progress(4, Some(4));
            Ok(4)
        });

        let url =
            "https://github.com/acme/keeper/releases/download/v9.9.9/AgentKeeper-9.9.9-setup.exe";
        let snapshot = m
            .start_download(url, Some("AgentKeeper-9.9.9-setup.exe"))
            .unwrap();
        assert_eq!(snapshot["active"], json!(true));
        assert_eq!(snapshot["done"], json!(false));
        assert_eq!(snapshot["filename"], json!("AgentKeeper-9.9.9-setup.exe"));
        assert_eq!(snapshot["percent"], json!(0));

        let task = wait_inactive(&m);
        assert_eq!(task["done"], json!(true));
        assert_eq!(task["percent"], json!(100));
        assert_eq!(task["received"], json!(4));
        assert_eq!(task["total"], json!(4));
        let path = task["path"].as_str().unwrap().to_string();
        assert!(Path::new(&path).is_file());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4);
        assert!(Path::new(&path).starts_with(&m.update_dir));
        assert_eq!(m.status()["task"], task);

        let state = m.state_snapshot();
        assert_eq!(state["downloadedPath"], json!(path));
        assert_eq!(state["downloadedVersion"], json!("9.9.9"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn download_rejects_a_url_outside_the_allow_list() {
        let root = temp_root("dl-bad-url");
        let m = manager(&root, "1.0.0", MockHttp::new());
        let error = m
            .start_download("http://evil.test/x.exe", None)
            .unwrap_err();
        assert_eq!(error.status(), 400);
        assert_eq!(error.code(), "https-required");
        assert_eq!(m.progress(), Value::Null, "no task is created");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn download_names_come_from_the_url_when_the_renderer_omits_one() {
        let root = temp_root("dl-name");
        let mut m = manager(&root, "1.0.0", MockHttp::new());
        m.downloader = Arc::new(|_url, dest, _proxy, _max, _timeout, on_progress| {
            std::fs::write(dest, b"x").unwrap();
            on_progress(1, Some(1));
            Ok(1)
        });

        let snapshot = m
            .start_download(
                "https://github.com/acme/keeper/releases/download/v1/AgentKeeper%201.0.exe",
                Some(""),
            )
            .unwrap();
        assert_eq!(snapshot["filename"], json!("AgentKeeper 1.0.exe"));
        wait_inactive(&m);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn failed_download_removes_the_partial_file() {
        let root = temp_root("dl-fail");
        let mut m = manager(&root, "1.0.0", MockHttp::new());
        m.downloader = Arc::new(|_url, dest, _proxy, _max, _timeout, on_progress| {
            on_progress(0, Some(4));
            std::fs::write(dest, b"abcd").unwrap();
            on_progress(4, Some(4));
            Err(FetchError::Http(500))
        });

        let snapshot = m
            .start_download(
                "https://github.com/acme/keeper/releases/download/v1/AgentKeeper-1.0.0.exe",
                None,
            )
            .unwrap();
        let path = snapshot["path"].as_str().unwrap().to_string();
        let task = wait_inactive(&m);

        assert_eq!(task["done"], json!(false));
        assert_eq!(task["canceled"], json!(false));
        assert_eq!(task["path"], Value::Null);
        assert!(task["error"]
            .as_str()
            .unwrap()
            .contains("download failed: the server returned HTTP 500"));
        assert!(
            !Path::new(&path).exists(),
            "a failed download deletes its file"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn canceled_download_reports_canceled_and_keeps_the_partial_file() {
        let root = temp_root("dl-cancel");
        let mut m = manager(&root, "1.0.0", MockHttp::new());
        let started = Arc::new((Mutex::new(false), Condvar::new()));

        let flag = Arc::clone(&started);
        m.downloader = Arc::new(move |_url, dest, _proxy, _max, _timeout, on_progress| {
            on_progress(0, Some(10));
            std::fs::write(dest, b"ab").unwrap();
            {
                let (lock, cv) = &*flag;
                *lock.lock().unwrap() = true;
                cv.notify_all();
            }
            let mut received = 2;
            for _ in 0..5000 {
                received += 1;
                if !on_progress(received, Some(10)) {
                    // The manager classifies this purely by its own flag.
                    return Err(FetchError::Transport("aborted".to_string()));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(received)
        });

        let snapshot = m
            .start_download(
                "https://github.com/acme/keeper/releases/download/v1/AgentKeeper-1.0.0.exe",
                None,
            )
            .unwrap();
        let path = snapshot["path"].as_str().unwrap().to_string();

        {
            let (lock, cv) = &*started;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = cv.wait(ready).unwrap();
            }
        }
        assert_eq!(m.cancel(), json!({ "canceled": true }));

        let task = wait_inactive(&m);
        assert_eq!(task["canceled"], json!(true));
        assert_eq!(task["done"], json!(false));
        assert_eq!(task["path"], Value::Null);
        assert_eq!(task["error"], Value::Null, "cancel is not an error");
        assert!(
            task["received"].as_u64().unwrap() > 0,
            "progress survives cancel"
        );
        // 解析口径: the cancel path returns before the `catch` that unlinks.
        assert!(Path::new(&path).exists(), "partial file survives a cancel");

        // Nothing to cancel any more.
        assert_eq!(m.cancel(), json!({ "canceled": false }));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn oversized_declared_length_is_refused_before_writing() {
        let root = temp_root("dl-big");
        let mut options = Options::default();
        options.platform = "win32".into();
        options.arch = "x64".into();
        options.app_version = "1.0.0".into();
        options.max_bytes = 1024;
        let mut m = UpdateManager::with_options(&root, options);
        m.downloader = Arc::new(|_url, dest, _proxy, _max, _timeout, on_progress| {
            // 50 MB declared, but no byte is written before the callback says no.
            if !on_progress(0, Some(50 * 1024 * 1024)) {
                return Err(FetchError::Transport("aborted".to_string()));
            }
            std::fs::write(dest, b"never").unwrap();
            Ok(4)
        });

        let snapshot = m
            .start_download(
                "https://github.com/acme/keeper/releases/download/v1/AgentKeeper-big.exe",
                Some("AgentKeeper-big.exe"),
            )
            .unwrap();
        let path = snapshot["path"].as_str().unwrap().to_string();
        let task = wait_inactive(&m);

        assert_eq!(task["path"], Value::Null);
        assert!(
            task["error"].as_str().unwrap().contains("size looks wrong"),
            "{:?}",
            task["error"]
        );
        assert!(task["error"].as_str().unwrap().contains("50 MB"));
        assert!(!Path::new(&path).exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_guards_and_launch_shape() {
        let root = temp_root("install");
        std::fs::create_dir_all(Path::new(&root).join(UPDATES_DIR)).unwrap();
        let update_dir = Path::new(&root).join(UPDATES_DIR);

        let launcher_calls = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::clone(&launcher_calls);
        let mut m = manager(&root, "1.0.0", MockHttp::new());
        m.launcher = Arc::new(move |file, silent| {
            calls.lock().unwrap().push((path_string(file), silent));
            Ok("fake".to_string())
        });

        assert_eq!(m.verify_installer("").unwrap_err().status(), 400);
        assert!(m.verify_installer("AgentKeeper.exe").is_err());
        assert!(m
            .verify_installer(&update_dir.join("AgentKeeper.txt").to_string_lossy())
            .is_err());

        let outside = Path::new(&root).join("AgentKeeper-outside.exe");
        std::fs::write(&outside, "x").unwrap();
        let error = m.verify_installer(&outside.to_string_lossy()).unwrap_err();
        assert!(
            error.message().contains("managed download directory"),
            "{}",
            error
        );

        let inside = update_dir.join("AgentKeeper-1.0.0.exe");
        std::fs::write(&inside, "x").unwrap();
        let real = m.verify_installer(&inside.to_string_lossy()).unwrap();
        assert_eq!(
            real,
            std::fs::canonicalize(&inside)
                .map(strip_verbatim_prefix)
                .unwrap()
        );

        let foreign = update_dir.join("OtherApp-1.0.0.exe");
        std::fs::write(&foreign, "x").unwrap();
        let error = m.verify_installer(&foreign.to_string_lossy()).unwrap_err();
        assert!(error.message().contains("another project"), "{}", error);

        let result = m.install(&inside.to_string_lossy()).unwrap();
        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["restarted"], json!(true));
        assert_eq!(result["mode"], json!("fake"));
        assert_eq!(result["path"], json!(path_string(&real)));
        let calls = launcher_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, path_string(&real));
        assert!(calls[0].1, "silent install is on by default");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_refuses_while_a_download_is_active() {
        let root = temp_root("install-active");
        std::fs::create_dir_all(Path::new(&root).join(UPDATES_DIR)).unwrap();
        let inside = Path::new(&root)
            .join(UPDATES_DIR)
            .join("AgentKeeper-1.0.0.exe");
        std::fs::write(&inside, "x").unwrap();

        let mut m = manager(&root, "1.0.0", MockHttp::new());
        let started = Arc::new((Mutex::new(false), Condvar::new()));
        let flag = Arc::clone(&started);
        m.downloader = Arc::new(move |_url, dest, _proxy, _max, _timeout, on_progress| {
            std::fs::write(dest, b"ab").unwrap();
            on_progress(2, Some(100));
            {
                let (lock, cv) = &*flag;
                *lock.lock().unwrap() = true;
                cv.notify_all();
            }
            for _ in 0..5000 {
                if !on_progress(2, Some(100)) {
                    return Err(FetchError::Transport("aborted".to_string()));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(2)
        });
        m.start_download(
            "https://github.com/acme/keeper/releases/download/v1/AgentKeeper-2.0.0.exe",
            Some("AgentKeeper-2.0.0.exe"),
        )
        .unwrap();
        {
            let (lock, cv) = &*started;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = cv.wait(ready).unwrap();
            }
        }

        let error = m.install(&inside.to_string_lossy()).unwrap_err();
        assert!(error.message().contains("still downloading"), "{}", error);
        assert_eq!(error.status(), 400);

        m.cancel();
        wait_inactive(&m);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn save_settings_clamps_persists_and_reschedules() {
        let root = temp_root("save-settings");
        let mut m = manager(&root, "1.0.0", MockHttp::new());

        let status = m.save_settings(&json!({ "auto": false })).unwrap();
        assert_eq!(status["settings"]["auto"], json!(false));
        assert_eq!(m.due_at(), 0);
        assert_eq!(m.tick(), None);

        let status = m
            .save_settings(&json!({
                "auto": true,
                "minutes": 0,
                "beta": true,
                "repo": " acme/keeper ",
                "apiBase": " https://mirror.test/ ",
                "proxy": "",
                "silent": false
            }))
            .unwrap();
        assert_eq!(status["settings"]["auto"], json!(true));
        assert_eq!(
            status["settings"]["minutes"],
            json!(1),
            "0 minutes clamps to 1"
        );
        assert_eq!(status["settings"]["beta"], json!(true));
        assert_eq!(status["settings"]["proxy"], json!("auto"));
        assert_eq!(status["settings"]["silent"], json!(false));
        assert_eq!(status["repo"], json!("acme/keeper"));
        assert_eq!(status["apiBase"], json!("https://mirror.test"));

        // The store saw the manager's key names, not the patch names.
        let stored = settings_store::read(&root);
        assert_eq!(stored["updateCheck"], json!(true));
        assert_eq!(stored["updateCheckMinutes"], json!(1));
        assert_eq!(stored["updateInstallSilent"], json!(false));

        // A minute change with a previous attempt recomputes the deadline.
        m.now = Arc::new(|| NOW);
        m.mutate_state(|state| state.last_attempt_at = NOW - 1000);
        m.save_settings(&json!({ "minutes": 30 })).unwrap();
        assert_eq!(m.status()["nextCheckAt"], json!(NOW - 1000 + 30 * 60_000));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn changing_the_proxy_clears_a_cooldown_from_another_bucket() {
        let root = temp_root("proxy-clear");
        let http = MockHttp::new();
        http.push(Ok(fake_response(403, &[], Value::Null)));
        let m = manager(&root, "1.0.0", http);
        assert_eq!(m.check(true)["ok"], json!(false));
        assert!(m.status()["retryAt"].as_i64().unwrap() > 0);
        assert!(m.status()["lastError"].is_string());

        let status = m.save_settings(&json!({ "proxy": "direct" })).unwrap();
        assert_eq!(status["retryAt"], json!(0));
        assert_eq!(status["lastError"], Value::Null);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn status_reports_checking_while_a_check_is_in_flight() {
        let root = temp_root("checking");
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Arc::new(Mutex::new(rx));

        let mut m = manager(&root, "1.0.0", MockHttp::new());
        m.fetch = Arc::new(move |_url, _token| {
            let _ = rx.lock().unwrap().recv();
            Ok(fake_response(200, &[], release_payload("v9.9.9")))
        });
        m.now = Arc::new(|| NOW);
        let m = Arc::new(m);

        let worker = Arc::clone(&m);
        let handle = std::thread::spawn(move || worker.check(true));

        let mut saw_checking = false;
        for _ in 0..4000 {
            let status = m.status();
            if status["checking"] == json!(true) {
                // `lastAttemptAt` is written before the request is sent.
                assert_eq!(status["lastAttemptAt"], json!(NOW));
                assert_eq!(status["checked"], json!(false));
                saw_checking = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(saw_checking, "status never reported checking=true");

        tx.send(()).unwrap();
        let result = handle.join().unwrap();
        // The returned snapshot is built *inside* the check, before `finally`
        // clears the in-flight flag (reads `status()` while
        // `checking` is still the promise), so the result carries
        // `checking: true`. Only a later `status()` sees it cleared.
        assert_eq!(result["checking"], json!(true));
        assert_eq!(m.status()["checking"], json!(false));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn concurrent_checks_share_one_request() {
        let root = temp_root("coalesce");
        let http = MockHttp::new();
        http.push(ok_release("v9.9.9"));
        // Any second response would be consumed only by a second request.
        http.push(ok_release("v9.9.8"));
        let m = Arc::new(manager(&root, "1.0.0", Arc::clone(&http)));

        let a = Arc::clone(&m);
        let one = std::thread::spawn(move || a.check(true));
        // Give the first request time to claim the slot.
        for _ in 0..1000 {
            if m.status()["checking"] == json!(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let b = Arc::clone(&m);
        let two = std::thread::spawn(move || b.check(false));

        let first = one.join().unwrap();
        let second = two.join().unwrap();
        assert_eq!(http.request_count(), 1, "one request serves both callers");
        assert_eq!(first["latest"], json!("9.9.9"));
        assert_eq!(second["latest"], json!("9.9.9"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scheduler_runs_immediately_then_waits_for_the_deadline() {
        let root = temp_root("sched");
        let http = MockHttp::new();
        http.push(ok_release("v9.9.9"));
        let m = manager(&root, "1.0.0", Arc::clone(&http));

        // No history → due now, not one interval away.
        assert_eq!(m.due_at(), 0);
        let first = m.tick().expect("first tick checks");
        assert_eq!(first["latest"], json!("9.9.9"));
        assert!(m.due_at() == NOW + 20 * 60_000);

        let count = http.request_count();
        assert!(
            m.tick().is_none(),
            "a tick before the deadline does nothing"
        );
        assert_eq!(http.request_count(), count);

        m.save_settings(&json!({ "auto": false })).unwrap();
        assert_eq!(m.due_at(), 0);
        assert!(m.tick().is_none());

        std::fs::remove_dir_all(&root).ok();
    }
}
