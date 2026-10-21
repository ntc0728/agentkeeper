//! Write-path validators — Rust port of `core/validate.js`.
//!
//! Every write goes through here first; issues are returned rather than thrown
//! so the UI can block the write instead of producing a file the agent will
//! silently ignore (or crash on).
//!
//! Parity notes:
//! * issue objects are built with keys in `level`, `field`, `message` order,
//!   because the renderer renders them positionally in one place;
//! * issue *order* is part of the contract — the UI lists them as returned;
//! * string lengths the JS side compares with `.length` are UTF-16 code units,
//!   not bytes and not chars.

use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

use super::model::{js_string, js_truthy};
use super::ecma_trim;

/// `RULES.reservedSkillNames`
const RESERVED_SKILL_NAMES: [&str; 9] = [
    ".", "..", "con", "prn", "aux", "nul", "com1", "com2", "lpt1",
];

/// `RULES.mcpServerName`'s allow-list of extra config keys.
const KNOWN_MCP_KEYS: [&str; 10] = [
    "command", "args", "env", "url", "type", "headers", "timeout", "disabled", "cwd", "transport",
];

fn skill_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,63}$").expect("skill name regex"))
}

fn mcp_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$").expect("mcp name regex"))
}

/// `/^\d+(\.\d+)*/` — deliberately NOT anchored at the end, so `1.0.0-beta`
/// passes. `\d` is written as `[0-9]` because JS `\d` is ASCII-only while the
/// Rust `regex` crate's `\d` is Unicode-aware by default.
fn version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9]+(\.[0-9]+)*").expect("version regex"))
}

/// `/^https?:\/\//i`. `-u` keeps the case folding ASCII-only like a non-`u`
/// JS regex (otherwise e.g. `ſ` would fold to `s`).
fn http_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i-u)^https?://").expect("http url regex"))
}

fn issue(level: &str, field: &str, message: impl Into<String>) -> Value {
    json!({"level": level, "field": field, "message": message.into()})
}

/* ------------------------------------------------------------------ */
/* JS coercion helpers                                                 */
/* ------------------------------------------------------------------ */

