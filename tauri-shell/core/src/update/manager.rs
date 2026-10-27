//! 更新状态机。
//!
//! 检查 → 下载 → 安装，计划与上次的结果持久化在
//! `<store>/update-state.json` 下。每个响应的形状都保持不变，因为
//! IPC 层会把这些值原样交给渲染层，
//! 而 `src/renderer/app.js` 会从中读取具名字段：
//!
//! | 方法 | 响应 |
//! |---|---|
//! | [`UpdateManager::status`] | 持久化的状态快照 |
//! | [`UpdateManager::check`] | 该状态；失败时为 `ok:false` + `reason` |
//! | [`UpdateManager::save_settings`] | 状态 |
//! | [`UpdateManager::set_token`] | 状态 + `saved` + `changed` |
//! | [`UpdateManager::start_download`] | 任务快照 |
//! | [`UpdateManager::progress`] | 任务或 `null` |
//! | [`UpdateManager::cancel`] | `{ canceled: bool }`（取消是否真的生效） |
//! | [`UpdateManager::install`] | `{ ok, restarted, path, mode }`（安装结果与重启方式） |
//! | [`UpdateManager::skip`] / [`UpdateManager::unskip`] | 状态 |
//!
//! IPC 层把下载/安装失败包装为 `{ ok: false, error }`；这里的方法
//! 返回 `Result`，`lib.rs` 的命令也以同样方式把 [`UpdateError`]
//! 转换过去。
//!
//! ## 保真度
//!
//! 行为、字段名、默认值以及状态*何时*写入都沿用
//! 之前的构建。哪里有 quirk，就复现该 quirk 并
//! 标注 `// 解析口径:`。
//!
//! * `check` 在网络调用*之前*写入 `lastAttemptAt`，
//!   在其*之后*写入 `lastSuccessAt`/`nextCheckAt`/`release`。404 被算作
//!   一次成功的检查（仓库只是还没有发布而已）。
//! * 60 秒的手动节流需要此前有一次*成功*，并且是拿
//!   `lastAttemptAt` 来比较，所以失败之后的两次手动检查都会
//!   真的走网络。
//! * 命中冷却期或节流时，绝不触碰状态文件。
//! * `state` 在 `update-state.json` 中是一个扁平对象；被跳过的版本也放在
//!   同一个文件里（`skippedVersion`），而不是设置里。文件中已存在的未知键
//!   在下次写入时会被保留，与参考实现的展开写法完全一致。
//! * **已取消**的下载会在删除半成品的 `catch` 之前就从 `try`
//!   块返回，因此半成品安装器留在磁盘上
//!   （任务的 `path` 被置空，下次下载同名文件时会 unlink 它）。
//!   只有失败的下载才删除。见 `run_download`。
//! * `install` 是先校验路径，*然后*才检查是否有下载
//!   正在进行，因此即使在下载中途，无效路径也会报路径错误。
//! * `status()` 除了读取设置/状态并解密
//!   令牌状态之外是纯函数；每次调用的每个字段都存在，包括第一次
//!   检查之前。
//!
//! 刻意保留并有文档说明的差异（每一项要么是"Rust 做不到"，要么是
//! 仅内部可见的细节）：
//!
//! * `appVersion` 来自 `env!("CARGO_PKG_VERSION")`。`Cargo.toml` 和
//!   `package.json` 都写的是 `0.2.0`；两者必须一起升版，否则
//!   应用会比对到一个与发布不同的"当前版本"。
//! * 内置仓库就是字面量 `ntc0728/agentkeeper` —— 也就是
//!   `package.json` 的 `repository.url` 产出的值。没有运行时
//!   `require('package.json')` 可以回退。
//! * `Retry-After` 只按 IMF-fixdate 解析（见 [`parse_http_date`]）；
//!   GitHub 发的是 `X-RateLimit-Reset`，所以这是一个没人会走到的回退路径。
//! * 下载跑在自己的线程上，因为 `core::http` 是阻塞的，而
//!   IPC handler 必须立刻返回任务快照（渲染层
//!   轮询 `ak:update-progress`）。
//!
//! ## 假定的接口（归其他模块所有，此处未实现）
//!
//! `core/update/http.rs` —— 在此实现：拉取发布信息的
//! 签名 `fetch_release(url, token, timeout_ms) -> Result<ReleaseFetch, UpdateError>`，
//! 其中 `ReleaseFetch { status, headers, json }`。对*任何* HTTP 状态都返回
//! `Ok`（含 404/403）；只有传输或 body 解析失败才返回 `Err`。它
//! **不接受任何代理设置**（见 `core::http` 中的限制）：因此
//! `updateProxy` 会影响下载，但不影响发布检查。
//!
//! `core/http.rs` —— 在此实现：下载用的
//! 签名 `download_to_file(url, dest, proxy, max_bytes, timeout_ms, on_progress)`，
//! 其中 `on_progress: &mut dyn FnMut(u64, Option<u64>) -> bool`；返回 `false` 会中止
//! 并把半成品文件留在原地（参考实现的取消路径）。
//!
//! `core/update/token.rs` —— 在此实现：令牌存储的 `TokenStore::new(root)`、
//! 查询状态的 `status() -> Value`、判断加密是否可用的
//! `encryption_available() -> bool`、取当前生效令牌的 `effective() ->
//! Option<String>`，以及写入令牌的 `set(Option<&str>) -> Result<TokenSetResult, UpdateError>`。

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
/* 常量                                                                */
/* ------------------------------------------------------------------ */

