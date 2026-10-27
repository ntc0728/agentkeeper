//! 软件更新纯函数 —— 版本比较、发布选择与资产筛选。
//!
//! 版本比较、发布挑选、资产选择、文件名加固
//! 以及主机白名单。这里不做任何 IO，这正是它
//! 可以直接测试的原因；IO 那一半在 `manager`、`http` 和 `token` 中。
//!
//! ## 为什么预发布段参与排序
//!
//! "加入 beta"通道会把 `0.3.0-beta.1` 和 `0.3.0-beta.2` 并列摆出。
//! 只比较三个核心数字会把它们判为相等，于是 beta 用户永远
//! 收不到 beta.2。semver 规则 11 规定预发布排在自己的
//! 正式版之下（`0.3.0-beta.2 < 0.3.0`），这让
//! beta.1 → beta.2 → 0.3.0 成为一条每一跳都是升级的链条。
//!
//! ## 保真度
//!
//! 行为是固定的而非重新设计：更新逻辑里有 quirks 的地方，quirk 会被
//! 原样复现并加注注释，因此在这里改动它就会改变
//! 哪些更新会被推送。
//! 值得注意的有：
//!
//! * `core` 各段是 `Number`，即 `f64`。
//!   因此两个大于 2^53 的不同整数比较结果相等，而 400 位的
//!   分量会解析为 `Infinity`（此时两边比较相等）。
//! * `1.2.3.4` 和 `1.2.3_beta` 忽略后缀，按正式版
//!   比较；只有 `-` 才开始一个预发布段。
//! * `pick_installer` 在 Linux 或 macOS 上永远返回不了资产：每个
//!   `.AppImage` / `.deb` / `.dmg` 候选都被
//!   `is_non_installer_asset` 拒绝，所以只有 Windows 路径可达。
//! * `installer_suffix` 返回 `"AppImage"`（大写 A），而
//!   `allowed_installer_suffixes` 返回 `"appimage"`（小写）。
//! * `UpdateError` 携带的是 `status`（400/502），而不是 `code`；`code()` 是
//!   Rust 侧新增的稳定判别器，让调用方不必去解析
//!   中文消息文本。
//!
//! ## URL 解析
//!
//! 使用 WHATWG 的 `URL` 解析器。没有引入 URL crate 作为
//! 依赖（而且
//! 引入也不在范围内），所以 `assert_downloadable` 自带一个小型
//! 解析器，实现该判定所依赖的部分：协议检测、
//! 特殊协议的正斜杠/反斜杠跳过、userinfo、端口校验、
//! 百分号解码并转小写的主机名，以及路径提取。与
//! WHATWG 的偏差刻意保持保守 —— IDNA 和百分号编码的非 ASCII
//! 主机被拒绝而不是转成 punycode，`href` 是预处理后的输入
//! 而非重新序列化的结果（不做默认端口省略），并且 `pathname`
//! 不做百分号或点分段的归一化。这意味着这个解析器可能
//! 拒绝一个 WHATWG 解析器接受的 URL，但绝不会接受
//! 一个它拒绝的 URL。

use regex::Regex;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::fmt;
use std::sync::OnceLock;

use crate::ecma_trim;
use crate::model::{js_slice, js_truthy};

/// GitHub API 根地址（可按调用覆盖，以支持自建镜像）。
pub const GITHUB_API: &str = "https://api.github.com";

/// 发布资产由这些主机提供（以及 `*.githubusercontent.com`）。
pub const ALLOWED_HOSTS: [&str; 4] = [
    "github.com",
    "www.github.com",
    "objects.githubusercontent.com",
    "codeload.github.com",
];

pub const ALLOWED_HOST_SUFFIX: &str = ".githubusercontent.com";

/// 在撑爆磁盘之前拒绝荒唐的 body（与上游相同的上限）。
pub const MAX_INSTALLER_BYTES: u64 = 600 * 1024 * 1024;

/* ------------------------------------------------------------------ */
/* 错误                                                                */
/* ------------------------------------------------------------------ */

/// 带 HTTP 类状态码的更新失败，对应参考实现的 `UpdateError`。
///
/// 形状：`{ name: 'UpdateError', message, status }`，`status` 默认
/// 为 502。参考类中并不存在 `code`；它是新增的、
/// 给 Rust 调用方用的稳定判别器（现有调用方只读 `.message`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateError {
    message: String,
    status: u16,
    code: &'static str,
}

