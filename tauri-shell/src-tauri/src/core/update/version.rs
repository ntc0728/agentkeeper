//! Software-update pure helpers — Rust port of `core/update/version.js`.
//!
//! Version comparison, release picking, asset selection, file-name hardening
//! and host allow-listing. Nothing here performs IO, which is what makes it
//! directly testable; the IO half (`manager`, `http`, `token`) is not ported
//! yet.
//!
//! ## Why prerelease segments take part in the ordering
//!
//! The "join beta" channel puts `0.3.0-beta.1` and `0.3.0-beta.2` side by side.
//! Comparing only the three core numbers calls them equal, so a beta user would
//! never receive beta.2. Semver rule 11 says a prerelease sorts below its own
//! final release (`0.3.0-beta.2 < 0.3.0`), which makes
//! beta.1 → beta.2 → 0.3.0 a chain where every hop is an upgrade.
//!
//! ## Fidelity
//!
//! This is a migration, not a redesign. Where the JS has a quirk the quirk is
//! reproduced and marked `// JS quirk:` — changing it here would make the
//! Tauri build offer updates the Electron build does not (or vice versa).
//! Notable ones:
//!
//! * `core` components are JS `Number`s, i.e. `f64`. Two distinct integers
//!   above 2^53 therefore compare equal, and a 400-digit component parses to
//!   `Infinity` (both sides then compare equal).
//! * `1.2.3.4` and `1.2.3_beta` ignore the suffix and compare as final
//!   releases; only a `-` starts a prerelease.
//! * `pick_installer` can never return an asset on Linux or macOS: every
//!   `.AppImage` / `.deb` / `.dmg` candidate is rejected by
//!   `is_non_installer_asset`, so only the Windows path is reachable.
//! * `installer_suffix` returns `"AppImage"` (capital A) while
//!   `allowed_installer_suffixes` returns `"appimage"` (lowercase).
//! * `UpdateError` carries `status` (400/502), not a `code`; `code()` is a
//!   Rust-side stable discriminator added so callers need not parse the
//!   Chinese message text.
//!
//! ## URL parsing
//!
//! The JS uses the WHATWG `URL` parser. No URL crate is a dependency (and
//! adding one is out of scope), so `assert_downloadable` carries a small
//! parser implementing the parts the predicate depends on: scheme detection,
//! special-scheme slash/backslash skipping, userinfo, port validation,
//! percent-decoded/lowercased hostnames, and path extraction. Deviations from
//! WHATWG are deliberately conservative — IDNA and percent-encoded non-ASCII
//! hosts are rejected instead of punycoded, `href` is the preprocessed input
//! rather than a re-serialization (no default-port elision), and `pathname`
//! is not percent- or dot-segment-normalized. That means this parser may
//! reject an exotic URL the JS accepts, but never accepts one the JS rejects.

use regex::Regex;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::fmt;
use std::sync::OnceLock;

use crate::core::ecma_trim;
use crate::core::model::{js_slice, js_truthy};

/// GitHub API root (overridable per call for a self-hosted mirror).
pub const GITHUB_API: &str = "https://api.github.com";

/// Release assets are served from these hosts (and `*.githubusercontent.com`).
pub const ALLOWED_HOSTS: [&str; 4] = [
    "github.com",
    "www.github.com",
    "objects.githubusercontent.com",
    "codeload.github.com",
];

pub const ALLOWED_HOST_SUFFIX: &str = ".githubusercontent.com";

/// Refuse absurd bodies before filling a disk (same ceiling as upstream).
pub const MAX_INSTALLER_BYTES: u64 = 600 * 1024 * 1024;

/* ------------------------------------------------------------------ */
/* errors                                                              */
/* ------------------------------------------------------------------ */

/// Update failure with an HTTP-ish status, mirroring the JS `UpdateError`.
///
/// JS shape: `{ name: 'UpdateError', message, status }`, `status` defaulting
/// to 502. `code` does not exist in the JS class; it is an additive, stable
/// discriminator for Rust callers (the JS callers only ever read `.message`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateError {
    message: String,
    status: u16,
    code: &'static str,
}