/// 两次检查间隔小于此值就复用上次的答案（手动检查也算）。
pub const MIN_CHECK_GAP_MS: i64 = 60 * 1000;

/// GitHub 返回 403/429 且没有可用响应头时的重试下限。
pub const RATE_LIMIT_FLOOR_MS: i64 = 60 * 1000;

pub const DEFAULT_MINUTES: i64 = 20;
pub const MIN_MINUTES: i64 = 1;
pub const MAX_MINUTES: i64 = 1440;

/// `USER_AGENT` —— 该请求头属于 `update/http.rs`；放在这里是为了
/// 那些把该值钉死的测试。
pub const USER_AGENT: &str = "AgentKeeper-updater";

const STATE_FILE: &str = "update-state.json";
const UPDATES_DIR: &str = "updates";

/// `package.json` 的 `repository.url` 解析出的值。见模块
/// 文档：文件缺失时参考实现恰好回退到这个字符串。
pub const BUILT_IN_REPO: &str = "ntc0728/agentkeeper";

/// 一次 GitHub API 调用的明确截止时间（连接、TLS、请求、body）。
///
/// `ureq` 有一个*默认*读超时，所以这里不写具体值也仍能
/// "工作" —— 然后随依赖升级而漂移。把它写出来同时也是为了记录
/// 意图：对于小 JSON body 来说总截止时间是对的（卡住的对端必须让检查
/// 失败，而不是把调度线程挂住），而对安装器
/// 下载流则是错的，它改为按阶段使用 [`DOWNLOAD_TIMEOUT_MS`]。
pub const NETWORK_TIMEOUT_MS: u64 = 30_000;

/// 安装器下载的按阶段截止时间。有意大于 API 调用的
/// 截止时间：600 MB 的传输允许慢，但不允许毫无动静。
/// `http::download_to_file` 把它作用于每次读/写/连接，绝不
/// 作用于整段传输（那会中止一次合理的慢速下载）。
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

/// `String(v || '')`：假值一律变空串，其余按 JS 字符串化。
fn js_string_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(v) if js_truthy(v) => js_to_string(v),
        _ => String::new(),
    }
}

/// 设置文件可能包含的标量形态对应的 `Number(v)`。足够支撑
/// `Math.round(Number(...))`；实践中 JSON 设置里的值都是数字。
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
/* 仓库与间隔归一化                                                       */
/* ------------------------------------------------------------------ */

/// `owner/repo`，GitHub 唯一接受作路径段的形状。
///
/// 参考实现用 `/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/`；手动检查保持
/// 无依赖，并拒绝 `a/b/c`、空的两半和空白。
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

/// `clampMinutes` —— `Math.round(Number(v))`，输入非有限时取默认值，
/// 并夹到 1..=1440。
pub fn clamp_minutes(value: &Value) -> i64 {
    let n = js_number(Some(value));
    if !n.is_finite() {
        return DEFAULT_MINUTES;
    }
    // `Math.round` 向 +Infinity 方向取半数；`f64::round` 则远离
    // 零。唯一差别是 -0.5 -> -0 还是 -1，而两者都会被夹到 1。
    let rounded = n.round();
    (rounded as i64).clamp(MIN_MINUTES, MAX_MINUTES)
}

/* ------------------------------------------------------------------ */
/* `ak:update-*` 载荷守卫                                              */
/* ------------------------------------------------------------------ */
//
// 参考实现是在 *handler* 里校验原始 IPC 载荷，在
// `manager.saveSettings` 真正运行之前 —— `saveSettings` 本身只做夹取和
// 去空白。这两个函数就是那道守卫，放在这里是为了让
// 规则成为一个可测试的整体，而 `lib.rs` 保持为一个薄适配层。没有它们，
// 一个畸形的 repo / apiBase / proxy / interval 会被静默持久化，
// 之后才以"更新检查失败"的形式冒出来，而不是在点击时就报错。

// Rust 引擎讲英文；`toast()` 会对它收到的每个 `{ok:false,error}` 运行
// `zhError()` → `zhText()`，所以中文来自
// `src/renderer/engine-zh.js`（EXACT），绝不是来自这里。这也是
// `ipc.repoFormat`、`ipc.apiBaseFormat`、`ipc.proxyFormat` 这三个键，
// 加上 `ipc.intervalRange`、`update.tokenFieldRequired`、`update.tokenType`
// 之所以保留英文的原因。
//
// `ERR_REPO_FORMAT` 刻意与 `discover.repoFormat` 使用的
// 是*同一句话*：一条规则，一个字符串，一个字典条目。