impl UpdateError {
    /// `new UpdateError(message, status)`；默认 status 为 502。
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            status,
            code: "update-error",
        }
    }

    fn invalid_url() -> Self {
        Self {
            message: "the download address is not a valid URL".to_string(),
            status: 400,
            code: "invalid-url",
        }
    }

    fn https_required() -> Self {
        Self {
            message: "the download address must be https".to_string(),
            status: 400,
            code: "https-required",
        }
    }

    fn host_not_allowed(host: &str) -> Self {
        Self {
            message: format!("download address host is not on the allow list: {}", host),
            status: 400,
            code: "host-not-allowed",
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// HTTP 类状态码：被拒绝的 URL 为 400，默认值为 502。
    pub fn status(&self) -> u16 {
        self.status
    }

    /// 稳定的种类判别器（Rust 侧新增；见该类型的文档）。
    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UpdateError {}

/* ------------------------------------------------------------------ */
/* 版本解析与比较                                                        */
/* ------------------------------------------------------------------ */

/// 解析结果的形态是 `{ core: [major, minor, patch], pre: string | null }`。
///
/// 核心数字是 `f64`，因为它们以 `Number` 存储 —— 见
/// 模块文档中关于 >2^53 行为的说明。
#[derive(Debug, Clone, PartialEq)]
pub struct SplitVersion {
    pub core: [f64; 3],
    pub pre: Option<String>,
}

fn leading_digits(text: &str) -> usize {
    text.bytes().take_while(|b| b.is_ascii_digit()).count()
}

/// 把 `v1.2.3-beta.1+build` 拆成 `{ core: [1,2,3], pre: "beta.1" }`。
///
/// 只认 ASCII 数字（此处 `\d` 不带 `/u` 等价）。缺少
/// `-` 后缀意味着"没有预发布段"：`1.2.3.4` 和 `1.2.3_beta` 是
/// 非 semver 的写法，保留"忽略后缀"的行为，因此它们
/// 按正式版比较。构建元数据（`+…`）永远不影响顺序。
pub fn split_version(text: &str) -> Option<SplitVersion> {
    let trimmed = ecma_trim(text);
    let body = match trimmed.strip_prefix(['v', 'V']) {
        Some(rest) => rest,
        None => trimmed,
    };

    let mut core = [0.0_f64; 3];
    let mut rest = body;
    for (i, slot) in core.iter_mut().enumerate() {
        let len = leading_digits(rest);
        if len == 0 {
            return None;
        }
        // 超过 f64 容量的数字串会变成 `Infinity`，与
        // `Number()` 的行为完全一致。
        *slot = rest[..len].parse::<f64>().unwrap_or(f64::INFINITY);
        rest = &rest[len..];
        // 只有前两个分量后面必须是 '.'。
        if i < 2 {
            if !rest.starts_with('.') {
                return None;
            }
            rest = &rest[1..];
        }
    }

    let head = rest.split('+').next().unwrap_or("");
    // `head.length > 1` —— 单独的 "-" 不是预发布。
    let pre = match head.strip_prefix('-') {
        Some(tail) if !tail.is_empty() => Some(tail.to_string()),
        _ => None,
    };

    Some(SplitVersion { core, pre })
}

/// `[major, minor, patch]`，当 tag 不是语义化版本时为 `None`。
pub fn parse_version(text: &str) -> Option<[f64; 3]> {
    split_version(text).map(|split| split.core)
}

/// 这个 tag 是预发布吗（`-beta.1`）？未知形态按正式版处理。
pub fn is_prerelease_version(text: &str) -> bool {
    split_version(text)
        .map(|split| split.pre.is_some())
        .unwrap_or(false)
}

/// 字符串按 UTF-16 码元比较（`a > b`），
/// 而不是码点：增补字符（编码为从 D800 起的代理对）排在
/// E000..FFFF 的 BMP 字符*之前*。按字节序比较会搞错这一点。
fn js_str_cmp(a: &str, b: &str) -> Ordering {
    let mut left = a.encode_utf16();
    let mut right = b.encode_utf16();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
            }
        }
    }
}

/// 对应参考实现的 `/^[0-9]+$/ ? Number(text) : null`。
fn numeric_identifier(text: &str) -> Option<f64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(text.parse::<f64>().unwrap_or(f64::INFINITY))
}

/// semver 规则 11：数字标识符按数值比较（`beta.2 < beta.10`），
/// 数字排在字母数字之后，较短的前缀排在前面
/// 例如 `1.0.0-alpha` 就排在 `1.0.0-alpha.1` 之前）。
pub fn compare_prerelease(left: &str, right: &str) -> Ordering {
    let a: Vec<&str> = left.split('.').collect();
    let b: Vec<&str> = right.split('.').collect();

    let mut i = 0usize;
    loop {
        if i >= a.len() && i >= b.len() {
            return Ordering::Equal;
        }
        if i >= a.len() {
            return Ordering::Less;
        }
        if i >= b.len() {
            return Ordering::Greater;
        }

        let na = numeric_identifier(a[i]);
        let nb = numeric_identifier(b[i]);
        let order = match (na, nb) {
            // 这里不可能出现 NaN（只有数字），所以 `==`/`>` 与
            // 参考实现的 `na === nb` / `na > nb` 完全一致，包括 Infinity。
            (Some(x), Some(y)) => {
                if x == y {
                    Ordering::Equal
                } else if x > y {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => js_str_cmp(a[i], b[i]),
        };
        if order != Ordering::Equal {
            return order;
        }
        i += 1;
    }
}

/// 两侧都能解析时为 `Some(Ordering)`，任一侧不能解析则为 `None`。
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let left = split_version(a)?;
    let right = split_version(b)?;

    for i in 0..3 {
        if left.core[i] > right.core[i] {
            return Some(Ordering::Greater);
        }
        if left.core[i] < right.core[i] {
            return Some(Ordering::Less);
        }
    }

    match (&left.pre, &right.pre) {
        (None, None) => Some(Ordering::Equal),
        // 不带预发布标记的版本大于带标记的：1.0.0 > 1.0.0-beta.1
        (None, Some(_)) => Some(Ordering::Greater),
        (Some(_), None) => Some(Ordering::Less),
        (Some(left), Some(right)) => Some(compare_prerelease(left, right)),
    }
}

/* ------------------------------------------------------------------ */
/* 值类型转换                                                        */
/* ------------------------------------------------------------------ */

/// GitHub payload 中可能出现的 JSON 值对应的 `String(v)`。
fn js_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        // Array.prototype.toString === join(',')，其中 null/undefined 为 ""。
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

/// `String(v || '')` — 先取真值，再字符串化。
fn js_text_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(v) if js_truthy(v) => js_to_string(v),
        _ => String::new(),
    }
}

/// `size` 可能取的标量形态对应的 `Number(v)`。这是参考实现
/// 数字解析的一个子集（没有十六进制/八进制/二进制字面量）；实践中 JSON 的 size 就是数字。
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

/// Serde 像 `JSON.stringify` 那样把 `NaN`/`Infinity` 序列化为 `null`，
/// 并且整数不带小数部分（`12`，而不是 `12.0`）。
fn json_number(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        Value::from(value as i64)
    } else {
        Value::from(value)
    }
}

/* ------------------------------------------------------------------ */
/* 发布挑选                                                             */
/* ------------------------------------------------------------------ */