impl UpdateError {
    /// `new UpdateError(message, status)`; the JS default status is 502.
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

    /// HTTP-ish status: 400 for rejected URLs, 502 for the JS default.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Stable kind discriminator (Rust-side addition; see the type docs).
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
/* version parsing + comparison                                        */
/* ------------------------------------------------------------------ */

/// `{ core: [major, minor, patch], pre: string | null }`.
///
/// The core numbers are `f64` because JS stores them as `Number` — see the
/// module docs on the >2^53 behaviour.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitVersion {
    pub core: [f64; 3],
    pub pre: Option<String>,
}

fn leading_digits(text: &str) -> usize {
    text.bytes().take_while(|b| b.is_ascii_digit()).count()
}

/// Split `v1.2.3-beta.1+build` into `{ core: [1,2,3], pre: "beta.1" }`.
///
/// Only ASCII digits count (JS `\d` without `/u` is the same thing). A missing
/// `-` suffix means "no prerelease segment": `1.2.3.4` and `1.2.3_beta` are
/// non-semver spellings that keep the old "suffix ignored" behaviour, so they
/// compare as final releases. Build metadata (`+…`) never affects order.
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
        // A digit run longer than an f64 can hold becomes `Infinity`, exactly
        // as `Number()` does.
        *slot = rest[..len].parse::<f64>().unwrap_or(f64::INFINITY);
        rest = &rest[len..];
        // Only the first two components require a '.' after them.
        if i < 2 {
            if !rest.starts_with('.') {
                return None;
            }
            rest = &rest[1..];
        }
    }

    let head = rest.split('+').next().unwrap_or("");
    // `head.length > 1` — a bare "-" is not a prerelease.
    let pre = match head.strip_prefix('-') {
        Some(tail) if !tail.is_empty() => Some(tail.to_string()),
        _ => None,
    };

    Some(SplitVersion { core, pre })
}

/// `[major, minor, patch]`, or `None` when the tag is not semantic.
pub fn parse_version(text: &str) -> Option<[f64; 3]> {
    split_version(text).map(|split| split.core)
}

/// Is this tag a prerelease (`-beta.1`)? Unknown shapes are treated as final.
pub fn is_prerelease_version(text: &str) -> bool {
    split_version(text).map(|split| split.pre.is_some()).unwrap_or(false)
}

/// JS string comparison (`a > b`) is by UTF-16 code unit, not code point: a
/// supplementary character (encoded as a surrogate pair starting D800) sorts
/// *before* a BMP character in E000..FFFF. Byte order would get that wrong.
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

/// `/^[0-9]+$/ ? Number(text) : null`
fn numeric_identifier(text: &str) -> Option<f64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(text.parse::<f64>().unwrap_or(f64::INFINITY))
}

/// Semver rule 11: numeric identifiers compare numerically (`beta.2 < beta.10`),
/// numeric sorts below alphanumeric, and a shorter prefix sorts first
/// (`1.0.0-alpha < 1.0.0-alpha.1`).
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
            // No NaN is possible here (digits only), so `==`/`>` mirrors the
            // JS `na === nb` / `na > nb` exactly, including Infinity.
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

/// `Some(Ordering)` when both sides parse, `None` when either does not.
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
        // 1.0.0 > 1.0.0-beta.1
        (None, Some(_)) => Some(Ordering::Greater),
        (Some(_), None) => Some(Ordering::Less),
        (Some(left), Some(right)) => Some(compare_prerelease(left, right)),
    }
}

/* ------------------------------------------------------------------ */
/* JS value coercion helpers                                           */
/* ------------------------------------------------------------------ */

/// `String(v)` for the JSON values a GitHub payload can contain.
fn js_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        // Array.prototype.toString === join(','), where null/undefined are "".
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

