//! 资产模型——资产长什么样，一个字段对一个字段。
//!
//! 输出刻意建成 `serde_json::Value` 而不是有类型的结构体：渲染层把每个字段都当作
//! 可选的，而用结构体太容易悄悄*丢掉*一个 UI 要读的字段
//! （比如漏掉 `summary.byOrigin`，渲染层就坏了）。
//! 显式写出的 `null` 让线上形态与渲染层读到的完全一致。

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 对 `str` 求 sha256，截断到 16 个十六进制字符——指纹、id 和 project slug
/// 都靠它做身份标识。
pub fn hash16(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(16);
    for b in digest.iter() {
        out.push_str(&format!("{:02x}", b));
        if out.len() >= 16 {
            break;
        }
    }
    out.truncate(16);
    out
}

/// `slug(s)`——转小写，`[a-z0-9一-龥]` 之外的字符一律折叠成
/// 一个连字符，两端去掉，兜底为 "unnamed"。
pub fn slug(s: &str) -> String {
    let src = if s.is_empty() { "unnamed" } else { s };
    let lowered = src.trim().to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut pending_dash = false;
    for ch in lowered.chars() {
        let keep = ch.is_ascii_lowercase()
            || ch.is_ascii_digit()
            || ('\u{4e00}'..='\u{9fa5}').contains(&ch);
        if keep {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        "unnamed".to_string()
    } else {
        out
    }
}

/// `plugin_origin(meta)`——cached / marketplace / managed 三者之一。
pub fn plugin_origin(rel_label: &str, is_marketplace: bool) -> &'static str {
    let low = rel_label.to_lowercase();
    if low.contains("cache") || low.contains("marketplace") || low.contains("extensions") {
        return "cached";
    }
    if is_marketplace {
        return "marketplace";
    }
    "managed"
}

/// JSON 值按 JS 真值规则（`!!v`）判定：只有 null/false/0/""/NaN 为假。
pub fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        _ => true, // arrays and objects are always truthy in JS
    }
}

/// 按 JS 的 `String(v)` 字符串强制转换规则渲染一个 JSON 值。
pub fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/* ------------------------------------------------------------------ */
/* 时间戳                                                             */
/* ------------------------------------------------------------------ */

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // Howard Hinnant 的 days-from-civil 的逆运算。本项目只需要一件事——JS 兼容的
    // ISO 字符串——为此不值得把 chrono 拉进来。
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// UTC 下的 ISO-8601：毫秒精度，`Z` 后缀。
pub fn iso_from_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        millis
    )
}

pub fn iso_from_systemtime(t: SystemTime) -> Option<String> {
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    Some(iso_from_millis(d.as_millis() as i64))
}

/* ------------------------------------------------------------------ */
/* 路径                                                               */
/* ------------------------------------------------------------------ */

/// Windows 路径用反斜杠；由 `~/.workbuddy` 这类 token 拼出的 `PathBuf`
/// 会留下零散的正斜杠。凡是交给渲染层的路径都过这里，这样字符串才对得上。
pub fn norm_str(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s.to_string()
    }
}

/// 拼接 `base` 与 `rel`（`rel` 用正斜杠），按 `norm_str` 同样的方式规范化。
pub fn join_native(base: &Path, rel: &str) -> PathBuf {
    let cleaned = if cfg!(windows) {
        rel.replace('/', "\\")
    } else {
        rel.to_string()
    };
    base.join(cleaned)
}

/// `p` 的文件名部分。
pub fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string())
}

/// `p` 的文件名部分去掉 `ext`——只在完全一致、区分大小写时才去掉，
/// 所以 `README.MD` 会保留它的扩展名。
pub fn basename_strip(p: &str, ext: &str) -> String {
    let b = basename(p);
    match b.strip_suffix(ext) {
        Some(s) => s.to_string(),
        None => b,
    }
}

/* ------------------------------------------------------------------ */
/* 资产工厂                                                           */
/* ------------------------------------------------------------------ */

pub const ASSET_KINDS: [&str; 5] = ["skill", "mcp", "plugin", "rule", "memory"];