/// `ipc.intervalRange` —— `检查间隔必须是 1 ~ 1440 分钟`。
pub const ERR_INTERVAL_RANGE: &str = "check interval must be between 1 and 1440 minutes";
/// `ipc.repoFormat` —— `仓库必须是 owner/repo 形式`。
pub const ERR_REPO_FORMAT: &str = "repo must be owner/repo or a github.com URL";
/// `ipc.apiBaseFormat` —— `更新源必须是 http(s) 地址`。
pub const ERR_API_BASE_FORMAT: &str = "update source must be an http(s) URL";
/// `ipc.proxyFormat` —— `代理必须是 auto、direct 或 http(s) 地址`。
pub const ERR_PROXY_FORMAT: &str = "proxy must be auto, direct or an http(s) URL";
/// `update.tokenType` —— `token 必须是字符串或 null`。
pub const ERR_TOKEN_TYPE: &str = "token must be a string or null";
/// `update.tokenFieldRequired` —— `请求缺少 token 字段（清除请传 null）`。
pub const ERR_TOKEN_FIELD_REQUIRED: &str =
    "request is missing the token field (pass null to clear)";

/// `asString(v, max)`（参考实现）：非字符串一律是 `""`，
/// 字符串按 `max` 个字符切片。
fn js_as_string(value: &Value, max: usize) -> String {
    match value {
        Value::String(text) => text.chars().take(max).collect(),
        _ => String::new(),
    }
}

/// `ak:update-settings` 的守卫。
///
/// 返回 IPC 层答复用的错误消息，补丁可接受时返回 `None`。补丁没带的键
/// 不做检查，因此部分保存（比如只带 `auto`）绝不会因为
/// 一个它根本没发来的字段而被拒绝。
pub fn validate_settings_patch(patch: &Value) -> Option<&'static str> {
    if let Some(value) = patch.get("minutes") {
        // 先 `Math.round(Number(v))`：20.6 → 21 和 0.6 → 1 都通过，
        // NaN/Infinity 以及 1..=1440 之外的值不通过。
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
        // `/^https?:\/\/\S+$/` —— 固定的路由必须是一个不含
        // 空白字符的 URL。
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

/// `ak:update-token` 的守卫。
///
/// `payload` 是**整个 IPC body**，而不是 `token` 字段，这正是
/// 缺失字段分支能工作的原因。参考实现检查 `'token' in p`；
/// 显而易见的 Rust 翻译 —— `token: Option<Value>` —— 做不到，因为
/// Tauri 的 `CommandItem::deserialize_option`（`tauri-2.12.4/src/ipc/command.rs:146`）
/// 把*缺失*的键变成 `visit_none()`，把 `Value::Null` 也变成
/// `visit_none()`，于是 `{}` 和 `{token:null}` 到达时完全一样，
/// 已经没有东西可以用来区分了。保留请求体就能让这两种
/// 情况一路区分下去，于是：
///
/// * 没有 `token` 键（或 body 不是对象，`asObject` 会把它折叠成 `{}`）
///   → 报 `ERR_TOKEN_FIELD_REQUIRED`；
/// * `null` → `Ok(None)`：清除；
/// * 字符串 → `Ok(Some(..))`：替换；
/// * 数字 / 布尔 / 数组 / 对象 → `ERR_TOKEN_TYPE`。
///
/// 清除必须是显式的。忘记带字段的调用方是调用方的 bug，
/// 而悄悄删掉凭据会是处理它最糟糕的方式。
pub fn token_arg(payload: &Value) -> Result<Option<String>, &'static str> {
    match payload.get("token") {
        None => Err(ERR_TOKEN_FIELD_REQUIRED),
        Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ERR_TOKEN_TYPE),
    }
}

/* ------------------------------------------------------------------ */
/* 限流截止时间                                                          */
/* ------------------------------------------------------------------ */