/// 从 `/releases` 列表里挑出"要升级到的那个版本"。
///
/// `/releases/latest` 已经排除了预发布，所以 beta 通道必须
/// 自行列出发布并挑选。规则：
///
///   * 草稿永远不算（用了令牌时它们才会出现）；
///   * 没有可解析版本的 tag 不参与；
///   * 其余情况取**版本号**最高的（不是列表顺序 —— 热修复可能
///     发布在更高版本之后）；
///   * `only_prerelease` 让 beta 用户留在 beta 通道：一旦你运行了
///     `0.3.0-beta.2`，正式的 `0.3.0` 就是"核心相同、无预发布"，
///     会被算作更新，从而把你拖出自己选择的通道。
///     在那里返回 `None` 是刻意的。
pub fn pick_listed_release<'a>(list: &'a Value, only_prerelease: bool) -> Option<&'a Value> {
    let items = list.as_array()?;
    let mut best: Option<&'a Value> = None;
    let mut first: Option<&'a Value> = None;

    for item in items {
        // `!item || typeof item !== 'object'`：只有对象（以及同样
        // 报为 'object' 的数组）参与。
        if !matches!(item, Value::Object(_) | Value::Array(_)) {
            continue;
        }
        // `item.draft === true` —— 真值字符串不算草稿。草稿在
        // 记录 `first` *之前*就被跳过：它们也绝不能成为
        // 正式通道的回退项。
        if item.get("draft") == Some(&Value::Bool(true)) {
            continue;
        }
        if first.is_none() {
            first = Some(item);
        }
        let tag = js_text_or_empty(item.get("tag_name"));
        if parse_version(&tag).is_none() {
            continue;
        }
        if only_prerelease && !is_prerelease_version(&tag) {
            continue;
        }
        let newer = match best {
            None => true,
            Some(current) => {
                compare_versions(&tag, &js_text_or_empty(current.get("tag_name")))
                    == Some(Ordering::Greater)
            }
        };
        if newer {
            best = Some(item);
        }
    }

    // "完全没有任何可解析项"时回退到最新一项 —— 但只对
    // 正式通道如此；beta 通道的含义是"没有 beta → 没有更新"。
    if only_prerelease {
        best
    } else {
        best.or(first)
    }
}

/* ------------------------------------------------------------------ */
/* URL、主机与资产                                                       */
/* ------------------------------------------------------------------ */

/// 本模块所需的 WHATWG `URL` 子集。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    /// 经 WHATWG 空白预处理后的输入。不重新序列化。
    pub href: String,
    /// 含冒号：`"https:"`。
    pub protocol: String,
    /// 转小写、百分号解码，不含端口与 userinfo。
    pub hostname: String,
    /// 仅路径，特殊协议至少为 `"/"`。
    pub pathname: String,
}

/// WHATWG 预处理：去掉首尾的 C0 控制字符与空格，然后
/// 移除所有制表符/换行。
fn preprocess_url(raw: &str) -> String {
    let trimmed = raw.trim_matches(|c: char| c <= '\u{20}');
    trimmed
        .chars()
        .filter(|c| *c != '\t' && *c != '\n' && *c != '\r')
        .collect()
}

fn split_scheme(text: &str) -> Option<(String, &str)> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    let mut i = 1;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'+' | b'-' | b'.'))
    {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b':' {
        return None;
    }
    Some((
        format!("{}:", text[..i].to_ascii_lowercase()),
        &text[i + 1..],
    ))
}

/// 外层 `None` = 端口非法；`Some(None)` = 省略/空端口。
fn parse_port(text: &str) -> Option<Option<u16>> {
    if text.is_empty() {
        return Some(None);
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u32 = text.parse().ok()?;
    if value > 65535 {
        return None;
    }
    Some(Some(value as u16))
}

const FORBIDDEN_HOST_CHARS: [char; 18] = [
    '\0', '\t', '\n', '\r', ' ', '#', '/', ':', '<', '>', '?', '@', '[', '\\', ']', '^', '|', '%',
];

/// 百分号解码、转小写，并校验 reg-name / IPv4 主机。
///
/// 偏差：非 ASCII 主机被拒绝（WHATWG 会做 IDNA 编码）；
/// 白名单上的主机都不是非 ASCII，因此只有特殊的 `apiBase` 会受影响。
fn decode_host(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }

    let text = String::from_utf8(decoded).ok()?;
    if !text.is_ascii() {
        return None;
    }
    let host = text.to_ascii_lowercase();
    if host
        .chars()
        .any(|c| FORBIDDEN_HOST_CHARS.contains(&c) || (c as u32) < 0x20)
    {
        return None;
    }
    Some(host)
}

/// 把去掉 userinfo 之后的 authority 拆成主机与可选端口。
fn split_host_port(authority: &str) -> Option<(String, Option<u16>)> {
    if authority.is_empty() {
        return Some((String::new(), None));
    }

    if let Some(rest) = authority.strip_prefix('[') {
        let close = rest.find(']')?;
        let inner = &rest[..close];
        let after = &rest[close + 1..];
        let port = if after.is_empty() {
            None
        } else {
            parse_port(after.strip_prefix(':')?)?
        };
        if inner.is_empty()
            || !inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.' || c == 'v' || c == 'V')
        {
            return None;
        }
        // WHATWG 在 `hostname` 中保留方括号。
        return Some((format!("[{}]", inner.to_ascii_lowercase()), port));
    }

    let (host_raw, port) = match authority.rfind(':') {
        Some(i) => (&authority[..i], parse_port(&authority[i + 1..])?),
        None => (authority, None),
    };
    Some((decode_host(host_raw)?, port))
}

fn split_path(tail: &str, special: bool) -> String {
    let end = tail.find(|c| c == '?' || c == '#').unwrap_or(tail.len());
    let raw = &tail[..end];
    let normalized = if special {
        raw.replace('\\', "/")
    } else {
        raw.to_string()
    };
    if normalized.is_empty() && special {
        "/".to_string()
    } else {
        normalized
    }
}

/// 处理某个协议的 authority 与路径。`special` 选择 WHATWG 规则，
/// 即 `\` 是路径分隔符、默认路径为 `/`。
fn parse_authority(rest: &str, special: bool, allow_empty_host: bool) -> Option<(String, String)> {
    let after = if special {
        rest.trim_start_matches(|c| c == '/' || c == '\\')
    } else {
        rest
    };
    let end = if special {
        after
            .find(|c| c == '/' || c == '\\' || c == '?' || c == '#')
            .unwrap_or(after.len())
    } else {
        after
            .find(|c| c == '/' || c == '?' || c == '#')
            .unwrap_or(after.len())
    };
    let authority = &after[..end];
    let tail = &after[end..];

    let host_port = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    let (host, _port) = split_host_port(host_port)?;
    if host.is_empty() && !allow_empty_host {
        return None;
    }
    Some((host, split_path(tail, special)))
}