/// `String(v || '')` — JS truthiness first, then string coercion.
fn js_text_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(v) if js_truthy(v) => js_to_string(v),
        _ => String::new(),
    }
}

/// `Number(v)` for the scalar shapes `size` can take. A subset of JS number
/// parsing (no hex/octal/binary literals); JSON sizes are numbers in practice.
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

/// Serde serializes `NaN`/`Infinity` as `null` in JS `JSON.stringify`, and
/// integral numbers without a fractional part (`12`, not `12.0`).
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
/* release picking                                                     */
/* ------------------------------------------------------------------ */

/// Pick "the version to upgrade to" out of a `/releases` list.
///
/// `/releases/latest` already excludes prereleases, so the beta channel has to
/// list releases and choose itself. Rules:
///
///   * drafts never count (they show up when a token is used);
///   * tags without a parseable version do not participate;
///   * otherwise the highest **version** wins (not list order — a hotfix can be
///     published after a higher version);
///   * `only_prerelease` keeps a beta user on the beta channel: once you run
///     `0.3.0-beta.2`, the final `0.3.0` is "same core, no prerelease" and
///     would count as newer, dragging you off the channel you opted into.
///     Returning `None` there is deliberate.
pub fn pick_listed_release<'a>(list: &'a Value, only_prerelease: bool) -> Option<&'a Value> {
    let items = list.as_array()?;
    let mut best: Option<&'a Value> = None;
    let mut first: Option<&'a Value> = None;

    for item in items {
        // `!item || typeof item !== 'object'`: only objects (and arrays, which
        // JS also reports as 'object') participate.
        if !matches!(item, Value::Object(_) | Value::Array(_)) {
            continue;
        }
        // `item.draft === true` — a truthy string is not a draft. Drafts are
        // skipped *before* `first` is recorded: they must never be the
        // final-channel fallback either.
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

    // "Nothing parseable at all" falls back to the newest entry — but only for
    // the final channel; the beta channel means "no beta → no update".
    if only_prerelease {
        best
    } else {
        best.or(first)
    }
}

/* ------------------------------------------------------------------ */
/* URLs, hosts, assets                                                 */
/* ------------------------------------------------------------------ */

/// The subset of a WHATWG `URL` this module needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    /// Input after WHATWG whitespace preprocessing. Not re-serialized.
    pub href: String,
    /// Includes the colon: `"https:"`.
    pub protocol: String,
    /// Lowercased, percent-decoded, without port or userinfo.
    pub hostname: String,
    /// Path only, at least `"/"` for special schemes.
    pub pathname: String,
}

/// WHATWG preprocessing: strip leading/trailing C0 controls and spaces, then
/// remove every tab/newline.
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
        && (bytes[i].is_ascii_alphanumeric()
            || matches!(bytes[i], b'+' | b'-' | b'.'))
    {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b':' {
        return None;
    }
    Some((format!("{}:", text[..i].to_ascii_lowercase()), &text[i + 1..]))
}

/// Outer `None` = invalid port; `Some(None)` = omitted/empty port.
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

/// Percent-decode, lowercase and validate a reg-name / IPv4 host.
///
/// Deviation: non-ASCII hosts are rejected (WHATWG would IDNA-encode them);
/// no allow-listed host is non-ASCII, so only an exotic `apiBase` is affected.
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

/// Split the post-userinfo authority into host and optional port.
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
        // WHATWG keeps the brackets in `hostname`.
        return Some((format!("[{}]", inner.to_ascii_lowercase()), port));
    }

    let (host_raw, port) = match authority.rfind(':') {
        Some(i) => (&authority[..i], parse_port(&authority[i + 1..])?),
        None => (authority, None),
    };
    Some((decode_host(host_raw)?, port))
}

