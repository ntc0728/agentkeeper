//! TOML round-trip and comment-preserving section surgery — a port of the
//! TOML half of `core/keeper.js` plus `lineDiff()`.
//!
//! Contract (do not change the signatures — `keeper.rs` is compiled against
//! them):
//!
//! * `parse(src) -> Result<Value, String>`
//! * `stringify(&Value) -> Result<String, String>`
//! * `splice_server(raw, server, section) -> String` — replace the
//!   `[mcp_servers.<server>]` region (including nested subtables) with
//!   `section`, or append it when the server is new. Every byte outside that
//!   region — comments included — must survive.
//! * `remove_server(raw, server) -> String`
//! * `line_diff(old, new) -> Vec<Value>`
//!
//! The splice helpers are byte-for-byte ports of `spliceTomlServer`,
//! `removeTomlServer`, `findTomlServerRegion` and `lineDiff`, including the
//! quirks: `\r?\n` line splitting that keeps a trailing empty line, the
//! `\n{3,}` collapse that only ever fires for LF files (CRLF never contains
//! three adjacent `\n`), and `\s` being JavaScript's whitespace set.
//!
//! `stringify` emits a document with `toml::to_string`, which produces the
//! `[mcp_servers.<name>]` header followed by its keys and a
//! `[mcp_servers.<name>.env]` subtable — the same shape `@iarna/toml` writes.
//! Its exact spacing only needs to be valid TOML because the result is spliced
//! textually and re-parsed afterwards.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::{ecma_trim, ecma_trim_end};

/// `TOML.parse()` — `Err(String)` on a syntax error.
pub fn parse(src: &str) -> Result<Value, String> {
    let doc: toml::Value = toml::from_str(src).map_err(|e| e.message().to_string())?;
    Ok(to_json(doc))
}

/// TOML datetimes have no `serde_json::Value` counterpart; the JS side gets a
/// `Date` and only ever stringifies it back, so keeping the literal text is
/// the closest faithful mapping (and, unlike a hard error, does not make a
/// config with a timestamp look corrupt).
fn to_json(value: toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::from(i),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.into_iter().map(to_json).collect()),
        toml::Value::Table(t) => {
            Value::Object(t.into_iter().map(|(k, v)| (k, to_json(v))).collect())
        }
    }
}

/// `TOML.stringify()`
pub fn stringify(value: &Value) -> Result<String, String> {
    toml::to_string(value).map_err(|e| e.to_string())
}

/* ------------------------------------------------------------------ */
/* Section surgery                                                     */
/* ------------------------------------------------------------------ */

/// `String(x).split(/\r?\n/)` — a trailing separator yields a trailing empty
/// element, and a lone `\r` does not split.
fn split_lines(raw: &str) -> Vec<String> {
    raw.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect()
}

fn header_re(server: &str) -> Regex {
    Regex::new(&format!(
        r#"^\s*\[\s*["']?mcp_servers["']?\s*\.\s*["']?{}["']?\s*\]\s*$"#,
        regex::escape(server)
    ))
    .expect("server header pattern is always valid")
}

fn any_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\[\s*([^\]]+?)\s*\]\s*$").unwrap())
}

/// `cleanTomlKey` — strip the brackets, trim, then peel one layer of quotes.
fn clean_key(h: &str) -> String {
    let t = ecma_trim(h.trim_start_matches('[').trim_end_matches(']'));
    let b = t.as_bytes();
    if t.len() >= 2 && b[0] == b'"' && b[t.len() - 1] == b'"' && !t[1..t.len() - 1].contains('"') {
        return t[1..t.len() - 1].to_string();
    }
    if t.len() >= 2 && b[0] == b'\'' && b[t.len() - 1] == b'\'' && !t[1..t.len() - 1].contains('\'') {
        return t[1..t.len() - 1].to_string();
    }
    t.to_string()
}

struct Region {
    start: usize,
    end: usize,
}

/// `findTomlServerRegion` — the header line plus every following line, up to
/// (not including) the next header that is a different table.
fn find_region(lines: &[String], server: &str) -> Option<Region> {
    let head = header_re(server);
    let start = lines.iter().position(|l| head.is_match(l))?;
    let any = any_header_re();
    let base = clean_key(&any.captures(&lines[start])?.get(1)?.as_str());
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        let Some(caps) = any.captures(line) else { continue };
        let key = clean_key(&caps[1]);
        if key == base || key.starts_with(&format!("{}.", base)) {
            continue;
        }
        end = i;
        break;
    }
    Some(Region { start, end })
}

/// `s.replace(/\n{3,}/g, '\n\n')` — runs of bare `\n` only, so CRLF files are
/// untouched, exactly like the JS regex.
fn collapse_newlines(s: &str) -> String {
    fn flush(run: usize, out: &mut String) {
        if run >= 3 {
            out.push_str("\n\n");
        } else {
            for _ in 0..run {
                out.push('\n');
            }
        }
    }
    let mut out = String::with_capacity(s.len());
    let mut run = 0usize;
    for c in s.chars() {
        if c == '\n' {
            run += 1;
        } else {
            flush(run, &mut out);
            run = 0;
            out.push(c);
        }
    }
    flush(run, &mut out);
    out
}