/// 依据 GitHub 的限流响应头算出的"最早可以再次请求的时刻"。
///
/// `Retry-After`（秒数或 HTTP 日期）、`X-RateLimit-Reset`（epoch 秒，
/// 只在剩余额度为 0 时有意义），以及 403/429 时的一分钟下限。
/// 响应头名按大小写不敏感匹配；取第一个值（也就是
/// 取数组的第一个元素）。
pub fn retry_deadline(headers: &[(String, String)], status: u16, now: i64) -> i64 {
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    let mut deadline: i64 = 0;
    // `if (retryAfter)` —— 空字符串或 "0"……等等："0" 是真值，0 不是。
    // 来自响应头映射的字符串除非为空否则总是真值，而
    // `Number('0')` 是一个完全正当的零。
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

/// IMF-fixdate（`Sun, 06 Nov 1994 08:49:37 GMT`）→ epoch 毫秒。
///
/// `Date.parse` 接受更多写法；这是 RFC 7231
/// 指定为规范格式的那种，也是 GitHub 若使用该响应头会发送的格式。
/// 在 `Date.parse` 会返回 `NaN` 的地方返回 `None`。
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

/// Howard Hinnant 的 `days_from_civil` —— 距 1970-01-01 的天数。
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
/* 平台映射                                                             */
/* ------------------------------------------------------------------ */

/// 按 Node 方式拼写的 `process.platform` —— `installerKind`、
/// `allowedInstallerSuffixes` 和静默安装检查都会拿这些字符串
/// 作比较。
pub fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

/// 按 Node 方式拼写的 `process.arch`（`x86_64` 会选错资产）。
pub fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/* ------------------------------------------------------------------ */
/* 持久化状态                                                           */
/* ------------------------------------------------------------------ */

/// 内存中的 `update-state.json`。
///
/// 字段顺序只影响文件的字节：[`State::to_value`] 按参考实现字面量的顺序
/// 写入七个基础键，然后写入 `extra`（未知键、
/// `downloadedPath`/`downloadedVersion`），顺序与读入/加入时相同 ——
/// 结果等同于 `{ ...base, ...raw }`。
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

/// `readState()` —— 基础默认值，被文件里的内容覆盖。
///
/// 参考实现把解析出的对象展开到默认值之上；因此未知键会
/// 存活并被写回。损坏/缺失的文件会退回默认值
/// （损坏的状态文件绝不能把启动搞瘫）。
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
            // `Number` 字段会被强制转换；被手工改成字符串的值会让
            // 调度
            // 计算出错。这里加固是刻意的（见模块文档）。
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
/* 管理器                                                               */
/* ------------------------------------------------------------------ */

/// 一次发布查询的响应。
///
/// manager 需要状态码（404 = "还没有发布"，403/429 = 冷却期）
/// 和响应头（`Retry-After`、`X-RateLimit-*`），所以它使用
/// `http::fetch_release` 及其 [`ReleaseFetch`]，而不是只返回 body 的
/// `fetch_release_json`。
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
/// 启动器注入点，做成 public 是为了让集成测试能断言
/// `install()` 而不真正执行文件。生产环境始终传
/// `None`，从而得到 [`default_launcher`]。
pub type LauncherFn = Arc<dyn Fn(&Path, bool) -> Result<String, UpdateError> + Send + Sync>;
type LaunchFn = LauncherFn;
type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// manager 的设置视图（`settings()`）。
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

/// 非注入点的构造参数（注入点是字段，因此
/// 测试可以一次替换一个）。
///
/// `Debug` 是手写的：`LauncherFn` 是 `dyn Fn`，并不实现 `Debug`。
#[derive(Clone)]
pub struct Options {
    pub platform: String,
    pub arch: String,
    pub app_version: String,
    /// `options.dir` —— 默认为 `<root>/updates`。
    pub dir: Option<PathBuf>,
    /// `options.maxBytes`；0 表示参考实现的默认值（`|| MAX_INSTALLER_BYTES`）。
    pub max_bytes: u64,
    /// `options.launcher` 的取值。`None` 表示用
    /// [`default_launcher`]，它会真正执行安装器 —— 测试会设置它，
    /// 这样 `install()` 就能被断言而无需派生进程。
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
    /// 进行中的检查最终解析成的值。第二个调用者会等待它
    /// 而不是发起第二次请求（参考实现共享同一个 promise）。
    result: Option<Value>,
}

/// 更新管理器：每进程一个，类似参考实现的 `buildContext()`。
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

    // 注入点，对应参考实现的构造函数（`http`、`now`、
    // `launcher`、`logger`）。测试在构造之后替换它们。
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
    /// `createUpdateManager({ store })` —— 生产环境默认值。
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
            // 启动器绝不会捕获构造时的 `silent`：这个
            // 设置每次安装时都重新读取，与 `defaultLauncher` 一致。
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

    /* ---------------- 设置 ---------------- */

    /// `settings()` —— 每次调用都重新从磁盘读取。
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

    /// `settings()` 的形状（测试也用得上）。
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

    /* ---------------- 状态 ---------------- */

    /// UI 轮询的快照。每次调用的每个字段都存在。
    pub fn status(&self) -> Value {
        let cfg = self.settings_cfg();
        let (state, task) = {
            let inner = lock(&self.inner);
            (inner.state.clone(), inner.task.clone())
        };
        let checking = lock(&self.inflight).running;

        // `state.release || null` —— 任何假值都读作"还没检查过"。
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

    /// `_state()` —— 持久化的对象，用于保真测试。
    pub fn state_snapshot(&self) -> Value {
        lock(&self.inner).state.to_value()
    }

    /// `tokenStatus()` —— 令牌信封自己的视图
    /// （filled/origin/encryption），与一次检查无关。
    ///
    /// 虽然目前只有 `tests/update_pipeline.rs` 调用它，仍保持 public：
    /// 它是在没有网络往返的情况下观察已存令牌的唯一途径，
    /// 这正是"清除"可以被断言的原因。
    pub fn token_status(&self) -> Value {
        self.token.status()
    }

    /* ---------------- 计划 ---------------- */

    /// 调度器下次该跑检查的时刻（0 = 现在就该跑 / 未安排）。
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

    /// 调度器的心跳（每 30 s 调用一次）。`None` = 无事可做。
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

    /* ---------------- 检查 ---------------- */

    /// 对应参考实现的 `check({ manual })` 调用形态。
    ///
    /// 手动检查与计划检查走同一条路径；手动检查会忽略
    /// `nextCheckAt`，但仍然尊重针对 `lastAttemptAt` 的 60 秒间隔。
    pub fn check(&self, manual: bool) -> Value {
        // 并发调用者共享进行中的结果（`if (checking) return
        // checking`）。在这里等待就是 await 同一个
        // promise 的阻塞等价物。
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
            // 冷却期的答复来自 GitHub；提前重试只会再换来
            // 一个 403。
            if inner.state.retry_at > started_at {
                let retry_at = inner.state.retry_at;
                drop(inner);
                let mut value = self.status();
                set_key(&mut value, "ok", Value::Bool(false));
                set_key(&mut value, "reason", json!("cooldown"));
                set_key(&mut value, "retryAt", json!(retry_at));
                return value;
            }
            // 60 秒间隔只在之前一次检查*成功*过时才生效。
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

        // 占下这个槽位。发生竞争的调用者会拿到这次产生的
        // 结果。
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

        // `writeState({ lastAttemptAt: startedAt })` 发生在 fetch 之前：
        // 请求中途崩溃对调度器来说仍算一次尝试。
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
        let encoded_repo = cfg.repo.clone(); // normalizeRepo 已经限制了字符集
        let only_prerelease = cfg.beta && version::is_prerelease_version(&self.app_version);
        let url = if cfg.beta {
            format!("{}/repos/{}/releases?per_page=30", base, encoded_repo)
        } else {
            format!("{}/repos/{}/releases/latest", base, encoded_repo)
        };

        let token = self.token.effective();
        // `update/http.rs` 刻意不接受任何代理设置（只走直连
        // 路由）；因此 `updateProxy` 会影响下载，但不影响检查。
        // 这是有文档记录的缺口，不是这里的疏忽。
        let response = (self.fetch)(&url, token.as_deref()).map_err(|error| FetchFailure {
            message: error.message().to_string(),
            // 传输/解析失败发生在 `retryDeadline` 之前，所以
            // 错误里没有 `retryAt`。
            retry_at: 0,
        })?;

        let headers: Vec<(String, String)> = response
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        let deadline = retry_deadline(&headers, response.status, (self.now)());

        if response.status == 404 {
            // 还没有任何发布的仓库，或者只实现了
            // `/releases/latest` 的镜像。是"没东西可升级"，不是失败。
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

    /// `clearCooldown()` —— 换路由/换凭据会让另一个配额桶挣来的
    /// 冷却期失效。
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

    /* ---------------- 下载 ---------------- */

    /// `startDownload` —— 返回初始任务快照，传输在
    /// 后台线程上进行（渲染层轮询 `progress`）。
    ///
    /// 任务进行中时的第二次调用会返回正在运行的快照
    /// 而不是再发起一次传输；URL 校验在这项检查
    /// 之后，顺序与上游一致。
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
        // `name || fallbackName` —— 空字符串会落到 fallback。
        let chosen = match name {
            Some(text) if !text.is_empty() => text,
            _ => &fallback_name,
        };
        let filename = version::safe_file_name(chosen, &self.platform);

        let _ = std::fs::create_dir_all(&self.update_dir);
        let file_path = self.update_dir.join(&filename);
        // 同版本重新下载会覆盖；否则会留下一份陈旧副本。
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
            // 构建之后重新检查：两次竞争的点击绝不能引发两次
            // 传输。（参考实现理论上也会让两者都通过；这里是唯一
            // 一个第二次传输严格更糟的地方。）
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

    /// `progress()` / `taskSnapshot()` —— 任务或 `null`。
    pub fn progress(&self) -> Value {
        lock(&self.inner).task.clone().unwrap_or(Value::Null)
    }

    /// `cancel()` —— 只有进行中的任务能被取消。
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

    /* ---------------- 安装 ---------------- */

    /// 校验渲染层想执行的路径。
    ///
    /// 四个条件，缺一不可：绝对路径、本平台的扩展名、
    /// 位于受管的下载目录之内，以及文件名里含有来自
    /// `updateAssetKeyword` 的产品关键字。返回真实路径。
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

        // `fs.realpathSync` + `statSync(...).isFile()`；两种失败共用一条
        // 消息。`canonicalize` 像 realpath 那样跟随符号链接。
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

        // `path.relative(rootReal, real)` 以 ".." 开头 → 在外面。
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

        // 不是上游的规则：配置错误的 `updateRepo` 绝不能安装
        // *另一个*项目的安装器。
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

    /// 启动已下载的安装器。调用方紧接着就会退出应用。
    ///
    /// 顺序沿用参考实现：先校验路径，*然后*才在有下载
    /// 进行时拒绝执行。
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

    /* ---------------- 设置 / 令牌 / 跳过 ---------------- */

    /// `ak:update-settings`，守卫与写入合成一步 —— 这就是 `lib.rs` 调用的。
    ///
    /// 这些命令函数对该 crate 是私有的，所以测试无法触及
    /// `update_save_settings`；没有这一步，"先守卫、后写入"这个
    /// 顺序就是四行谁也测不红的代码。它同时也是纵深防御：
    /// 将来 Rust 侧的调用方也会得到同样的规则，而不只是
    /// 渲染层那条路径。
    pub fn save_settings_checked(&self, patch: &Value) -> Result<Value, UpdateError> {
        if let Some(message) = validate_settings_patch(patch) {
            return Err(UpdateError::new(message, 400));
        }
        self.save_settings(patch)
    }

    /// `saveSettings(patch)` —— 夹取/去空白、持久化、调整计划。
    ///
    /// IPC 层会先校验原始载荷并返回
    /// `{ ok:false, error }`；这个方法是 manager 那一半。注意它是
    /// *夹取*而不是拒绝：`minutes: 0` 会变成 `1`，而畸形的
    /// `repo` 会被原样存下，读回来时才表现为内置的那个。
    /// 拒绝是守卫的职责，不是这个方法的。
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

        // 拨动开关会清空计划：关 → 什么都不跑；开且
        // 没有待处理的截止时间 → 下一次心跳立刻查询。
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
        // 换路由会让另一个配额桶挣来的冷却期失效。
        if patch.get("proxy").is_some() {
            self.clear_cooldown();
        }

        Ok(self.status())
    }

    /// `ak:update-token`，守卫与写入合成一步 —— 这就是 `lib.rs` 调用的。
    ///
    /// 理由与 [`UpdateManager::save_settings_checked`] 相同。BUG-1 有两半，
    /// 而这是唯一一处两半都可断言的接缝：body 必须能
    /// 挺过 `null`，*并且* `None` 必须原封不动地传到 `set_token` ——
    /// 原来的 `set_token(Some(&token))` 正是让 `None` 够不到的原因。
    pub fn set_token_checked(&self, payload: &Value) -> Result<Value, UpdateError> {
        let token = token_arg(payload).map_err(|message| UpdateError::new(message, 400))?;
        self.set_token(token.as_deref())
    }

    /// `setToken(token)` —— `None` 表示清除（`token === null`）。
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

    /// "跳过这个版本" —— 按版本记在 `update-state.json` 里。
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

/// 一次发布查询产生的失败，附带参考实现挂在
/// 其 `UpdateError` 上的冷却期（`UpdateError` 本身带不了）。
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
/* 下载工作线程                                                          */
/* ------------------------------------------------------------------ */

/// `startDownload` 的后台那一半。
///
/// 进度回调既报告进度也强制执行体积上限：上限是在*这里*
/// 用完全相同的措辞检查的，而不是由 HTTP 层检查。
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
            // 按规范，在更新进度之前检查。
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

            // 对应参考实现的 `updateProgress(received, declared)`。
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

            // 先 `if (task) { active = false; done = true; ... }`，然后
            // `writeState({ downloadedPath, downloadedVersion })`，再由
            // `finally` 把 active 置为 false。这次写入发生在任务
            // 停止报告 `active` 之前。
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
                // 解析口径: 取消路径会 resolve 流式 promise，
                // 并在删除半成品的 `catch` 之前从 `try` 返回，
                // 因此半成品安装器会留存。只有
                // 失败才会删除。任务仍然报告 path=null。
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

/// 任务的 `finally`：active=false，以及各个终态标记。
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
/* 启动器                                                               */
/* ------------------------------------------------------------------ */

/// 把安装器交给 OS。
///
/// Windows + 静默使用 NSIS 的 `/S /R`（静默，安装后重新启动）。`core/` 里
/// 没有 OS 的"打开路径"API，所以其他情况都是普通的 spawn，
/// 并报告 `mode: "spawn"`（渲染层只显示这个模式，从不
/// 据此分支）。
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
/* 路径与 URI 辅助函数                                                   */
/* ------------------------------------------------------------------ */

/// Node 的 `path.win32.isAbsolute` / `path.posix.isAbsolute` 判定。
fn is_absolute_js(text: &str) -> bool {
    let bytes = text.as_bytes();
    if cfg!(windows) {
        // `\foo`、`/foo`、`\\server\share`、`C:\foo` —— Node 接受没有盘符的
        // 根路径，而 `Path::is_absolute` 会拒绝它。
        (!bytes.is_empty() && (bytes[0] == b'/' || bytes[0] == b'\\'))
            || (bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'/' || bytes[2] == b'\\'))
    } else {
        text.starts_with('/')
    }
}

/// Windows 上的 `fs.realpathSync` 返回 `C:\…`；`std::fs::canonicalize` 会加上
/// verbatim 前缀。去掉它，好让交给渲染层和
/// 启动器的值保持一致。
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

/// `path.resolve` 用于可能不存在的路径：按词法折叠 `.`/`..`。
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

/// 对应 `!relative.startsWith('..') && !path.isAbsolute(relative)`。
///
/// 同一目录的两种写法比较结果相等（逐段比较，Windows 上
/// 折叠大小写）。对以 `..` 开头的*文件名*的拒绝是解析口径：
/// `path.relative` 对一个真叫这个名字的文件返回 `..foo.exe`，
/// 它以 `..` 开头，因此被拒绝。
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

/// `decodeURIComponent` —— 百分号解码，再按 UTF-8 解释；格式错误的输入像
/// `URIError` 那样报错。
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
/* 进程级单例                                                            */
/* ------------------------------------------------------------------ */

/// 每进程一个管理器，对应参考实现的 `buildContext()`。
///
/// 第一个 root 胜出（生产环境只有一个）；测试则直接构造
/// 一个独立的 [`UpdateManager`]。
pub fn ensure_manager(root: &str) -> &'static UpdateManager {
    static CELL: OnceLock<UpdateManager> = OnceLock::new();
    CELL.get_or_init(|| UpdateManager::new(root))
}

/* ------------------------------------------------------------------ */
/* 测试                                                                 */
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
        // 设置默认值来自 `Store.defaultSettings()`。
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

        // 读取时会去掉 apiBase 末尾的斜杠。
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

        // 状态文件：键精确、顺序精确、末尾有换行。第一个
        // 键是 `lastAttemptAt`，它带的是*注入的*时钟
        // （参考实现的 `writeState({ lastAttemptAt: startedAt })`），不是
        // 墙上时间。
        let raw = std::fs::read_to_string(state_path(&root)).unwrap();
        assert!(raw.ends_with("}\n"));
        let first_key = raw.lines().nth(1).unwrap().trim().trim_end_matches(',');
        assert_eq!(first_key, format!("\"lastAttemptAt\": {}", NOW));
        let saved: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(saved["release"]["tag"], json!("9.9.9"));
        assert_eq!(saved["skippedVersion"], json!(""));

        // 60 秒内的第二次手动检查会复用答案，不发请求。
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

        // 间隔内的计划检查仍会执行（节流只作用于手动检查）。
        http.push(ok_release("v9.9.9"));
        let scheduled = m.check(false);
        assert_eq!(scheduled["ok"], json!(true));
        assert_eq!(http.request_count(), before + 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn check_404_is_a_successful_no_release_answer() {
        let root = temp_root("check-404");
        let http = MockHttp::new(); // 空队列 → 404
        let m = manager(&root, "1.0.0", http);

        let result = m.check(true);
        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["latest"], Value::Null);
        assert_eq!(result["hasUpdate"], Value::Null);
        assert_eq!(result["comparison"], Value::Null);
        // 404 会写入 lastSuccessAt：这是一个真实的答案，不是失败。
        assert_eq!(result["checked"], json!(true));
        assert_eq!(result["lastError"], Value::Null);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn check_403_sets_a_cooldown_from_the_reset_header() {
        let root = temp_root("check-403");
        let http = MockHttp::new();
        let reset = (NOW / 1000) + 120; // epoch 秒
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

        // 下一次检查直接由冷却期作答，不发请求。
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
        // Retry-After 秒数。
        assert_eq!(
            retry_deadline(&[("retry-after".into(), "5".into())], 500, NOW),
            NOW + 5000
        );
        // 403 且无响应头：一分钟下限。
        assert_eq!(retry_deadline(&[], 403, NOW), NOW + 60_000);
        assert_eq!(retry_deadline(&[], 429, NOW), NOW + 60_000);
        // 只有剩余额度为 0 时 X-RateLimit-Reset 才算数。
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
        // 一个 HTTP 日期（IMF-fixdate）。
        assert_eq!(
            retry_deadline(
                &[("Retry-After".into(), "Sun, 06 Nov 1994 08:49:37 GMT".into())],
                200,
                NOW
            ),
            784_111_777_000
        );
        // 无法解析的值不贡献任何东西。
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
        // 错误对象上的 `retryAt` 是 0 → 没有冷却期。
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

        // 重启后依然存在：第二个管理器读的是同一个状态文件。
        let restarted = manager(&root, "1.0.0", Arc::clone(&http));
        assert_eq!(restarted.status()["skipped"], json!(true));
        assert_eq!(restarted.status()["latest"], json!("9.9.9"));

        assert_eq!(restarted.unskip()["skipped"], json!(false));
        // `m` 是另一个管理器，它有自己那份内存状态副本
        // （每个 manager 在构造时只读一次 state），所以清空后的值
        // 只在执行了 `unskip` 的那个实例上可见。
        assert_eq!(restarted.status()["skippedVersion"], json!(""));

        // 跳过一个显式指定的版本（IPC 路径传 `payload.version`）。
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
        // `Number(null) === 0`，所以
        // `clampMinutes(null)` 是 1，不是默认值。只有非有限输入
        // （NaN/Infinity，即设置键缺失时的 `undefined`）才会得到
        // DEFAULT_MINUTES —— 而且
        // `Store.defaultSettings()` 总会写入 `updateCheckMinutes: 20`，所以
        // 管理器永远碰不到那种情况。
        assert_eq!(clamp_minutes(&Value::Null), 1);

        assert_eq!(normalize_repo(" acme/keeper "), "acme/keeper");
        assert_eq!(normalize_repo("acme/keeper.git"), "acme/keeper.git");
        assert_eq!(normalize_repo("acme"), "");
        assert_eq!(normalize_repo("a/b/c"), "");
        assert_eq!(normalize_repo("ac me/keeper"), "");
        assert_eq!(normalize_repo("/keeper"), "");
    }

    /// `ak:update-settings` 的守卫。
    ///
    /// 没有它，`update_save_settings` 会把不可用的值夹成一个
    /// 看起来可用的值再持久化，于是用户只有在下次检查失败时
    /// 才知道自己的 repo/proxy 有问题。
    #[test]
    fn the_settings_guard_rejects_every_value_outside_the_range() {
        // minutes：先 `Math.round(Number(v))`，再要求 1..=1440。
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

        // repo：去空白；空值回退到内置仓库，所以是
        // 合法的。非字符串的 `asString()` 是 `''` —— 同样合法。
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

        // apiBase：非空时须满足 `^https?://`。
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

        // proxy：auto | direct | `^https?://\S+$`；空值退化为 auto。
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

        // 补丁没带的键绝不会被检查，因此部分保存
        // （比如只带 `auto`）不会因为没发某个字段而被拒绝。
        assert_eq!(validate_settings_patch(&json!({ "auto": false })), None);
        assert_eq!(validate_settings_patch(&json!({})), None);
    }

    /// `ak:update-token` 的载荷校验 — 渲染层的"清除令牌"按钮发送 `null`，而
    /// `String` 参数连反序列化都无法接受它。
    ///
    /// 完整契约（包括缺失字段的分支）在
    /// `tests/update_pipeline.rs` 的
    /// `token_arg_turns_null_into_clear_and_rejects_other_types` 一例中
    /// 断言；这一个只是紧挨着代码的快速单元级绊线。
    #[test]
    fn the_token_argument_guard_turns_null_into_clear() {
        // `null` 表示清除 —— 这正是 BUG-1 的全部要点。
        assert_eq!(token_arg(&json!({ "token": null })), Ok(None));
        assert_eq!(
            token_arg(&json!({ "token": "ghp_example" })),
            Ok(Some("ghp_example".to_string()))
        );
        assert_eq!(token_arg(&json!({ "token": 5 })), Err(ERR_TOKEN_TYPE));
        assert_eq!(token_arg(&json!({ "token": true })), Err(ERR_TOKEN_TYPE));
        // 没有该字段的 body 是错误，绝不是静默清除。
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
                    // manager 仅凭自己的标志位来分类这种情况。
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
        // 解析口径: 取消路径在 unlink 那个 `catch` 之前就返回。
        assert!(Path::new(&path).exists(), "partial file survives a cancel");

        // 没什么可再取消的了。
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
            // 声明 50 MB，但在回调说不行之前一个字节都没写。
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

        // 存储层看到的是 manager 的键名，不是补丁里的键名。
        let stored = settings_store::read(&root);
        assert_eq!(stored["updateCheck"], json!(true));
        assert_eq!(stored["updateCheckMinutes"], json!(1));
        assert_eq!(stored["updateInstallSilent"], json!(false));

        // 之前有过一次尝试时，改分钟数会重新计算截止时间。
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
                // `lastAttemptAt` 在请求发出之前就已写入。
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
        // 返回的快照是在检查*内部*构建的，早于 `finally`
        // 清除进行中标志（此时读 `status()`，
        // `checking` 仍是那个 promise），所以结果里带着
        // `checking: true`。只有之后的 `status()` 才看到它被清掉。
        assert_eq!(result["checking"], json!(true));
        assert_eq!(m.status()["checking"], json!(false));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn concurrent_checks_share_one_request() {
        let root = temp_root("coalesce");
        let http = MockHttp::new();
        http.push(ok_release("v9.9.9"));
        // 任何第二个响应只会被第二次请求消费。
        http.push(ok_release("v9.9.8"));
        let m = Arc::new(manager(&root, "1.0.0", Arc::clone(&http)));

        let a = Arc::clone(&m);
        let one = std::thread::spawn(move || a.check(true));
        // 给第一个请求留出占下槽位的时间。
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

        // 没有历史 → 现在就该跑，而不是隔一个间隔。
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
