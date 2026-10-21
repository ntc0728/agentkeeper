//! YAML 1.2 emit + parse, matching the `yaml` npm package closely enough that
//! a write followed by the agent's own reader is a no-op.
//!
//! Two callers:
//!   * `frontmatter::serialize` — SKILL.md / rule frontmatter
//!   * `keeper::upsert_mcp` / `delete_mcp` — YAML-shaped MCP configs
//!
//! Contract (do not change the signatures — two other modules are compiled
//! against them):
//!
//! * `parse(src) -> Result<Value, String>` — full YAML 1.2. `Err` carries a
//!   human-readable message; callers turn that into
//!   `config file is corrupt, refusing to overwrite: <path>`.
//! * `stringify(&Value) -> String` — block style, two-space indent, **never
//!   wrapped** (`YAML.stringify(v, {lineWidth: 0})`), and with **no trailing
//!   newline** (`stringifyYamlSubset` strips it).
//!
//! Parsing is delegated to `serde_yaml_ng` (the same parser
//! `frontmatter::parse_yaml` uses, so the two can never disagree). An empty
//! document arrives as `Value::Null`; the JS callers spell that `|| {}` and
//! the Rust ones do the same.
//!
//! Emitting is hand-written rather than `serde_yaml_ng::to_string` because the
//! npm package is called with `lineWidth: 0` and serde_yaml wraps at 80
//! columns. Round-trip fidelity is the hard requirement: for every value `v`,
//! `parse(&stringify(&v)) == v`, integers staying integers and floats staying
//! floats. Cosmetic differences from the npm emitter, all documented here:
//!
//!   * multi-line strings are written as one double-quoted line with `\n`
//!     escapes instead of a `|` literal block (a block would also be correct,
//!     but chomping/indentation edge cases are not worth the round-trip risk);
//!   * strings that need quoting are double-quoted; npm prefers single quotes
//!     when the string contains no escapes;
//!   * a plain string is kept unquoted whenever it re-parses as that exact
//!     string, which is decided by actually parsing the candidate, not by
//!     pattern-matching YAML's reserved words.
//!
//! Everything else (block style, two-space indent, `- key: value` sequence
//! layout, `{}` / `[]` for empty collections, key order) follows npm's output.

use serde_json::{Map, Value};

/// `YAML.parse()` — `Err(String)` on a syntax error.
pub fn parse(src: &str) -> Result<Value, String> {
    serde_yaml_ng::from_str::<Value>(src).map_err(|e| e.to_string())
}

/// `YAML.stringify(v, { lineWidth: 0 })` minus the trailing newline.
pub fn stringify(value: &Value) -> String {
    emit_node(value, 0).join("\n")
}

fn pad(indent: usize) -> String {
    " ".repeat(indent)
}

/// Scalar or empty collection rendered on one line; `None` for anything that
/// has to be written as a nested block.
fn inline(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("null".to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(scalar(s)),
        Value::Object(m) if m.is_empty() => Some("{}".to_string()),
        Value::Array(a) if a.is_empty() => Some("[]".to_string()),
        _ => None,
    }
}

fn emit_node(value: &Value, indent: usize) -> Vec<String> {
    match value {
        Value::Object(m) if !m.is_empty() => emit_map(m, indent),
        Value::Array(a) if !a.is_empty() => emit_seq(a, indent),
        other => vec![format!("{}{}", pad(indent), inline(other).unwrap_or_default())],
    }
}

fn emit_map(map: &Map<String, Value>, indent: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(map.len());
    for (k, v) in map {
        let key = format_key(k);
        match inline(v) {
            Some(text) => out.push(format!("{}{}: {}", pad(indent), key, text)),
            None => {
                out.push(format!("{}{}:", pad(indent), key));
                out.extend(emit_node(v, indent + 2));
            }
        }
    }
    out
}

fn emit_seq(items: &[Value], indent: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match inline(item) {
            Some(text) => out.push(format!("{}- {}", pad(indent), text)),
            None => {
                // npm puts the first key of a map item on the dash line:
                //   - name: a
                //     value: b
                let mut sub = emit_node(item, indent + 2);
                let head = {
                    let first = sub[0].strip_prefix(&pad(indent + 2)).unwrap_or(&sub[0]);
                    format!("{}- {}", pad(indent), first)
                };
                out.push(head);
                out.extend(sub.drain(1..));
            }
        }
    }
    out
}

fn format_key(key: &str) -> String {
    if plain_key_ok(key) {
        key.to_string()
    } else {
        double_quote(key)
    }
}