/// `spliceTomlServer(raw, server, sectionText)`
pub fn splice_server(raw: &str, server: &str, section: &str) -> String {
    let eol = if raw.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines = split_lines(raw);
    let section_lines: Vec<String> = section
        .trim_end_matches('\n')
        .split('\n')
        .map(|l| l.to_string())
        .collect();
    match find_region(&lines, server) {
        Some(region) => {
            lines.splice(region.start..region.end, section_lines);
            collapse_newlines(&lines.join(eol))
        }
        None => {
            let prefix = ecma_trim_end(raw);
            if prefix.is_empty() {
                section.to_string()
            } else {
                format!("{}{}{}{}", prefix, eol, eol, section)
            }
        }
    }
}

/// `removeTomlServer(raw, server)`
pub fn remove_server(raw: &str, server: &str) -> String {
    let eol = if raw.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines = split_lines(raw);
    let Some(region) = find_region(&lines, server) else {
        return raw.to_string();
    };
    lines.drain(region.start..region.end);
    let joined = collapse_newlines(&lines.join(eol));
    format!("{}{}", ecma_trim_end(&joined), eol)
}

/* ------------------------------------------------------------------ */
/* Diff                                                                */
/* ------------------------------------------------------------------ */

/// `lineDiff(a, b)` → `[{t:'mod',n,old,next} | {t:'del',n,old} | {t:'add',n,next}]`,
/// truncated to 400 entries.
pub fn line_diff(old: &str, new: &str) -> Vec<Value> {
    let a = split_lines(old);
    let b = split_lines(new);
    let n = a.len().max(b.len());
    let mut out = Vec::with_capacity(n.min(400));
    for i in 0..n {
        let x = a.get(i).map(String::as_str);
        let y = b.get(i).map(String::as_str);
        if x == y {
            continue;
        }
        let mut entry = Map::new();
        let num = i + 1;
        match (x, y) {
            (Some(old_line), Some(next_line)) => {
                entry.insert("t".into(), Value::from("mod"));
                entry.insert("n".into(), Value::from(num));
                entry.insert("old".into(), Value::from(old_line));
                entry.insert("next".into(), Value::from(next_line));
            }
            (Some(old_line), None) => {
                entry.insert("t".into(), Value::from("del"));
                entry.insert("n".into(), Value::from(num));
                entry.insert("old".into(), Value::from(old_line));
            }
            (None, Some(next_line)) => {
                entry.insert("t".into(), Value::from("add"));
                entry.insert("n".into(), Value::from(num));
                entry.insert("next".into(), Value::from(next_line));
            }
            (None, None) => unreachable!("i < max(len_a, len_b)"),
        }
        out.push(Value::Object(entry));
        if out.len() == 400 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RAW: &str = "# agentkeeper: keep this header\n\
\n\
[mcp_servers.alpha]\n\
command = \"alpha-cmd\"\n\
args = [\"--x\"]\n\
\n\
# between\n\
[mcp_servers.beta]\n\
command = \"beta-cmd\"\n\
\n\
# tail comment\n\
[other]\n\
k = 1";

    #[test]
    fn splice_replaces_only_the_server_region() {
        // A comment sitting between two tables belongs to the *preceding*
        // region (the JS region ends at the next header line, whatever is in
        // between), so "# between" goes away with alpha; "# tail comment"
        // still belongs to beta and is not touched.
        let section = "[mcp_servers.alpha]\ncommand = \"new-cmd\"\nenv = { A = \"1\" }\n";
        let expected = "# agentkeeper: keep this header\n\
\n\
[mcp_servers.alpha]\n\
command = \"new-cmd\"\n\
env = { A = \"1\" }\n\
[mcp_servers.beta]\n\
command = \"beta-cmd\"\n\
\n\
# tail comment\n\
[other]\n\
k = 1";
        assert_eq!(splice_server(RAW, "alpha", section), expected);
    }

    #[test]
    fn splice_keeps_subtables_of_the_same_server() {
        let raw = "[mcp_servers.alpha]\ncommand = \"a\"\n\n[mcp_servers.alpha.env]\nK = \"V\"\n\n[other]\nk = 1\n";
        let section = "[mcp_servers.alpha]\ncommand = \"b\"\n";
        let expected = "[mcp_servers.alpha]\ncommand = \"b\"\n[other]\nk = 1\n";
        assert_eq!(splice_server(raw, "alpha", section), expected);
    }

    #[test]
    fn splice_appends_when_absent() {
        let raw = "# top\n\n[other]\nk = 1\n";
        let section = "[mcp_servers.new]\ncommand = \"x\"\n";
        assert_eq!(
            splice_server(raw, "new", section),
            "# top\n\n[other]\nk = 1\n\n[mcp_servers.new]\ncommand = \"x\"\n"
        );
        assert_eq!(
            splice_server("", "new", section),
            "[mcp_servers.new]\ncommand = \"x\"\n"
        );
    }

    #[test]
    fn remove_drops_the_region_and_absent_is_a_no_op() {
        let expected = "# agentkeeper: keep this header\n\
\n\
[mcp_servers.beta]\n\
command = \"beta-cmd\"\n\
\n\
# tail comment\n\
[other]\n\
k = 1\n";
        assert_eq!(remove_server(RAW, "alpha"), expected);
        assert_eq!(remove_server(RAW, "nope"), RAW);
    }

    #[test]
    fn crlf_is_preserved() {
        let raw = "# top\r\n[mcp_servers.a]\r\ncommand = \"x\"\r\n";
        // The region runs to end of file and swallows the trailing empty
        // element produced by the final CRLF, and the section's own trailing
        // newline is stripped — same as the JS port.
        let out = splice_server(raw, "a", "[mcp_servers.a]\ncommand = \"y\"\n");
        assert_eq!(out, "# top\r\n[mcp_servers.a]\r\ncommand = \"y\"");
        assert!(!out.replace("\r\n", "").contains('\n'));

        let removed = remove_server(raw, "a");
        assert_eq!(removed, "# top\r\n");
    }

    #[test]
    fn collapse_newlines_matches_the_js_regex() {
        let raw = "[mcp_servers.a]\ncommand = \"x\"\n[other]\n\n\n\nk = 1\n";
        let out = splice_server(raw, "a", "[mcp_servers.a]\ncommand = \"y\"\n");
        assert_eq!(out, "[mcp_servers.a]\ncommand = \"y\"\n[other]\n\nk = 1\n");
        // Two newlines stay two; only three or more collapse.
        assert_eq!(collapse_newlines("a\n\nb"), "a\n\nb");
        assert_eq!(collapse_newlines("a\n\n\nb"), "a\n\nb");
        assert_eq!(collapse_newlines("a\n\n\n\n\nb"), "a\n\nb");
        assert_eq!(collapse_newlines("a\r\n\r\n\r\nb"), "a\r\n\r\n\r\nb");
    }

    #[test]
    fn quoted_header_and_key_forms_are_found() {
        let raw = "[\"mcp_servers\".'a.b']\ncommand = \"x\"\n\n[other]\nk = 1\n";
        let out = splice_server(raw, "a.b", "[\"mcp_servers\".'a.b']\ncommand = \"y\"\n");
        assert_eq!(out, "[\"mcp_servers\".'a.b']\ncommand = \"y\"\n[other]\nk = 1\n");
    }

    #[test]
    fn stringify_emits_tables_that_reparse() {
        let v = json!({
            "mcp_servers": {
                "demo": {
                    "command": "npx",
                    "args": ["-y", "@scope/server"],
                    "env": { "TOKEN": "abc" }
                }
            }
        });
        let text = stringify(&v).unwrap();
        assert!(text.contains("[mcp_servers.demo]"), "{}", text);
        assert!(text.contains("[mcp_servers.demo.env]"), "{}", text);
        assert_eq!(parse(&text).unwrap(), v);
    }

    #[test]
    fn parse_errors_and_empty_documents() {
        assert_eq!(parse("").unwrap(), json!({}));
        assert!(parse("bad = [").is_err());
        assert_eq!(parse("a = 1").unwrap(), json!({ "a": 1 }));
        assert_eq!(parse("a = 1.0").unwrap()["a"].as_f64(), Some(1.0));
    }

    #[test]
    fn line_diff_marks_mod_del_add() {
        let d = line_diff("l1\nl2\nl3", "l1\nL2\nl3\nl4");
        assert_eq!(d, vec![
            json!({"t": "mod", "n": 2, "old": "l2", "next": "L2"}),
            json!({"t": "add", "n": 4, "next": "l4"}),
        ]);

        let d = line_diff("l1\nl2\nl3", "l1");
        assert_eq!(d, vec![
            json!({"t": "del", "n": 2, "old": "l2"}),
            json!({"t": "del", "n": 3, "old": "l3"}),
        ]);

        let d = line_diff("a\r\nb", "a\r\nc");
        assert_eq!(d, vec![json!({"t": "mod", "n": 2, "old": "b", "next": "c"})]);

        assert!(line_diff("same", "same").is_empty());
    }

    #[test]
    fn line_diff_truncates_at_400() {
        let old: String = (0..500).map(|i| format!("old{}\n", i)).collect();
        let new: String = (0..500).map(|i| format!("new{}\n", i)).collect();
        let d = line_diff(&old, &new);
        assert_eq!(d.len(), 400);
        assert_eq!(d[399]["n"], json!(400));
        assert_eq!(d[399]["t"], json!("mod"));
    }
}