/// `String(v)`. `model::js_string` matches JS for scalars but prints JSON for
/// arrays/objects, so the container cases are handled here: JS gives
/// `[].join(',')` for arrays and `"[object Object]"` for plain objects.
fn js_str(v: &Value) -> String {
    match v {
        Value::Array(items) => items
            .iter()
            .map(|item| {
                // `[null].join(',')` is "" — null/undefined holes stringify empty.
                if item.is_null() {
                    String::new()
                } else {
                    js_str(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
        Value::Number(n) => js_number(n),
        _ => js_string(v),
    }
}

/// `String(1.0)` is `"1"` in JS, while serde renders the JSON float `1.0`.
fn js_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{}", f as i64)
    } else {
        n.to_string()
    }
}

/// JS `.length` for the fields the validators measure.
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `data[key] || ''` — JS `||` catches `0`, `false`, `""`, `null` as well.
fn prop_str(obj: &Value, key: &str) -> String {
    match obj.get(key) {
        Some(v) if js_truthy(v) => js_str(v),
        _ => String::new(),
    }
}

/// `Object.keys(v)` for the values that can reach it: objects keep insertion
/// order (serde_json `preserve_order`), and a JSON array's enumerable keys are
/// its indices.
fn object_keys(v: &Value) -> Vec<String> {
    match v {
        Value::Object(map) => map.keys().cloned().collect(),
        Value::Array(items) => (0..items.len()).map(|i| i.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// `data.version && !/^\d+(\.\d+)*/.test(String(data.version))`
fn check_version(data: &Value, issues: &mut Vec<Value>) {
    if let Some(v) = data.get("version").filter(|v| js_truthy(*v)) {
        if !version_re().is_match(&js_str(v)) {
            issues.push(issue("warn", "version", "version should look like 1.0.0"));
        }
    }
}

/* ------------------------------------------------------------------ */
/* validators                                                          */
/* ------------------------------------------------------------------ */

/// Port of `validateSkill(data, opts)`.
///
/// `body` is `opts.body`: `None` models `undefined`, `Some(Value::Null)`
/// models an explicit JSON `null` — and `String(null).trim()` is `"null"`, so
/// the latter does NOT trip the empty-body error.
pub fn validate_skill(
    data: &Value,
    body: Option<&Value>,
    dir_name: Option<&Value>,
) -> Vec<Value> {
    let mut issues = Vec::new();

    // JS quirk: `data.name || ''` means the number 0 and `false` silently
    // become "", i.e. "name is required" rather than a type error.
    let raw_name = prop_str(data, "name");
    let name = ecma_trim(&raw_name).to_string();

    if name.is_empty() {
        issues.push(issue("error", "name", "name is required"));
    } else if !skill_name_re().is_match(&name) {
        issues.push(issue(
            "error",
            "name",
            "name must be lowercase letters/digits/hyphen, max 64 chars",
        ));
    }
    // Checked on the trimmed name, lowercased — independently of the branch
    // above, so `CON` yields both the casing error and the reserved-name error.
    if RESERVED_SKILL_NAMES.contains(&name.to_lowercase().as_str()) {
        issues.push(issue(
            "error",
            "name",
            format!("\"{}\" is a reserved name on Windows", name),
        ));
    }
    if let Some(dir) = dir_name {
        let dir_str = js_str(dir);
        if js_truthy(dir) && dir_str != name {
            issues.push(issue(
                "warn",
                "name",
                format!(
                    "directory is \"{}\" but name is \"{}\" — some agents key the cache by directory",
                    dir_str, name
                ),
            ));
        }
    }

    let raw_desc = prop_str(data, "description");
    let desc = ecma_trim(&raw_desc);
    if desc.is_empty() {
        issues.push(issue(
            "error",
            "description",
            "description is required — it is how the agent decides to load the skill",
        ));
    } else if utf16_len(desc) > 1024 {
        issues.push(issue(
            "warn",
            "description",
            "description over 1024 chars may be truncated by some agents",
        ));
    }

    // JS quirk: no truthiness gate here — only `undefined` skips the check.
    // `Some(Null)` stringifies to "null" and passes.
    if let Some(b) = body {
        if ecma_trim(&js_str(b)).is_empty() {
            issues.push(issue("error", "body", "SKILL.md body is empty"));
        }
    }

    check_version(data, &mut issues);

    if let Some(tools) = data.get("allowed-tools") {
        if !tools.is_null() && !tools.is_string() && !tools.is_array() {
            issues.push(issue(
                "error",
                "allowed-tools",
                "allowed-tools must be a string or array",
            ));
        }
    }

    if let Some(compat) = data.get("compatibility").and_then(|v| v.as_str()) {
        if utf16_len(compat) > 500 {
            issues.push(issue("warn", "compatibility", "compatibility string is very long"));
        }
    }

    issues
}

/// Port of `validateMcpServer(id, cfg)`.
pub fn validate_mcp_server(id: &str, cfg: &Value) -> Vec<Value> {
    let mut issues = Vec::new();

    if !mcp_name_re().is_match(id) {
        issues.push(issue(
            "error",
            "name",
            "MCP server name must match /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/",
        ));
    }

    // `!cfg || typeof cfg !== 'object'`. JSON arrays are `typeof 'object'` in
    // JS, so an array does NOT take this early return.
    if !js_truthy(cfg) || !(cfg.is_object() || cfg.is_array()) {
        issues.push(issue("error", "config", "server config must be an object"));
        return issues;
    }

    let command = cfg.get("command").and_then(|v| v.as_str());
    let has_stdio = command.map(|s| !s.is_empty()).unwrap_or(false);
    let has_http = cfg
        .get("url")
        .and_then(|v| v.as_str())
        .map(|s| http_re().is_match(s))
        .unwrap_or(false);
    if !has_stdio && !has_http {
        issues.push(issue(
            "error",
            "config",
            "server needs either `command` (stdio) or `url` (http/sse)",
        ));
    }

    if let Some(args) = cfg.get("args") {
        if !args.is_array() {
            issues.push(issue("error", "args", "args must be an array of strings"));
        }
    }

    // JS quirk: `typeof null === 'object'` and `Array.isArray(null)` is false,
    // so `env: null` is accepted while `env: []` is rejected.
    if let Some(env) = cfg.get("env") {
        if !env.is_object() && !env.is_null() {
            issues.push(issue("error", "env", "env must be an object"));
        }
    }

    // `Object.keys(cfg)` in insertion order. For an array this yields the
    // indices, so a non-empty array config also produces one warning per item
    // (`field` is the raw key, not "config").
    for key in object_keys(cfg) {
        if !KNOWN_MCP_KEYS.contains(&key.as_str()) {
            issues.push(issue(
                "warn",
                &key,
                format!("unrecognized key \"{}\" — agent may ignore it", key),
            ));
        }
    }

    if has_stdio && command == Some("npx") && !has_http {
        // `Array.isArray(cfg.args) ? cfg.args.join(' ') : ''`
        let joined = match cfg.get("args") {
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    if item.is_null() {
                        String::new()
                    } else {
                        js_str(item)
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        if !joined.contains("-y") {
            issues.push(issue(
                "warn",
                "args",
                "npx without -y can block on an interactive install prompt",
            ));
        }
    }

    issues
}

/// Port of `validatePlugin(data)`.
pub fn validate_plugin(data: &Value) -> Vec<Value> {
    let mut issues = Vec::new();

    // Plain truthiness, no trim and no string coercion.
    if !data.get("name").map(js_truthy).unwrap_or(false) {
        issues.push(issue("error", "name", "plugin name is required"));
    }

    check_version(data, &mut issues);

    if let Some(desc) = data.get("description").filter(|v| js_truthy(*v)) {
        if utf16_len(&js_str(desc)) > 200 {
            issues.push(issue(
                "warn",
                "description",
                "plugin description longer than 200 chars",
            ));
        }
    }

    issues
}

/// Port of `validateRule(data, body)`. `body` is already `String(body || '')`
/// as far as the JS is concerned; the caller does that coercion.
pub fn validate_rule(body: &str) -> Vec<Value> {
    let mut issues = Vec::new();

    if ecma_trim(body).is_empty() {
        issues.push(issue(
            "error",
            "body",
            "rule file is empty — an empty rule file adds context cost with no behaviour",
        ));
    }
    if utf16_len(body) > 200_000 {
        issues.push(issue(
            "warn",
            "body",
            "rule file is over 200k chars and will consume a large share of the context window",
        ));
    }

    issues
}

/// Port of `validateMemory(data, body)`.
pub fn validate_memory(body: &str) -> Vec<Value> {
    let mut issues = Vec::new();

    if ecma_trim(body).is_empty() {
        issues.push(issue("error", "body", "memory entry is empty"));
    }

    issues
}

/// `String(payload.body || '')` — the coercion `validateRule` / `validateMemory`
/// rely on in JS.
fn body_str(payload: &Value) -> String {
    prop_str(payload, "body")
}

/// Port of `validateByKind(kind, payload)`.
pub fn validate_by_kind(kind: &str, payload: &Value) -> Vec<Value> {
    match kind {
        "skill" => {
            let empty = json!({});
            let data = payload
                .get("data")
                .filter(|v| js_truthy(*v))
                .unwrap_or(&empty);
            validate_skill(data, payload.get("body"), payload.get("dirName"))
        }
        "mcp" => {
            let name = payload
                .get("name")
                .map(js_str)
                .unwrap_or_else(|| "undefined".to_string());
            let missing = Value::Null;
            let cfg = payload.get("config").unwrap_or(&missing);
            validate_mcp_server(&name, cfg)
        }
        "plugin" => {
            // `payload.data || payload` — falls back to the whole payload.
            let fallback;
            let data = match payload.get("data").filter(|v| js_truthy(*v)) {
                Some(data) => data,
                None => {
                    fallback = payload.clone();
                    &fallback
                }
            };
            validate_plugin(data)
        }
        "rule" => validate_rule(&body_str(payload)),
        "memory" => validate_memory(&body_str(payload)),
        _ => vec![issue("error", "kind", format!("unknown kind {}", kind))],
    }
}

/// `isBlocking(issues)` — any `level === 'error'`.
pub fn is_blocking(issues: &[Value]) -> bool {
    issues
        .iter()
        .any(|i| i.get("level").and_then(|v| v.as_str()) == Some("error"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(issues: &[Value]) -> Vec<(String, String)> {
        issues
            .iter()
            .map(|i| {
                (
                    i["level"].as_str().unwrap_or("").to_string(),
                    i["field"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn valid_skill_has_no_issues() {
        let issues = validate_skill(
            &json!({
                "name": "pdf-tools",
                "description": "Extract text",
                "version": "1.0.0",
                "allowed-tools": ["Read", "Bash"],
                "compatibility": "claude"
            }),
            None,
            None,
        );
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn empty_and_bad_names() {
        let issues = validate_skill(&json!({"name": "   ", "description": "d"}), None, None);
        assert_eq!(
            issues,
            vec![json!({"level":"error","field":"name","message":"name is required"})]
        );

        let issues = validate_skill(&json!({"name": "My-Skill", "description": "d"}), None, None);
        assert_eq!(
            issues,
            vec![json!({"level":"error","field":"name","message":"name must be lowercase letters/digits/hyphen, max 64 chars"})]
        );

        // JS `||` turns 0 into '' — "required", not a type complaint.
        let issues = validate_skill(&json!({"name": 0, "description": "d"}), None, None);
        assert_eq!(issues[0]["message"], "name is required");
    }

    #[test]
    fn reserved_names_use_trimmed_lowercase() {
        let issues = validate_skill(&json!({"name": "  CON  ", "description": "d"}), None, None);
        assert_eq!(
            fields(&issues),
            vec![
                ("error".to_string(), "name".to_string()),
                ("error".to_string(), "name".to_string())
            ]
        );
        assert_eq!(
            issues[1],
            json!({"level":"error","field":"name","message":"\"CON\" is a reserved name on Windows"})
        );

        let issues = validate_skill(&json!({"name": "con", "description": "d"}), None, None);
        assert_eq!(issues.len(), 1);
        assert_eq!(
            issues[0]["message"],
            "\"con\" is a reserved name on Windows"
        );
    }

    #[test]
    fn description_length_is_utf16_units() {
        // 512 chars, 1024 UTF-16 units -> allowed.
        let ok: String = "😀".repeat(512);
        assert!(validate_skill(&json!({"name": "a", "description": ok}), None, None).is_empty());

        // 513 chars, 1026 UTF-16 units -> warn. A char count would say 513.
        let long: String = "😀".repeat(513);
        let issues = validate_skill(&json!({"name": "a", "description": long}), None, None);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["level"], "warn");
        assert_eq!(issues[0]["field"], "description");

        // 342 CJK chars: 1026 bytes, 342 UTF-16 units -> allowed.
        let cjk: String = "一".repeat(342);
        assert!(validate_skill(&json!({"name": "a", "description": cjk}), None, None).is_empty());
    }

    #[test]
    fn allowed_tools_type_check() {
        assert!(validate_skill(
            &json!({"name":"a","description":"d","allowed-tools":"Read"}),
            None,
            None
        )
        .is_empty());
        assert!(validate_skill(
            &json!({"name":"a","description":"d","allowed-tools":["Read"]}),
            None,
            None
        )
        .is_empty());
        assert!(validate_skill(
            &json!({"name":"a","description":"d","allowed-tools":null}),
            None,
            None
        )
        .is_empty());

        let issues = validate_skill(
            &json!({"name":"a","description":"d","allowed-tools":7}),
            None,
            None,
        );
        assert_eq!(
            issues,
            vec![json!({"level":"error","field":"allowed-tools","message":"allowed-tools must be a string or array"})]
        );
    }

    #[test]
    fn version_is_an_unanchored_prefix_match() {
        assert!(
            validate_skill(
                &json!({"name":"a","description":"d","version":"1.0.0-beta"}),
                None,
                None
            )
            .is_empty()
        );
        assert!(
            validate_skill(&json!({"name":"a","description":"d","version":"1"}), None, None)
                .is_empty()
        );
        let issues =
            validate_skill(&json!({"name":"a","description":"d","version":"v1"}), None, None);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["field"], "version");
        // 0 is falsy -> the whole check is skipped.
        assert!(
            validate_skill(&json!({"name":"a","description":"d","version":0}), None, None).is_empty()
        );
    }

    #[test]
    fn body_undefined_vs_null() {
        assert!(validate_skill(&json!({"name":"a","description":"d"}), None, None).is_empty());
        // String(null) === "null" -> non-empty, so no error.
        assert!(
            validate_skill(&json!({"name":"a","description":"d"}), Some(&json!(null)), None)
                .is_empty()
        );

        let empty = json!("");
        let issues = validate_skill(&json!({"name":"a","description":"d"}), Some(&empty), None);
        assert_eq!(
            issues,
            vec![json!({"level":"error","field":"body","message":"SKILL.md body is empty"})]
        );

        let blank = json!(" \n\t");
        assert_eq!(
            validate_skill(&json!({"name":"a","description":"d"}), Some(&blank), None).len(),
            1
        );

        // String(0) === "0" -> non-empty.
        let zero = json!(0);
        assert!(
            validate_skill(&json!({"name":"a","description":"d"}), Some(&zero), None).is_empty()
        );
    }

    #[test]
    fn dir_name_warning_uses_js_truthiness() {
        let other = json!("other-dir");
        let issues = validate_skill(&json!({"name":"a","description":"d"}), None, Some(&other));
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["level"], "warn");
        assert_eq!(
            issues[0]["message"],
            "directory is \"other-dir\" but name is \"a\" — some agents key the cache by directory"
        );

        assert!(
            validate_skill(&json!({"name":"a","description":"d"}), None, Some(&json!("a")))
                .is_empty()
        );
        assert!(
            validate_skill(&json!({"name":"a","description":"d"}), None, Some(&json!("")))
                .is_empty()
        );
        assert!(validate_skill(
            &json!({"name":"a","description":"d"}),
            None,
            Some(&json!(null))
        )
        .is_empty());
    }

    #[test]
    fn mcp_valid_and_unknown_key_order() {
        let cfg = json!({"command":"node","args":["server.js"],"zzz":1,"aaa":2});
        let issues = validate_mcp_server("my-server", &cfg);
        assert_eq!(
            fields(&issues),
            vec![
                ("warn".to_string(), "zzz".to_string()),
                ("warn".to_string(), "aaa".to_string())
            ]
        );
        assert_eq!(
            issues[0]["message"],
            "unrecognized key \"zzz\" — agent may ignore it"
        );
    }

    #[test]
    fn npx_without_dash_y_warns() {
        assert!(
            validate_mcp_server("srv", &json!({"command":"npx","args":["-y","pkg"]})).is_empty()
        );
        let issues = validate_mcp_server("srv", &json!({"command":"npx","args":["pkg"]}));
        assert_eq!(
            issues,
            vec![json!({"level":"warn","field":"args","message":"npx without -y can block on an interactive install prompt"})]
        );
        // No args at all -> joined "" -> also warns.
        assert_eq!(
            validate_mcp_server("srv", &json!({"command":"npx"})).len(),
            1
        );
    }

    #[test]
    fn mcp_config_shapes() {
        let null = Value::Null;
        let issues = validate_mcp_server("srv", &null);
        assert_eq!(
            issues,
            vec![json!({"level":"error","field":"config","message":"server config must be an object"})]
        );

        // Arrays are `typeof 'object'`: no early return, falls through to the
        // stdio/http error (and, being empty, produces no index warnings).
        let issues = validate_mcp_server("srv", &json!([]));
        assert_eq!(
            issues,
            vec![json!({"level":"error","field":"config","message":"server needs either `command` (stdio) or `url` (http/sse)"})]
        );

        assert!(validate_mcp_server("srv", &json!({"url":"HTTPS://example.com/sse"})).is_empty());
        assert!(!validate_mcp_server("srv", &json!({"url":"ftp://example.com"})).is_empty());
    }

    #[test]
    fn mcp_array_config_scans_indices_as_keys() {
        // Object.keys([1]) === ["0"] in JS.
        let issues = validate_mcp_server("srv", &json!([1]));
        assert_eq!(
            fields(&issues),
            vec![
                ("error".to_string(), "config".to_string()),
                ("warn".to_string(), "0".to_string())
            ]
        );
    }

    #[test]
    fn mcp_env_null_quirk() {
        assert!(validate_mcp_server("s", &json!({"command":"node","env":{}})).is_empty());
        assert!(validate_mcp_server("s", &json!({"command":"node","env":null})).is_empty());

        let issues = validate_mcp_server("s", &json!({"command":"node","env":[]}));
        assert_eq!(
            issues,
            vec![json!({"level":"error","field":"env","message":"env must be an object"})]
        );

        let issues = validate_mcp_server("s", &json!({"command":"node","env":"X"}));
        assert_eq!(issues[0]["field"], "env");
    }

    #[test]
    fn mcp_name_and_args_type() {
        assert!(validate_mcp_server("a.b-c_1", &json!({"command":"x"})).is_empty());

        let issues = validate_mcp_server("-bad", &json!({"command":"x"}));
        assert_eq!(fields(&issues), vec![("error".to_string(), "name".to_string())]);

        let issues = validate_mcp_server("s", &json!({"command":"x","args":"nope"}));
        assert_eq!(
            issues,
            vec![json!({"level":"error","field":"args","message":"args must be an array of strings"})]
        );
    }

    #[test]
    fn plugin_checks() {
        assert!(validate_plugin(&json!({"name":"p","version":"2.1","description":"x"})).is_empty());

        let issues = validate_plugin(&json!({}));
        assert_eq!(
            issues,
            vec![json!({"level":"error","field":"name","message":"plugin name is required"})]
        );

        let long: String = "x".repeat(201);
        let issues = validate_plugin(&json!({"name":"p","description":long}));
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["field"], "description");

        let issues = validate_plugin(&json!({"name":"p","version":"v2"}));
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["field"], "version");
    }

    #[test]
    fn rule_and_memory() {
        assert!(validate_rule("hello").is_empty());
        assert_eq!(validate_rule("  \n ").len(), 1);
        assert_eq!(validate_memory("").len(), 1);
        assert!(validate_memory("note").is_empty());

        let at_limit = "a".repeat(200_000);
        assert!(validate_rule(&at_limit).is_empty());
        let over = "a".repeat(200_001);
        assert_eq!(validate_rule(&over).len(), 1);
    }

    #[test]
    fn by_kind_dispatch() {
        let payload = json!({"data":{"name":"a","description":"d"},"body":"hi","dirName":"a"});
        assert!(validate_by_kind("skill", &payload).is_empty());

        let issues = validate_by_kind("nope", &json!({}));
        assert_eq!(
            issues,
            vec![json!({"level":"error","field":"kind","message":"unknown kind nope"})]
        );

        // plugin: `payload.data || payload`
        assert!(validate_by_kind("plugin", &json!({"name":"p"})).is_empty());

        // mcp: a missing config is `undefined` -> early config error.
        let issues = validate_by_kind("mcp", &json!({"name":"srv"}));
        assert_eq!(issues[0]["message"], "server config must be an object");

        // `String(body || '')`
        let issues = validate_by_kind("memory", &json!({"body":""}));
        assert_eq!(issues.len(), 1);
        let issues = validate_by_kind("memory", &json!({"body":0}));
        assert_eq!(issues.len(), 1);
        assert!(validate_by_kind("memory", &json!({"body":"kept"})).is_empty());
    }

    #[test]
    fn is_blocking_and_key_order() {
        let issues = validate_skill(&json!({}), None, None);
        let keys: Vec<String> = issues[0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, vec!["level", "field", "message"]);
        assert!(is_blocking(&issues));
        assert!(!is_blocking(&[]));

        let warns = validate_skill(
            &json!({"name":"a","description":"d","version":"nope"}),
            None,
            None,
        );
        assert!(!warns.is_empty());
        assert!(!is_blocking(&warns));
    }
}