fn parse_url(raw: &str) -> Option<ParsedUrl> {
    let cleaned = preprocess_url(raw);
    let (protocol, rest) = split_scheme(&cleaned)?;

    // `file:` 在 WHATWG 中是特殊协议但有自己的一套空主机规则；它
    // 永远过不了 https 检查，因此这里按普通协议解析。
    let special = matches!(
        protocol.as_str(),
        "http:" | "https:" | "ws:" | "wss:" | "ftp:"
    );

    let (hostname, pathname) = if special {
        parse_authority(rest, true, false)?
    } else if let Some(after) = rest.strip_prefix("//") {
        parse_authority(after, false, true)?
    } else {
        (String::new(), split_path(rest, false))
    };

    Some(ParsedUrl {
        href: cleaned,
        protocol,
        hostname,
        pathname,
    })
}

/// 在 GitHub 资产主机上只接受 https。
///
/// 下载路径是安装器上游的一道直接"执行这个文件"的边界，
/// 因此渲染层提供的 URL 不可信：被攻破的页面
/// 绝不能把更新器指向 `http://evil/x.exe`。主机被显式配置
/// （`apiBase`）的镜像则允许使用它被配置时的任意协议 —— 那是
/// 一个刻意的本地决定。
///
/// `api_base == ""` 是默认值（`{ apiBase = '' }`）。无法解析的
/// `apiBase` 会被静默忽略；见下面分支处的注释。
pub fn assert_downloadable(raw_url: &str, api_base: &str) -> Result<ParsedUrl, UpdateError> {
    let url = parse_url(raw_url).ok_or_else(UpdateError::invalid_url)?;
    let host = url.hostname.to_lowercase();

    // 解析口径：`new URL(apiBase)` 外层的 `catch` 只重抛 `UpdateError`，
    // 所以格式错误的 apiBase 被吞掉，退回默认的 GitHub 校验。
    if !api_base.is_empty() {
        if let Some(base) = parse_url(api_base) {
            let base_host = base.hostname.to_lowercase();
            if !base_host.is_empty()
                && (host == base_host || host.ends_with(&format!(".{}", base_host)))
            {
                if base.protocol == "https:" && url.protocol != "https:" {
                    return Err(UpdateError::https_required());
                }
                return Ok(url);
            }
        }
    }

    if url.protocol != "https:" {
        return Err(UpdateError::https_required());
    }
    let allowed = ALLOWED_HOSTS.contains(&host.as_str()) || host.ends_with(ALLOWED_HOST_SUFFIX);
    if !allowed {
        return Err(UpdateError::host_not_allowed(&host));
    }
    Ok(url)
}

/* ------------------------------------------------------------------ */
/* 安装器                                                              */
/* ------------------------------------------------------------------ */

/// 某个平台的主安装器扩展名（不含点）。
///
/// `platform` 是 Node 的取值（`"darwin"` / `"win32"`）；其他任何值，
/// 包括代表未定义参数的空字符串，都走
/// Linux 分支。
pub fn installer_suffix(platform: &str) -> &'static str {
    if platform == "darwin" {
        "dmg"
    } else if platform == "win32" {
        "exe"
    } else {
        "AppImage"
    }
}

/// 粗粒度的种类，供 UI 决定措辞（UAC / 挂载 / 包管理器）。
pub fn installer_kind(platform: &str) -> &'static str {
    if platform == "darwin" {
        "dmg"
    } else if platform == "win32" {
        "nsis"
    } else {
        "appimage"
    }
}

/// 该平台接受的扩展名；shell 端的运行检查用的是同一份
/// 列表。注意是小写拼写，与 [`installer_suffix`] 不同。
pub fn allowed_installer_suffixes(platform: &str) -> &'static [&'static str] {
    if platform == "darwin" {
        &["dmg"]
    } else if platform == "win32" {
        &["exe"]
    } else {
        &["appimage", "deb"]
    }
}

struct AssetPatterns {
    setup: Regex,
    portable: Regex,
    archive: Regex,
    foreign_exe: Regex,
}

fn asset_patterns() -> &'static AssetPatterns {
    static PATTERNS: OnceLock<AssetPatterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // `unicode(false)` is not cosmetic: 关掉 Unicode 模式后只折叠 ASCII，
        // 且 `\b` 是 ASCII 词边界，所以 `中portable.exe` 确实会匹配
        // `-?portable\b`（中 是 non-word char）。
        let build = |pattern: &str| {
            regex::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .unicode(false)
                .build()
                .unwrap_or_else(|e| panic!("update asset regex {:?} is invalid: {}", pattern, e))
        };
        AssetPatterns {
            setup: build(r"setup|install"),
            portable: build(r"-?portable\b"),
            archive: build(r"\.(zip|7z|tar\.gz|appimage|deb|rpm|snap|dmg)$"),
            foreign_exe: build(r"-(?:win|web|linux|mac)\.exe$"),
        }
    })
}

/// 绝不能被当作"安装器"的资产。
///
/// **便携版**什么也不安装（没有快捷方式、没有卸载项），
/// 而且会留下一个更新器永远不会升级的副本；压缩包 / AppImage /
/// deb / dmg 属于其他平台。
pub fn is_non_installer_asset(name: &str) -> bool {
    let patterns = asset_patterns();
    patterns.portable.is_match(name)
        || patterns.archive.is_match(name)
        || patterns.foreign_exe.is_match(name)
}

fn asset_name(item: &Value) -> String {
    // `String((item && item.name) || '')` —— 非对象没有 `name`。
    js_text_or_empty(item.get("name"))
}

/// 架构关键词，最具体的排在最前。为另一种
/// 架构构建的发布能装上却起不来，所以优先精确匹配。
fn arch_keywords(platform: &str, arch: &str) -> &'static [&'static str] {
    if platform == "darwin" {
        if arch == "arm64" {
            &["universal", "aarch64", "arm64", "x64"]
        } else {
            &["universal", "x64", "x86_64", "arm64"]
        }
    } else if platform == "win32" {
        if arch == "arm64" {
            &["arm64", "x64"]
        } else {
            &["x64", "amd64"]
        }
    } else if arch == "arm64" {
        &["aarch64", "arm64"]
    } else {
        &["x86_64", "amd64"]
    }
}