/// A key is safe unquoted when `key: x` parses back to exactly that one entry.
/// The probe costs one tiny parse per key, which is nothing next to the file
/// write it is about to be part of.
fn plain_key_ok(key: &str) -> bool {
    if key.is_empty() || key.chars().any(is_control) {
        return false;
    }
    match parse(&format!("{}: x", key)) {
        Ok(Value::Object(m)) => m.len() == 1 && matches!(m.get(key), Some(Value::String(v)) if v == "x"),
        _ => false,
    }
}

fn scalar(s: &str) -> String {
    if plain_ok(s) {
        s.to_string()
    } else {
        double_quote(s)
    }
}

/// `true` when `s` written bare still reads back as the same string. This is
/// the definition npm's emitter approximates: anything that resolves to a
/// number, boolean, null, timestamp or a different structure gets quoted.
fn plain_ok(s: &str) -> bool {
    if s.is_empty() || s.starts_with(' ') || s.ends_with(' ') {
        return false;
    }
    if s.chars().any(is_control) {
        return false;
    }
    matches!(parse(s), Ok(Value::String(t)) if t == s)
}

fn is_control(c: char) -> bool {
    (c as u32) < 0x20 || c == '\u{7f}'
}

/// YAML double-quoted scalar. UTF-8 passes through unchanged; only the quote,
/// the backslash and control characters are escaped.
fn double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if is_control(c) => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(v: Value) {
        let text = stringify(&v);
        assert!(!text.ends_with('\n'), "trailing newline in {:?}", text);
        let back = parse(&text).unwrap_or_else(|e| panic!("reparse failed for {:?}: {}", text, e));
        assert_eq!(back, v, "round-trip mismatch through {:?}", text);
    }

    #[test]
    fn round_trips_fixture_set() {
        round_trip(json!({
            "name": "demo",
            "count": 3,
            "neg": -12,
            "ratio": 1.5,
            "whole_float": 1.0,
            "enabled": true,
            "disabled": false,
            "missing": null,
            "numeric_text": "1.0",
            "hash_text": "a: b # not a comment",
            "empty": "",
            "padded": "  spaces  ",
            "bool_text": "true",
            "null_text": "null",
            "tilde_text": "~",
            "multi": "line one\nline two\n",
            "cjk": "中文标题：测试",
            "nested": { "deep": { "deeper": [1, 2, 3] } },
            "empty_obj": {},
            "empty_arr": [],
            "list": ["a", "b", 3, true, null],
            "rows": [
                { "k": "v1", "n": 1 },
                { "k": "v2", "n": 2, "extra": { "x": [] } }
            ],
            "matrix": [[1, 2], [3, 4]]
        }));
    }

    #[test]
    fn empty_collections_are_flow_style() {
        assert_eq!(stringify(&json!({})), "{}");
        assert_eq!(stringify(&json!([])), "[]");
        assert_eq!(stringify(&Value::Null), "null");
        assert_eq!(parse("").unwrap(), Value::Null);
        assert_eq!(parse("{}").unwrap(), json!({}));
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(stringify(&json!("plain text")), "plain text");
        assert_eq!(stringify(&json!("1.0")), "\"1.0\"");
        assert_eq!(stringify(&json!("true")), "\"true\"");
        assert_eq!(stringify(&json!("~")), "\"~\"");
        assert_eq!(stringify(&json!("")), "\"\"");
        assert_eq!(stringify(&json!(" padded ")), "\" padded \"");
        assert_eq!(stringify(&json!("a: b")), "\"a: b\"");
        assert_eq!(stringify(&json!("a #b")), "\"a #b\"");
        assert_eq!(stringify(&json!("[x]")), "\"[x]\"");
        assert_eq!(stringify(&json!("{a: 1}")), "\"{a: 1}\"");
    }

    #[test]
    fn layout_matches_npm_shape() {
        let text = stringify(&json!({
            "rows": [{ "k": "v", "n": 1 }],
            "nested": { "deep": true }
        }));
        assert_eq!(
            text,
            "rows:\n  - k: v\n    n: 1\nnested:\n  deep: true"
        );
    }

    #[test]
    fn long_lines_are_not_wrapped() {
        let long = "word ".repeat(80).trim_end().to_string();
        // A 400-character scalar must stay on the one line its key owns.
        let text = stringify(&json!({ "body": long }));
        assert_eq!(text.lines().count(), 1, "value was wrapped: {:?}", text);
        assert!(text.ends_with(&long));
        // A bare scalar is one line too.
        let bare = stringify(&json!(long));
        assert_eq!(bare.lines().count(), 1);
        assert_eq!(bare, long);
    }

    #[test]
    fn empty_document_and_syntax_error() {
        assert_eq!(parse("# only a comment\n").unwrap(), Value::Null);
        let err = parse("a: [1, 2").unwrap_err();
        assert!(!err.is_empty());
        // Corrupt input must be an error, never a silent empty object.
        assert!(parse("\t- broken: [").is_err());
    }
}