/// 资产 id：`<kind>:<slug>:<agent>:<scope>:<project>:<hash(dir)>`。
pub fn asset_id(
    kind: &str,
    name: &str,
    agent_id: &str,
    scope: &str,
    project_id: Option<&str>,
    dir: Option<&str>,
) -> String {
    let base = [
        kind,
        &slug(name),
        agent_id,
        scope,
        project_id.unwrap_or("-"),
    ]
    .join(":");
    match dir {
        Some(d) if !d.is_empty() => format!("{}:{}", base, &hash16(&d.to_lowercase())[..6]),
        _ => base,
    }
}

/// `make_asset` 接受的全部字段，默认值已经填好。
#[derive(Default)]
pub struct NewAsset {
    pub kind: &'static str,
    pub name: String,
    pub id: Option<String>,
    pub agent_id: String,
    pub agent_label: String,
    pub scope: String,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub root: Option<String>,
    pub dir: Option<String>,
    pub file: Option<String>,
    pub files: Vec<String>,
    pub format: Option<String>,
    pub enabled: Option<bool>,
    pub size: u64,
    pub mtime: Option<String>,
    pub fingerprint: Option<String>,
    pub meta: Value,
    pub description: String,
    pub tags: Vec<Value>,
    pub shared: bool,
    pub writable: Option<bool>,
    pub origin: Option<String>,
    pub issues: Vec<Value>,
}

pub fn make_asset(f: NewAsset) -> Value {
    assert!(
        ASSET_KINDS.contains(&f.kind),
        "unknown asset kind: {}",
        f.kind
    );
    let name = if f.name.is_empty() {
        "(unnamed)".to_string()
    } else {
        f.name
    };
    let id = f.id.unwrap_or_else(|| {
        asset_id(
            f.kind,
            &name,
            &f.agent_id,
            &f.scope,
            f.project_id.as_deref(),
            f.dir.as_deref(),
        )
    });
    let fingerprint = f
        .fingerprint
        .unwrap_or_else(|| hash16(&format!("{}{}", f.file.clone().unwrap_or_default(), f.size)));
    let origin = f.origin.unwrap_or_else(|| {
        if f.kind == "plugin" {
            "managed".to_string()
        } else {
            "user".to_string()
        }
    });
    json!({
        "id": id,
        "kind": f.kind,
        "name": name,
        "agentId": f.agent_id,
        "agentLabel": if f.agent_label.is_empty() { Value::from(f.agent_id.clone()) } else { Value::from(f.agent_label) },
        "scope": f.scope,
        "projectId": f.project_id,
        "projectName": f.project_name,
        "root": f.root,
        "dir": f.dir,
        "file": f.file,
        "files": f.files,
        "format": f.format.unwrap_or_else(|| "md".to_string()),
        "enabled": f.enabled.unwrap_or(true),
        "size": f.size,
        "mtime": f.mtime,
        "fingerprint": fingerprint,
        "meta": if f.meta.is_null() { json!({}) } else { f.meta },
        "description": f.description,
        "tags": f.tags,
        "shared": f.shared,
        "source": "scan",
        "managed": false,
        "writable": f.writable.unwrap_or(true),
        "origin": origin,
        "issues": f.issues,
        "effective": Value::Null,
        "raw": Value::Null
    })
}

/// 按 UTF-16 码元切到 `n` 个，落在字符边界上。切点可能
/// 劈开代理对、留下一个孤立代理项；Rust 的 `String` 装不下这种，
/// 所以预览最多短一个字符。仅供展示的字段。
pub fn js_slice(s: &str, n: usize) -> String {
    if s.encode_utf16().count() <= n {
        return s.to_string();
    }
    let mut units = 0usize;
    let mut out = String::with_capacity(n);
    for ch in s.chars() {
        let u = ch.len_utf16();
        if units + u > n {
            break;
        }
        units += u;
        out.push(ch);
    }
    out
}

/// 解析口径：tags 是裸值时成为单元素列表，数组元素保持原类型（插件清单会带
/// `{en, zh}` 标签对象，不能被字符串化）。
pub fn tags_of(data: &Value) -> Vec<Value> {
    tags_from(data.get("tags"))
}

pub fn tags_from(value: Option<&Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(items)) => items.clone(),
        Some(v) if js_truthy(v) => vec![v.clone()],
        _ => Vec::new(),
    }
}