/// 为该平台/架构挑选安装器资产，形状为
/// `{ name, size, url }`，与参考实现的返回值一致。
///
/// Windows：NSIS 产物带 `-setup`，有时也会附带一个裸 exe —— 后者
/// 既不创建快捷方式也不创建卸载项，所以 `setup`
/// 必须胜出。当发布两者都没有时（自定义的 `artifactName`），剩下的
/// 第一个候选也好过一个被藏起来的按钮。
///
/// 口径：Linux 和 macOS 永远返回不了东西。`allowedInstallerSuffixes`
/// 允许 `.appimage` / `.deb` / `.dmg`，但 `isNonInstallerAsset` 拒绝了
/// 所有带这些扩展名的名字，所以那里的 `candidates` 始终为空。
pub fn pick_installer(assets: &Value, platform: &str, arch: &str) -> Option<Value> {
    let list: &[Value] = match assets.as_array() {
        Some(list) => list,
        None => &[],
    };

    for suffix in allowed_installer_suffixes(platform) {
        let dotted = format!(".{}", suffix);
        let candidates: Vec<&Value> = list
            .iter()
            .filter(|item| {
                let name = asset_name(item);
                name.to_lowercase().ends_with(&dotted) && !is_non_installer_asset(&name)
            })
            .collect();
        if candidates.is_empty() {
            continue;
        }

        let mut chosen: Option<&Value> = None;
        if platform == "win32" {
            chosen = candidates
                .iter()
                .copied()
                .find(|item| asset_patterns().setup.is_match(&asset_name(item)));
        }
        if chosen.is_none() {
            for &keyword in arch_keywords(platform, arch) {
                chosen = candidates
                    .iter()
                    .copied()
                    .find(|item| asset_name(item).to_lowercase().contains(keyword));
                if chosen.is_some() {
                    break;
                }
            }
        }
        let chosen = chosen.unwrap_or(candidates[0]);

        let size = js_number(chosen.get("size"));
        let size = if size.is_nan() { 0.0 } else { size }; // `Number(x) || 0`
        return Some(json!({
            "name": asset_name(chosen),
            "size": json_number(size),
            "url": js_text_or_empty(chosen.get("browser_download_url")),
        }));
    }
    None
}

/// 只取 basename，`\/:*?"<>|` 折叠为 `_`，并去空白。回退名总是保留
/// 平台后缀 —— 下游校验会检查扩展名，所以一个
/// 无扩展名的回退名会把"奇怪的资产名"变成"无法安装"。
pub fn safe_file_name(name: &str, platform: &str) -> String {
    let base = name.rsplit(|c| c == '/' || c == '\\').next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect();
    let cleaned = ecma_trim(&cleaned);
    if !cleaned.is_empty() {
        return cleaned.to_string();
    }
    format!("AgentKeeper-update.{}", installer_suffix(platform))
}

/// [`release_from_api`] 的可选参数；缺失的值为 `""`，而
/// 这里所有比较中 `undefined` 也都落到这个值上。
#[derive(Debug, Clone, Copy, Default)]
pub struct ReleaseOptions<'a> {
    pub platform: &'a str,
    pub arch: &'a str,
    pub api_base: &'a str,
    pub repo: &'a str,
}

/// GitHub Release → manager 缓存、UI 渲染所用的扁平形状。
/// `notes` 有上限：200 KB 的更新日志绝不该经过 IPC 传输。
pub fn release_from_api(payload: &Value, options: ReleaseOptions<'_>) -> Value {
    let text = |key: &str| js_text_or_empty(payload.get(key));

    let raw_tag = text("tag_name");
    let tag = match raw_tag.strip_prefix(['v', 'V']) {
        Some(rest) => rest.to_string(),
        None => raw_tag.clone(),
    };
    let page_url = text("html_url");

    let name = {
        let name = text("name");
        if name.is_empty() {
            raw_tag.clone()
        } else {
            name
        }
    };
    let published_at = {
        let published = text("published_at");
        if published.is_empty() {
            text("created_at")
        } else {
            published
        }
    };
    let page = if !page_url.is_empty() {
        page_url
    } else if !options.repo.is_empty() {
        format!("https://github.com/{}/releases", options.repo)
    } else {
        "https://github.com".to_string()
    };
    // `payload && payload.prerelease === true` —— 假值 payload 会被
    // `&&` 原样返回，所以该字段并不总是 bool。予以保留。
    let prerelease = match payload {
        Value::Object(_) => {
            Value::Bool(matches!(payload.get("prerelease"), Some(Value::Bool(true))))
        }
        other if !js_truthy(other) => other.clone(),
        _ => Value::Bool(false),
    };

    let asset = pick_installer(
        payload.get("assets").unwrap_or(&Value::Null),
        options.platform,
        options.arch,
    )
    .unwrap_or(Value::Null);

    json!({
        "tag": tag,
        "name": name,
        // `String(body).slice(0, 8000)`；代理对问题见 `js_slice`。
        "notes": js_slice(&text("body"), 8000),
        "publishedAt": published_at,
        "pageUrl": page,
        "prerelease": prerelease,
        "asset": asset,
        "apiBase": options.api_base,
    })
}

