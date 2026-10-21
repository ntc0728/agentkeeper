//! Asset model — a field-for-field port of `core/model.js`.
//!
//! The output is built as `serde_json::Value` rather than a typed struct on
//! purpose: the renderer treats every field as optional, and a typed struct
//! would make it far too easy to silently *drop* a field the UI reads (the
//! stage-1 stub already caused one crash by omitting `summary.byOrigin`).
//! Explicit `null`s keep the shape identical to what `ipc.js` sends.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// `crypto.sha256(str).slice(0, 16)` — the identity primitive used for
/// fingerprints, ids and project slugs.
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

/// `slug(s)` — lowercase, everything outside `[a-z0-9一-龥]` collapsed to a
/// single dash, edges trimmed, falling back to "unnamed".
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

/// `pluginOrigin(meta)` — cached / marketplace / managed.
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

/// JS truthiness of a JSON value (`!!v`): only null/false/0/""/NaN are falsy.
pub fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        _ => true, // arrays and objects are always truthy in JS
    }
}

/// Render a JSON value the way JS string coercion would in `String(v)`.
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
/* timestamps                                                          */
/* ------------------------------------------------------------------ */

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // Howard Hinnant's days-from-civil, inverted. Avoids pulling in chrono
    // for the single thing this project needs: JS-compatible ISO strings.
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

/// `new Date(ms).toISOString()` — UTC, millisecond precision, `Z` suffix.
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
/* paths                                                               */
/* ------------------------------------------------------------------ */

/// Node's `path.join` output on Windows uses backslashes; a `PathBuf` built
/// from a token like `~/.workbuddy` keeps stray forward slashes. Every path
/// that reaches the renderer goes through here so the strings match.
pub fn norm_str(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s.to_string()
    }
}

/// `path.join(base, rel)` where `rel` uses forward slashes, normalised the
/// same way `norm_str` does.
pub fn join_native(base: &Path, rel: &str) -> PathBuf {
    let cleaned = if cfg!(windows) { rel.replace('/', "\\") } else { rel.to_string() };
    base.join(cleaned)
}

/// `path.basename(p)`
pub fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string())
}

/// `path.basename(p, ext)` — strips `ext` only on an exact, case-sensitive
/// match (JS semantics; `README.MD` keeps its extension).
pub fn basename_strip(p: &str, ext: &str) -> String {
    let b = basename(p);
    match b.strip_suffix(ext) {
        Some(s) => s.to_string(),
        None => b,
    }
}

/* ------------------------------------------------------------------ */
/* asset factory                                                       */
/* ------------------------------------------------------------------ */

pub const ASSET_KINDS: [&str; 5] = ["skill", "mcp", "plugin", "rule", "memory"];

/// Mirrors `assetId()`: `<kind>:<slug>:<agent>:<scope>:<project>:<hash(dir)>`.
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

/// Everything `makeAsset` accepts, with JS defaults applied by `make_asset`.
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
    assert!(ASSET_KINDS.contains(&f.kind), "unknown asset kind: {}", f.kind);
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
    let fingerprint = f.fingerprint.unwrap_or_else(|| {
        hash16(&format!(
            "{}{}",
            f.file.clone().unwrap_or_default(),
            f.size
        ))
    });
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

/// `String.prototype.slice(0, n)` in UTF-16 code units, on a character
/// boundary. JS can cut a surrogate pair in half and produce a lone
/// surrogate; a Rust `String` cannot hold one, so the preview may be at most
/// one character shorter. Display-only field.
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

/// `[].concat(data.tags || [])` — a bare value becomes a one-element list and
/// array members keep their original type (plugin manifests ship
/// `{en, zh}` tag objects, which must not be stringified).
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