fn split_path(tail: &str, special: bool) -> String {
    let end = tail
        .find(|c| c == '?' || c == '#')
        .unwrap_or(tail.len());
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

/// Handle a scheme's authority and path. `special` selects the WHATWG rules
/// where `\` is a path separator and the default path is `/`.
fn parse_authority(
    rest: &str,
    special: bool,
    allow_empty_host: bool,
) -> Option<(String, String)> {
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

    // `file:` is special in WHATWG but has its own empty-host rules; it can
    // never pass the https check, so it is parsed as an ordinary scheme here.
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

/// Reject anything but https on a GitHub asset host.
///
/// The download path is a direct "execute this file" boundary upstream of the
/// installer, so a renderer-supplied URL is not trusted: a compromised page
/// must not be able to point the updater at `http://evil/x.exe`. A mirror whose
/// host is configured explicitly (`apiBase`) is allowed to use whatever scheme
/// it was configured with — that is a deliberate, local decision.
///
/// `api_base == ""` is the JS default (`{ apiBase = '' }`). An unparseable
/// `apiBase` is silently ignored; see the comment on the branch below.
pub fn assert_downloadable(raw_url: &str, api_base: &str) -> Result<ParsedUrl, UpdateError> {
    let url = parse_url(raw_url).ok_or_else(UpdateError::invalid_url)?;
    let host = url.hostname.to_lowercase();

    // JS quirk: the `catch` around `new URL(apiBase)` rethrows only
    // `UpdateError`, so a malformed apiBase is swallowed and the default
    // GitHub checks run instead.
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
/* installers                                                          */
/* ------------------------------------------------------------------ */

/// Primary installer extension for a platform (no dot).
///
/// `platform` is the Node value (`"darwin"` / `"win32"`); anything else,
/// including the empty string that stands in for an undefined argument, takes
/// the Linux branch.
pub fn installer_suffix(platform: &str) -> &'static str {
    if platform == "darwin" {
        "dmg"
    } else if platform == "win32" {
        "exe"
    } else {
        "AppImage"
    }
}

/// Coarse kind, used by the UI for wording (UAC / mount / package manager).
pub fn installer_kind(platform: &str) -> &'static str {
    if platform == "darwin" {
        "dmg"
    } else if platform == "win32" {
        "nsis"
    } else {
        "appimage"
    }
}

/// Extensions this platform accepts; the shell-side run check uses the same
/// list. Note the lowercase spelling, unlike [`installer_suffix`].
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
        // `unicode(false)` is not cosmetic: JS without `/u` folds ASCII only
        // and treats `\b` as an ASCII word boundary, so `中portable.exe` DOES
        // match `-?portable\b` (中 is a non-word char to JS).
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

/// Assets that must never be treated as "the installer".
///
/// A **portable** build installs nothing (no shortcut, no uninstall entry) and
/// leaves a second copy the updater will never upgrade; archives / AppImage /
/// deb / dmg belong to other platforms.
pub fn is_non_installer_asset(name: &str) -> bool {
    let patterns = asset_patterns();
    patterns.portable.is_match(name)
        || patterns.archive.is_match(name)
        || patterns.foreign_exe.is_match(name)
}

fn asset_name(item: &Value) -> String {
    // `String((item && item.name) || '')` — non-objects have no `name`.
    js_text_or_empty(item.get("name"))
}

/// Architecture keywords, most specific first. A release built for another
/// architecture installs but fails to start, so prefer an exact match.
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

/// Choose the installer asset for this platform/arch, shaped
/// `{ name, size, url }` like the JS return value.
///
/// Windows: NSIS artifacts carry `-setup`, and a bare exe is sometimes attached
/// as well — that one creates no shortcut and no uninstall entry, so `setup`
/// must win. When the release has neither (a custom `artifactName`), the first
/// remaining candidate is still better than a hidden button.
///
/// `// JS quirk:` Linux and macOS never return anything. `allowedInstallerSuffixes`
/// admits `.appimage` / `.deb` / `.dmg`, but `isNonInstallerAsset` rejects every
/// name with those extensions, so `candidates` is always empty there.
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

/// Basename only, `\/:*?"<>|` folded to `_`, trimmed. The fallback always keeps
/// the platform suffix — downstream verification checks the extension, so an
/// extension-less fallback would turn "odd asset name" into "cannot install".
pub fn safe_file_name(name: &str, platform: &str) -> String {
    let base = name
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or("");
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

/// Optional arguments of [`release_from_api`]; absent values are `""`, which
/// is what JS `undefined` falls through to for every comparison here.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReleaseOptions<'a> {
    pub platform: &'a str,
    pub arch: &'a str,
    pub api_base: &'a str,
    pub repo: &'a str,
}

/// GitHub Release → the flat shape the manager caches and the UI renders.
/// `notes` is capped: a 200 KB changelog should never travel through IPC.
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
    // `payload && payload.prerelease === true` — a falsy payload is returned
    // as-is by `&&`, so the field is not always a bool. Preserved.
    let prerelease = match payload {
        Value::Object(_) => Value::Bool(matches!(
            payload.get("prerelease"),
            Some(Value::Bool(true))
        )),
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
        // `String(body).slice(0, 8000)`; see `js_slice` on surrogate pairs.
        "notes": js_slice(&text("body"), 8000),
        "publishedAt": published_at,
        "pageUrl": page,
        "prerelease": prerelease,
        "asset": asset,
        "apiBase": options.api_base,
    })
}