/* ------------------------------------------------------------------ */
/* 测试                                                                 */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_version_accepts_the_known_shapes() {
        let split = split_version("v1.2.3-beta.1+build").unwrap();
        assert_eq!(split.core, [1.0, 2.0, 3.0]);
        assert_eq!(split.pre.as_deref(), Some("beta.1"));

        assert_eq!(split_version(" 1.2.3 ").unwrap().pre, None);
        assert_eq!(split_version("V1.2.3").unwrap().core, [1.0, 2.0, 3.0]);
        // 非 semver 的写法保留"忽略后缀"的行为。
        assert_eq!(split_version("1.2.3.4").unwrap().core, [1.0, 2.0, 3.0]);
        assert_eq!(split_version("1.2.3_beta").unwrap().pre, None);
        assert_eq!(split_version("1.2.3+build").unwrap().pre, None);
        assert_eq!(
            split_version("1.2.3-beta+build").unwrap().pre.as_deref(),
            Some("beta")
        );
        // 单独的 "-" 不是预发布；"--beta" 只去掉一个连字符。
        assert_eq!(split_version("1.2.3-").unwrap().pre, None);
        assert_eq!(
            split_version("1.2.3--beta").unwrap().pre.as_deref(),
            Some("-beta")
        );
        // 前导零是数字，不是字符串。
        assert_eq!(split_version("01.002.0003").unwrap().core, [1.0, 2.0, 3.0]);

        // trim 去掉 BOM，但不去 U+0085。
        assert!(split_version("\u{feff}1.2.3\u{feff}").is_some());
        assert!(split_version("\u{85}1.2.3").is_none());

        assert!(split_version("1.2").is_none());
        assert!(split_version("vv1.2.3").is_none());
        assert!(split_version("").is_none());
        assert!(split_version("not a version").is_none());
    }

    #[test]
    fn parse_and_prerelease_shapes() {
        assert_eq!(parse_version("1.2.3"), Some([1.0, 2.0, 3.0]));
        assert_eq!(parse_version("1.2.3-beta.2"), Some([1.0, 2.0, 3.0]));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("nope"), None);

        assert!(is_prerelease_version("1.2.3-beta.1"));
        assert!(!is_prerelease_version("1.2.3"));
        assert!(!is_prerelease_version("1.2.3.4"));
        assert!(!is_prerelease_version("bad"));
    }

    #[test]
    fn prerelease_semver_ordering() {
        assert_eq!(compare_prerelease("beta.1", "beta.2"), Ordering::Less);
        assert_eq!(compare_prerelease("beta.2", "beta.10"), Ordering::Less);
        assert_eq!(compare_prerelease("alpha", "alpha.1"), Ordering::Less);
        assert_eq!(compare_prerelease("alpha.1", "alpha"), Ordering::Greater);
        assert_eq!(compare_prerelease("alpha", "alpha"), Ordering::Equal);
        // 数字标识符排在字母数字标识符之后。
        assert_eq!(compare_prerelease("0", "a"), Ordering::Less);
        assert_eq!(compare_prerelease("a", "0"), Ordering::Greater);
        // 字母数字比较按 ASCII（大写在小写之前）。
        assert_eq!(compare_prerelease("beta", "BETA"), Ordering::Greater);
        // 口径：both are numbers, so leading zeros are irrelevant.
        assert_eq!(compare_prerelease("01", "1"), Ordering::Equal);
        // 多出一个空标识符会让左边更长，因此更大。
        assert_eq!(compare_prerelease("a.", "a"), Ordering::Greater);
        assert_eq!(compare_prerelease("", ""), Ordering::Equal);
        // UTF-16 码元顺序：😀 以代理项开头，因此排得更低。
        assert_eq!(compare_prerelease("\u{1F600}", "\u{E000}"), Ordering::Less);
    }

    #[test]
    fn versions_form_an_upgrade_chain() {
        let chain = [
            "0.3.0-beta.1",
            "0.3.0-beta.2",
            "0.3.0-beta.10",
            "0.3.0",
            "0.3.1",
        ];
        for pair in chain.windows(2) {
            assert_eq!(
                compare_versions(pair[0], pair[1]),
                Some(Ordering::Less),
                "{} should be older than {}",
                pair[0],
                pair[1]
            );
            assert_eq!(compare_versions(pair[1], pair[0]), Some(Ordering::Greater));
        }

        assert_eq!(
            compare_versions("1.2.3", "1.2.3+build"),
            Some(Ordering::Equal)
        );
        assert_eq!(compare_versions("1.2.3.4", "1.2.3"), Some(Ordering::Equal));
        assert_eq!(
            compare_versions("1.2.3_beta", "1.2.3"),
            Some(Ordering::Equal)
        );
        assert_eq!(compare_versions("bad", "1.0.0"), None);
        assert_eq!(compare_versions("1.0.0", "also bad"), None);
        // 数字按 f64 舍入：above 2^53 两个不同的巨版本比较相等。
        assert_eq!(
            compare_versions(
                "999999999999999999999999.0.0",
                "999999999999999999999998.0.0"
            ),
            Some(Ordering::Equal)
        );
        // 数字串比 f64 能容纳的更长时，两边都是 Infinity。
        let huge = format!("{}.0.0", "9".repeat(400));
        let huger = format!("{}.0.0", "9".repeat(401));
        assert_eq!(compare_versions(&huge, &huger), Some(Ordering::Equal));
        assert!(split_version(&huge).unwrap().core[0].is_infinite());
    }

    #[test]
    fn picks_highest_parseable_non_draft() {
        let list = json!([
            { "tag_name": "v0.3.0-beta.2" },
            { "tag_name": "v0.3.0-beta.10" },
            { "tag_name": "v0.2.0" },
            { "tag_name": "v0.4.0", "draft": true },
            { "tag_name": "nonsense" }
        ]);

        assert_eq!(
            pick_listed_release(&list, false).unwrap()["tag_name"],
            "v0.3.0-beta.10"
        );
        assert_eq!(
            pick_listed_release(&list, true).unwrap()["tag_name"],
            "v0.3.0-beta.10"
        );
    }

    #[test]
    fn draft_flag_is_strictly_boolean() {
        // `draft === true` —— 真值字符串不会隐藏一个发布。
        let list = json!([
            { "tag_name": "v1.0.0", "draft": "true" },
            { "tag_name": "v2.0.0", "draft": true }
        ]);
        assert_eq!(
            pick_listed_release(&list, false).unwrap()["tag_name"],
            "v1.0.0"
        );

        // 真草稿在记录 `first` 之前就被跳过，因此它甚至不会
        // 成为"没有可解析项"时的回退。
        let drafts = json!([
            { "tag_name": "nightly", "draft": true },
            { "tag_name": "v1" }
        ]);
        assert_eq!(
            pick_listed_release(&drafts, false).unwrap()["tag_name"],
            "v1"
        );
    }

    #[test]
    fn beta_channel_never_falls_back_to_final() {
        let finals = json!([{ "tag_name": "v1.0.0" }]);
        assert!(pick_listed_release(&finals, true).is_none());

        // 正式通道仍会回退到第一项……
        let unparseable = json!([{ "tag_name": "nightly" }, { "tag_name": "v1" }]);
        assert_eq!(
            pick_listed_release(&unparseable, false).unwrap()["tag_name"],
            "nightly"
        );
        // ……即使那一项是数组；原始值会先被跳过。
        let odd = json!([null, 5, [{ "x": 1 }], { "tag_name": "nightly" }]);
        assert!(pick_listed_release(&odd, false).unwrap().is_array());
        // 不是列表也不会崩溃。
        assert!(pick_listed_release(&json!({ "not": "a list" }), false).is_none());
        // `tag_name` 会被字符串化：数组也能解析。
        let coerced = json!([{ "tag_name": ["1.2.3"] }]);
        assert!(pick_listed_release(&coerced, false).is_some());
    }

    #[test]
    fn download_allow_list_rejects_lookalikes() {
        assert!(assert_downloadable("https://github.com/a.exe", "").is_ok());
        assert!(assert_downloadable("https://www.github.com/a", "").is_ok());
        assert!(assert_downloadable("https://objects.githubusercontent.com/a", "").is_ok());
        assert!(assert_downloadable("https://x.githubusercontent.com/a", "").is_ok());
        assert!(assert_downloadable("https://GITHUB.COM/a", "").is_ok());

        for bad in [
            "https://evilgithub.com/x",
            "https://evil-github.com/x",
            "https://github.com.evil.com/x",
            "https://githubusercontent.com/x",
            "https://github.com./x",
        ] {
            let err = assert_downloadable(bad, "").unwrap_err();
            assert_eq!(err.status(), 400, "{}", bad);
            assert_eq!(err.code(), "host-not-allowed", "{}", bad);
            assert!(err.to_string().contains("not on the allow list"), "{}", bad);
        }

        let err = assert_downloadable("http://github.com/x", "").unwrap_err();
        assert_eq!((err.status(), err.code()), (400, "https-required"));

        let err = assert_downloadable("github.com/x", "").unwrap_err();
        assert_eq!(err.code(), "invalid-url");
        let err = assert_downloadable("https://", "").unwrap_err();
        assert_eq!(err.code(), "invalid-url");
        let err = assert_downloadable("https://github.com:99999/x", "").unwrap_err();
        assert_eq!(err.code(), "invalid-url");
        let err = assert_downloadable("https://not github.com/x", "").unwrap_err();
        assert_eq!(err.code(), "invalid-url");
    }

    #[test]
    fn url_parser_follows_whatwg_enough() {
        let url = assert_downloadable("  HTTPS://user:pass@GitHub.com/x  ", "").unwrap();
        assert_eq!(url.protocol, "https:");
        assert_eq!(url.hostname, "github.com");
        assert_eq!(url.pathname, "/x");

        // 百分号解码的主机与反斜杠当斜杠，WHATWG 都接受。
        assert_eq!(
            assert_downloadable("https://github%2Ecom/x", "")
                .unwrap()
                .hostname,
            "github.com"
        );
        let url = assert_downloadable("https://github.com\\@evil.com/x", "").unwrap();
        assert_eq!(url.hostname, "github.com");
        assert_eq!(url.pathname, "/@evil.com/x");

        // 缺少斜杠也能解析；空路径变成 "/"。
        assert_eq!(
            assert_downloadable("https:github.com", "")
                .unwrap()
                .pathname,
            "/"
        );
        assert_eq!(
            assert_downloadable("https://github.com?x=1", "")
                .unwrap()
                .pathname,
            "/"
        );
        // userinfo 无法夹带另一个主机。
        assert!(assert_downloadable("https://github.com@evil.com/x", "").is_err());
        // 可解析的非 https 协议会走到协议检查，而不是解析错误。
        assert_eq!(
            assert_downloadable("javascript:alert(1)", "")
                .unwrap_err()
                .code(),
            "https-required"
        );
    }

    #[test]
    fn api_base_mirror_rules() {
        // 显式配置的镜像主机（及其子域）可以绕过
        // GitHub 白名单。
        assert!(
            assert_downloadable("https://mirror.internal/a", "https://mirror.internal").is_ok()
        );
        assert!(
            assert_downloadable("https://sub.mirror.internal/a", "https://mirror.internal").is_ok()
        );
        assert!(
            assert_downloadable("https://notmirror.internal/a", "https://mirror.internal").is_err()
        );
        // https 的 base 会强制 https 下载……
        let err =
            assert_downloadable("http://mirror.internal/a", "https://mirror.internal").unwrap_err();
        assert_eq!(err.code(), "https-required");
        // ……http 的 base 则刻意允许 http。
        assert!(assert_downloadable("http://mirror.internal/a", "http://mirror.internal").is_ok());
        // Unparseable apiBase is silently ignored（解析口径，见 `assert_downloadable`）。
        assert!(assert_downloadable("https://github.com/a", "not a url").is_ok());
    }

    #[test]
    fn platform_suffixes_and_kinds() {
        assert_eq!(installer_suffix("darwin"), "dmg");
        assert_eq!(installer_suffix("win32"), "exe");
        // 这里是大写 A，而下面的后缀列表里是小写 —— 原样照抄。
        assert_eq!(installer_suffix("linux"), "AppImage");
        assert_eq!(installer_suffix("freebsd"), "AppImage");
        assert_eq!(installer_suffix(""), "AppImage");

        assert_eq!(installer_kind("darwin"), "dmg");
        assert_eq!(installer_kind("win32"), "nsis");
        assert_eq!(installer_kind("linux"), "appimage");

        assert_eq!(allowed_installer_suffixes("darwin"), &["dmg"][..]);
        assert_eq!(allowed_installer_suffixes("win32"), &["exe"][..]);
        assert_eq!(
            allowed_installer_suffixes("linux"),
            &["appimage", "deb"][..]
        );
    }

    #[test]
    fn non_installer_asset_filters() {
        assert!(is_non_installer_asset("AgentKeeper-0.2.0-portable.exe"));
        assert!(is_non_installer_asset("AgentKeeper-0.2.0-win.exe"));
        assert!(is_non_installer_asset("bundle.tar.gz"));
        assert!(is_non_installer_asset("AgentKeeper.AppImage"));
        assert!(!is_non_installer_asset("AgentKeeper-0.2.0-setup.exe"));
        assert!(!is_non_installer_asset("foo-portablex.exe"));
        // `\b` 只认 ASCII：中 是非单词字符，因此存在边界。
        assert!(is_non_installer_asset("中portable.exe"));
        // 压缩包规则要求扩展名之前有一个点。
        assert!(!is_non_installer_asset("APPIMAGE"));
    }

    #[test]
    fn installer_selection_and_the_platform_suffix_rule() {
        let assets = json!([
            { "name": "AgentKeeper-0.2.0-portable.exe", "size": 9, "browser_download_url": "https://github.com/p" },
            { "name": "AgentKeeper-0.2.0-setup.exe", "size": 12, "browser_download_url": "https://github.com/s" },
            { "name": "AgentKeeper-0.2.0-x64.exe", "size": 1, "browser_download_url": "https://github.com/x" }
        ]);
        let picked = pick_installer(&assets, "win32", "x64").unwrap();
        assert_eq!(picked["name"], "AgentKeeper-0.2.0-setup.exe");
        assert_eq!(picked["size"], 12);
        assert_eq!(picked["url"], "https://github.com/s");

        // 没有 setup/架构标记 → 取第一个候选；缺失值变成 0 / ""。
        let plain = json!([{ "name": "whatever.exe", "size": null }]);
        let picked = pick_installer(&plain, "win32", "x64").unwrap();
        assert_eq!(picked["size"], 0);
        assert_eq!(picked["url"], "");

        // 架构偏好：x64 胜过前面那个没有 `-setup` 的 amd64。
        let arch = json!([
            { "name": "AgentKeeper-0.2.0-amd64.exe", "size": 1, "browser_download_url": "https://github.com/a" },
            { "name": "AgentKeeper-0.2.0-x64.exe", "size": 2, "browser_download_url": "https://github.com/z" }
        ]);
        assert_eq!(
            pick_installer(&arch, "win32", "x64").unwrap()["name"],
            "AgentKeeper-0.2.0-x64.exe"
        );

        // 口径：Linux and macOS filter out every candidate, always.
        let linux = json!([{ "name": "AgentKeeper.AppImage", "size": 1 }]);
        assert!(pick_installer(&linux, "linux", "x64").is_none());
        let deb = json!([{ "name": "AgentKeeper.deb", "size": 1 }]);
        assert!(pick_installer(&deb, "linux", "x64").is_none());
        let mac = json!([{ "name": "AgentKeeper.dmg", "size": 1 }]);
        assert!(pick_installer(&mac, "darwin", "arm64").is_none());

        // 不是数组 → 没有候选。
        assert!(pick_installer(&json!({ "name": "x.exe" }), "win32", "x64").is_none());
    }

    #[test]
    fn safe_file_names() {
        assert_eq!(safe_file_name("a/b/c.exe", "win32"), "c.exe");
        assert_eq!(
            safe_file_name("C:\\Users\\x\\AgentKeeper.EXE", "win32"),
            "AgentKeeper.EXE"
        );
        assert_eq!(safe_file_name("a:b*c?.exe", "win32"), "a_b_c_.exe");
        assert_eq!(safe_file_name("", "win32"), "AgentKeeper-update.exe");
        assert_eq!(
            safe_file_name("   ", "linux"),
            "AgentKeeper-update.AppImage"
        );
        assert_eq!(safe_file_name("../../x.exe", "darwin"), "x.exe");
    }

    #[test]
    fn release_shape_is_the_parsed_shape() {
        let payload = json!({
            "tag_name": "v0.3.0",
            "name": "",
            "body": "notes",
            "published_at": "",
            "created_at": "2026-01-01T00:00:00Z",
            "html_url": "",
            "prerelease": true,
            "assets": [
                { "name": "AgentKeeper-0.3.0-setup.exe", "size": 5, "browser_download_url": "https://github.com/a.exe" }
            ]
        });
        let release = release_from_api(
            &payload,
            ReleaseOptions {
                platform: "win32",
                arch: "x64",
                repo: "o/r",
                ..Default::default()
            },
        );
        assert_eq!(release["tag"], "0.3.0");
        assert_eq!(release["name"], "v0.3.0");
        assert_eq!(release["notes"], "notes");
        assert_eq!(release["publishedAt"], "2026-01-01T00:00:00Z");
        assert_eq!(release["pageUrl"], "https://github.com/o/r/releases");
        assert_eq!(release["prerelease"], true);
        assert_eq!(release["asset"]["name"], "AgentKeeper-0.3.0-setup.exe");
        assert_eq!(release["apiBase"], "");

        // 没有 repo → 通用的 releases 页面；缺失 payload 也能容忍。
        let empty = release_from_api(&Value::Null, ReleaseOptions::default());
        assert_eq!(empty["pageUrl"], "https://github.com");
        assert_eq!(empty["asset"], Value::Null);
        // `payload && payload.prerelease === true` 会返回假值 payload。
        assert_eq!(empty["prerelease"], Value::Null);
    }

    #[test]
    fn update_error_carries_status_and_code() {
        let err = UpdateError::new("boom", 400);
        assert_eq!(err.status(), 400);
        assert_eq!(err.code(), "update-error");
        assert_eq!(err.message(), "boom");
        assert_eq!(err.to_string(), "boom");
        // 调用方省略状态码时，构造函数默认 502。
        assert_eq!(UpdateError::new("boom", 502).status(), 502);
    }

    #[test]
    fn the_pinned_constants() {
        assert_eq!(GITHUB_API, "https://api.github.com");
        assert_eq!(ALLOWED_HOST_SUFFIX, ".githubusercontent.com");
        assert_eq!(MAX_INSTALLER_BYTES, 600 * 1024 * 1024);
        assert!(ALLOWED_HOSTS.contains(&"objects.githubusercontent.com"));
    }
}
