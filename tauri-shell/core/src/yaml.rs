//! YAML 1.2 的输出与解析，紧贴 `yaml` npm 包的行为到这种程度：我们写一次文件，
//! 随后 agent 用它自己的读取器读回去，等于什么都没发生。
//!
//! 两个调用方：
//!   * `frontmatter::serialize` —— SKILL.md / rule 的 frontmatter
//!   * `keeper::upsert_mcp` / `delete_mcp` —— YAML 形态的 MCP 配置
//!
//! 契约（不要改这些签名——另外两个模块是照着它们编译的）：
//!
//! * `parse(src) -> Result<Value, String>` —— 完整的 YAML 1.2。`Err` 里带一条
//!   给人读的消息；调用方会把它变成
//!   `config file is corrupt, refusing to overwrite: <path>`。
//! * `stringify(&Value) -> String` —— 块风格、两空格缩进、**绝不折行**
//!   （`YAML.stringify(v, {lineWidth: 0})`），而且**不带末尾换行**
//!   （`stringifyYamlSubset` 会把它剥掉）。
//!
//! 解析交给 `serde_yaml_ng`（就是 `frontmatter::parse_yaml` 用的那个解析器，所以
//! 两者不可能给出不一致的结果）。空文档进来是 `Value::Null`；参考侧的调用方把它
//! 写成 `|| {}`，Rust 侧也一样处理。
//!
//! 输出是手写的，而不是用 `serde_yaml_ng::to_string`，因为 npm 包是按
//! `lineWidth: 0` 调用的，而 serde_yaml 会在 80 列处折行。往返保真是硬要求：
//! 对每个值 `v` 都有 `parse(&stringify(&v)) == v`，整数仍然是整数、浮点仍然是
//! 浮点。与 npm 输出器之间的外观差异，都在这里记录清楚：
//!
//!   * 多行字符串写成一行双引号字符串加 `\n` 转义，而不是 `|` 字面量块
//!     （用块本来也是正确的，但 chomping / 缩进的边界情况不值得冒往返的风险）；
//!   * 需要引号的字符串一律用双引号；字符串里没有转义时 npm 更偏好单引号；
//!   * 只要一段裸字符串重新解析回来还是同一个字符串，就保持不加引号——这是靠
//!     真正去解析那个候选串来判定的，而不是靠模式匹配 YAML 的保留字。
//!
//! 其余部分（块风格、两空格缩进、`- key: value` 序列布局、空集合写成 `{}` /
//! `[]`、键的顺序）都跟随 npm 的输出。

use serde_json::{Map, Value};

/// `YAML.parse()` —— 语法错误时返回 `Err(String)`。
pub fn parse(src: &str) -> Result<Value, String> {
    serde_yaml_ng::from_str::<Value>(src).map_err(|e| e.to_string())
}

/// `YAML.stringify(v, { lineWidth: 0 })`，去掉末尾那一个换行。
pub fn stringify(value: &Value) -> String {
    emit_node(value, 0).join("\n")
}

fn pad(indent: usize) -> String {
    " ".repeat(indent)
}

/// 标量或空集合渲染在同一行；凡是必须写成嵌套块的都返回 `None`。
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
        other => vec![format!(
            "{}{}",
            pad(indent),
            inline(other).unwrap_or_default()
        )],
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
                // npm 会把 map 项的第一个键放在短横线那一行上：
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

/// 当 `key: x` 解析回来恰好就是那一条时，这个键不加引号是安全的。这次探测对每个
/// 键只多花一次极小的解析，和它即将参与的那次文件写入相比不值一提。
fn plain_key_ok(key: &str) -> bool {
    if key.is_empty() || key.chars().any(is_control) {
        return false;
    }
    match parse(&format!("{}: x", key)) {
        Ok(Value::Object(m)) => {
            m.len() == 1 && matches!(m.get(key), Some(Value::String(v)) if v == "x")
        }
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

/// 当 `s` 裸着写出去、读回来仍然是同一个字符串时为 `true`。这正是 npm 输出器
/// 近似逼近的那条定义：凡是会被解析成数字、布尔、null、时间戳或另一种结构的，
/// 都要加引号。
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

/// YAML 双引号标量。UTF-8 原样穿过；只有引号、反斜杠和控制字符需要转义。
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
        assert_eq!(text, "rows:\n  - k: v\n    n: 1\nnested:\n  deep: true");
    }

    #[test]
    fn long_lines_are_not_wrapped() {
        let long = "word ".repeat(80).trim_end().to_string();
        // 400 个字符的标量必须老老实实待在它那个键所占的这一行上。
        let text = stringify(&json!({ "body": long }));
        assert_eq!(text.lines().count(), 1, "value was wrapped: {:?}", text);
        assert!(text.ends_with(&long));
        // 裸标量同样只有一行。
        let bare = stringify(&json!(long));
        assert_eq!(bare.lines().count(), 1);
        assert_eq!(bare, long);
    }

    #[test]
    fn empty_document_and_syntax_error() {
        assert_eq!(parse("# only a comment\n").unwrap(), Value::Null);
        let err = parse("a: [1, 2").unwrap_err();
        assert!(!err.is_empty());
        // 损坏的输入必须报错，绝不能悄悄变成空对象。
        assert!(parse("\t- broken: [").is_err());
    }
}