/* ------------------------------------------------------------------ */
/* tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_version_matches_js_shapes() {
        let split = split_version("v1.2.3-beta.1+build").unwrap();
        assert_eq!(split.core, [1.0, 2.0, 3.0]);
        assert_eq!(split.pre.as_deref(), Some("beta.1"));

        assert_eq!(split_version(" 1.2.3 ").unwrap().pre, None);
        assert_eq!(split_version("V1.2.3").unwrap().core, [1.0, 2.0, 3.0]);
        // Non-semver spellings keep the "suffix ignored" behaviour.
        assert_eq!(split_version("1.2.3.4").unwrap().core, [1.0, 2.0, 3.0]);
        assert_eq!(split_version("1.2.3_beta").unwrap().pre, None);
        assert_eq!(split_version("1.2.3+build").unwrap().pre, None);
        assert_eq!(split_version("1.2.3-beta+build").unwrap().pre.as_deref(), Some("beta"));
        // A bare "-" is not a prerelease; "--beta" strips exactly one.
        assert_eq!(split_version("1.2.3-").unwrap().pre, None);
        assert_eq!(split_version("1.2.3--beta").unwrap().pre.as_deref(), Some("-beta"));
        // Leading zeros are numbers, not strings.
        assert_eq!(split_version("01.002.0003").unwrap().core, [1.0, 2.0, 3.0]);

        // JS trim strips the BOM but not U+0085.
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
        // Numeric identifiers sort below alphanumeric ones.
        assert_eq!(compare_prerelease("0", "a"), Ordering::Less);
        assert_eq!(compare_prerelease("a", "0"), Ordering::Greater);
        // Alphanumeric comparison is ASCII (uppercase before lowercase).
        assert_eq!(compare_prerelease("beta", "BETA"), Ordering::Greater);
        // JS quirk: both are numbers, so leading zeros are irrelevant.
        assert_eq!(compare_prerelease("01", "1"), Ordering::Equal);
        // An extra empty identifier makes the left side longer, hence greater.
        assert_eq!(compare_prerelease("a.", "a"), Ordering::Greater);
        assert_eq!(compare_prerelease("", ""), Ordering::Equal);
        // UTF-16 code-unit order: 😀 starts with a surrogate, so it sorts lower.
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
            assert_eq!(
                compare_versions(pair[1], pair[0]),
                Some(Ordering::Greater)
            );
        }

        assert_eq!(compare_versions("1.2.3", "1.2.3+build"), Some(Ordering::Equal));
        assert_eq!(compare_versions("1.2.3.4", "1.2.3"), Some(Ordering::Equal));
        assert_eq!(compare_versions("1.2.3_beta", "1.2.3"), Some(Ordering::Equal));
        assert_eq!(compare_versions("bad", "1.0.0"), None);
        assert_eq!(compare_versions("1.0.0", "also bad"), None);
        // JS Number rounds above 2^53: two different huge versions are equal.
        assert_eq!(
            compare_versions(
                "999999999999999999999999.0.0",
                "999999999999999999999998.0.0"
            ),
            Some(Ordering::Equal)
        );
        // A digit run longer than f64 can hold is Infinity on both sides.
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
        // `draft === true` — a truthy string does not hide a release.
        let list = json!([
            { "tag_name": "v1.0.0", "draft": "true" },
            { "tag_name": "v2.0.0", "draft": true }
        ]);
        assert_eq!(
            pick_listed_release(&list, false).unwrap()["tag_name"],
            "v1.0.0"
        );

        // A true-draft is skipped before `first` is recorded, so it is not
        // even the "nothing parseable" fallback.
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

        // The final channel still falls back to the first entry...
        let unparseable = json!([{ "tag_name": "nightly" }, { "tag_name": "v1" }]);
        assert_eq!(
            pick_listed_release(&unparseable, false).unwrap()["tag_name"],
            "nightly"
        );
        // ...even when that entry is an array; primitives are skipped first.
        let odd = json!([null, 5, [{ "x": 1 }], { "tag_name": "nightly" }]);
        assert!(pick_listed_release(&odd, false).unwrap().is_array());
        // A non-list is not a crash.
        assert!(pick_listed_release(&json!({ "not": "a list" }), false).is_none());
        // `tag_name` is string-coerced: an array can parse.
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

        // Percent-decoded host and backslash-as-slash are accepted by WHATWG.
        assert_eq!(
            assert_downloadable("https://github%2Ecom/x", "").unwrap().hostname,
            "github.com"
        );
        let url = assert_downloadable("https://github.com\\@evil.com/x", "").unwrap();
        assert_eq!(url.hostname, "github.com");
        assert_eq!(url.pathname, "/@evil.com/x");

        // Missing slashes still parse; an empty path becomes "/".
        assert_eq!(
            assert_downloadable("https:github.com", "").unwrap().pathname,
            "/"
        );
        assert_eq!(
            assert_downloadable("https://github.com?x=1", "").unwrap().pathname,
            "/"
        );
        // Userinfo cannot smuggle a different host in.
        assert!(assert_downloadable("https://github.com@evil.com/x", "").is_err());
        // A parsable non-https scheme reaches the scheme check, not the parse error.
        assert_eq!(
            assert_downloadable("javascript:alert(1)", "").unwrap_err().code(),
            "https-required"
        );
    }

    #[test]
    fn api_base_mirror_rules() {
        // An explicitly configured mirror host (and its subdomains) bypasses
        // the GitHub allow-list.
        assert!(assert_downloadable("https://mirror.internal/a", "https://mirror.internal").is_ok());
        assert!(assert_downloadable("https://sub.mirror.internal/a", "https://mirror.internal").is_ok());
        assert!(assert_downloadable("https://notmirror.internal/a", "https://mirror.internal").is_err());
        // An https base forces https downloads...
        let err = assert_downloadable("http://mirror.internal/a", "https://mirror.internal").unwrap_err();
        assert_eq!(err.code(), "https-required");
        // ...an http base deliberately allows http.
        assert!(assert_downloadable("http://mirror.internal/a", "http://mirror.internal").is_ok());
        // Unparseable apiBase is silently ignored (JS quirk).
        assert!(assert_downloadable("https://github.com/a", "not a url").is_ok());
    }

    #[test]
    fn platform_suffixes_and_kinds() {
        assert_eq!(installer_suffix("darwin"), "dmg");
        assert_eq!(installer_suffix("win32"), "exe");
        // Capital A here, lowercase in the suffix list below — copied as-is.
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
        // JS `\b` is ASCII-only: 中 is a non-word char, so a boundary exists.
        assert!(is_non_installer_asset("中portable.exe"));
        // The archive rule requires a dot before the extension.
        assert!(!is_non_installer_asset("APPIMAGE"));
    }

    #[test]
    fn installer_selection_and_js_platform_bug() {
        let assets = json!([
            { "name": "AgentKeeper-0.2.0-portable.exe", "size": 9, "browser_download_url": "https://github.com/p" },
            { "name": "AgentKeeper-0.2.0-setup.exe", "size": 12, "browser_download_url": "https://github.com/s" },
            { "name": "AgentKeeper-0.2.0-x64.exe", "size": 1, "browser_download_url": "https://github.com/x" }
        ]);
        let picked = pick_installer(&assets, "win32", "x64").unwrap();
        assert_eq!(picked["name"], "AgentKeeper-0.2.0-setup.exe");
        assert_eq!(picked["size"], 12);
        assert_eq!(picked["url"], "https://github.com/s");

        // No setup/arch marker → first candidate; gaps become 0 / "".
        let plain = json!([{ "name": "whatever.exe", "size": null }]);
        let picked = pick_installer(&plain, "win32", "x64").unwrap();
        assert_eq!(picked["size"], 0);
        assert_eq!(picked["url"], "");

        // Arch preference: x64 wins over an earlier amd64 without `-setup`.
        let arch = json!([
            { "name": "AgentKeeper-0.2.0-amd64.exe", "size": 1, "browser_download_url": "https://github.com/a" },
            { "name": "AgentKeeper-0.2.0-x64.exe", "size": 2, "browser_download_url": "https://github.com/z" }
        ]);
        assert_eq!(
            pick_installer(&arch, "win32", "x64").unwrap()["name"],
            "AgentKeeper-0.2.0-x64.exe"
        );

        // JS quirk: Linux and macOS filter out every candidate, always.
        let linux = json!([{ "name": "AgentKeeper.AppImage", "size": 1 }]);
        assert!(pick_installer(&linux, "linux", "x64").is_none());
        let deb = json!([{ "name": "AgentKeeper.deb", "size": 1 }]);
        assert!(pick_installer(&deb, "linux", "x64").is_none());
        let mac = json!([{ "name": "AgentKeeper.dmg", "size": 1 }]);
        assert!(pick_installer(&mac, "darwin", "arm64").is_none());

        // Not an array → no candidates.
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
        assert_eq!(safe_file_name("   ", "linux"), "AgentKeeper-update.AppImage");
        assert_eq!(safe_file_name("../../x.exe", "darwin"), "x.exe");
    }

    #[test]
    fn release_shape_matches_js() {
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

        // No repo → the generic releases page; missing payload is tolerated.
        let empty = release_from_api(&Value::Null, ReleaseOptions::default());
        assert_eq!(empty["pageUrl"], "https://github.com");
        assert_eq!(empty["asset"], Value::Null);
        // `payload && payload.prerelease === true` returns the falsy payload.
        assert_eq!(empty["prerelease"], Value::Null);
    }

    #[test]
    fn update_error_carries_status_and_code() {
        let err = UpdateError::new("boom", 400);
        assert_eq!(err.status(), 400);
        assert_eq!(err.code(), "update-error");
        assert_eq!(err.message(), "boom");
        assert_eq!(err.to_string(), "boom");
        // The JS constructor defaults to 502 when the caller omits a status.
        assert_eq!(UpdateError::new("boom", 502).status(), 502);
    }

    #[test]
    fn constants_match_the_js() {
        assert_eq!(GITHUB_API, "https://api.github.com");
        assert_eq!(ALLOWED_HOST_SUFFIX, ".githubusercontent.com");
        assert_eq!(MAX_INSTALLER_BYTES, 600 * 1024 * 1024);
        assert!(ALLOWED_HOSTS.contains(&"objects.githubusercontent.com"));
    }
}
