//! Markdown frontmatter 解析。
//!
//! YAML 用真正的解析器解析，文档不合法时退回宽容的标量扫描。这个兜底不是为了
//! 好看：Cursor 的 `.mdc` 文件里经常写着 `globs: **/*.ts`，而 `*` 在 YAML 里是
//! 保留字符，所以只用严格解析器，恰恰会在 Rules 视图存在的意义所要展示的那些
//! 文件上报"没有 frontmatter"。

use serde_json::{json, Map, Value};

pub struct Frontmatter {
    pub data: Value,
    pub body: String,
    pub has_frontmatter: bool,
    pub order: Vec<String>,
}

/// `findKeySeparator`——结束一个 key 的第一个 `:`，跳过被引号包住的冒号，
/// 以及后面不跟空白的冒号（所以 `http://x` 是安全的）。
pub fn find_key_separator(body: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (idx, ch) in body.char_indices() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if ch == ':' {
            let next = body[idx + ch.len_utf8()..].chars().next();
            if next.is_none() || next.map(|c| c.is_whitespace()).unwrap_or(false) {
                return Some(idx);
            }
        }
    }
    None
}

fn unquote_key(k: &str) -> String {
    let t = k.trim();
    let bytes = t.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return t[1..t.len() - 1].to_string();
        }
    }
    t.to_string()
}

/// 切分 `---\n...\n---\nbody`。只是开头有 `---` 却始终没闭合的文件，按没有
/// frontmatter 处理，与参考实现的正则一致。
pub fn parse(text: &str) -> Frontmatter {
    let norm = text.strip_prefix('\u{feff}').unwrap_or(text);

    if !norm.starts_with("---") {
        return Frontmatter {
            data: json!({}),
            body: norm.to_string(),
            has_frontmatter: false,
            order: Vec::new(),
        };
    }

    // 首行必须正好是 `---`，后面只允许可选的空格、制表符。
    let first_line_end = match norm.find('\n') {
        Some(i) => i,
        None => {
            return Frontmatter {
                data: json!({}),
                body: norm.to_string(),
                has_frontmatter: false,
                order: Vec::new(),
            }
        }
    };
    let first_line = norm[..first_line_end].trim_end_matches('\r');
    if !is_fence(first_line) {
        return Frontmatter {
            data: json!({}),
            body: norm.to_string(),
            has_frontmatter: false,
            order: Vec::new(),
        };
    }

    let raw_start = first_line_end + 1;
    // 逐行找闭合的那道围栏。
    let mut pos = raw_start;
    while pos <= norm.len() {
        let line_end = norm[pos..]
            .find('\n')
            .map(|i| pos + i)
            .unwrap_or(norm.len());
        let line = norm[pos..line_end].trim_end_matches('\r');
        if is_fence(line) {
            let raw = &norm[raw_start..pos.saturating_sub(if pos > raw_start {
                line_sep_len(norm, pos)
            } else {
                0
            })];
            let body_start = if line_end < norm.len() {
                line_end + 1
            } else {
                line_end
            };
            let mut order = Vec::new();
            for l in raw.split('\n') {
                let l = l.trim_end_matches('\r');
                if let Some(ci) = find_key_separator(l.trim()) {
                    let k = unquote_key(&l.trim()[..ci]);
                    if !order.contains(&k) {
                        order.push(k);
                    }
                }
            }
            let data = parse_yaml(raw);
            return Frontmatter {
                data,
                body: norm[body_start..].to_string(),
                has_frontmatter: true,
                order,
            };
        }
        if line_end >= norm.len() {
            break;
        }
        pos = line_end + 1;
    }

    Frontmatter {
        data: json!({}),
        body: norm.to_string(),
        has_frontmatter: false,
        order: Vec::new(),
    }
}

fn line_sep_len(s: &str, pos: usize) -> usize {
    if pos >= 2 && s.as_bytes()[pos - 2] == b'\r' {
        2
    } else {
        1
    }
}

fn is_fence(line: &str) -> bool {
    let t = line.trim_end();
    if !t.starts_with("---") {
        return false;
    }
    let rest = &t[3..];
    rest.chars().all(|c| c == ' ' || c == '\t')
}

fn parse_yaml(raw: &str) -> Value {
    match serde_yaml_ng::from_str::<Value>(raw) {
        Ok(Value::Null) => json!({}),
        Ok(v @ Value::Object(_)) => v,
        Ok(other) => {
            // 文档是标量、或者是键不是字符串的映射：保留宽容读法的结果，
            // 而不是凭空造一个对象出来。
            let tolerant = parse_tolerant(raw);
            if tolerant.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
                tolerant
            } else {
                other
            }
        }
        Err(_) => parse_tolerant(raw),
    }
}

/// 对不是合法 YAML 的 frontmatter，尽力而为地提取 `key: scalar`。
/// 嵌套结构是有意不恢复的。
pub fn parse_tolerant(src: &str) -> Value {
    let mut out = Map::new();
    for line in src.split('\n') {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || line.starts_with(' ')
            || line.starts_with('\t')
        {
            continue;
        }
        let ci = match find_key_separator(trimmed) {
            Some(i) => i,
            None => continue,
        };
        let key = unquote_key(&trimmed[..ci]);
        let mut val = trimmed[ci + 1..].trim().to_string();
        let value = if (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
            || (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2)
        {
            Value::String(val[1..val.len() - 1].to_string())
        } else if val == "true" {
            Value::Bool(true)
        } else if val == "false" {
            Value::Bool(false)
        } else if val.is_empty() || val == "~" || val == "null" {
            Value::String(String::new())
        } else if let Some(n) = parse_js_number(&val) {
            n
        } else {
            Value::String(std::mem::take(&mut val))
        };
        if !key.is_empty() {
            out.insert(key, value);
        }
    }
    Value::Object(out)
}

/// `-?\d+(\.\d+)?` → Number。整数结果按整数输出，这样序列化出来和 JS 数字
/// 一模一样（`1`，而不是 `1.0`）。
fn parse_js_number(s: &str) -> Option<Value> {
    let body = s.strip_prefix('-').unwrap_or(s);
    if body.is_empty() {
        return None;
    }
    let mut parts = body.split('.');
    let int_part = parts.next()?;
    if int_part.is_empty() || !int_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if let Some(frac) = parts.next() {
        if frac.is_empty() || !frac.chars().all(|c| c.is_ascii_digit()) || parts.next().is_some() {
            return None;
        }
        let f: f64 = s.parse().ok()?;
        return Some(number_from_f64(f));
    }
    if parts.next().is_some() {
        return None;
    }
    s.parse::<i64>().ok().map(Value::from)
}

fn number_from_f64(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// `serializeFrontmatter(data, body, order)`——重建 `---…---\nbody`。
///
/// 字段顺序是对文件作者的照顾：调用方传入从原文里还原出来的 key 顺序，数据后来
/// 新增的 key 一律追加在末尾而不是插进来打乱。提示只提供*顺序*——
/// 值永远来自 `data`，所以过期的提示不可能把已删除的字段复活。
pub fn serialize(data: &Value, body: &str, order: &[&str]) -> String {
    let mut ordered = Map::new();
    if let Value::Object(m) = data {
        for k in order {
            if let Some(v) = m.get(*k) {
                ordered.insert((*k).to_string(), v.clone());
            }
        }
        for (k, v) in m {
            if !order.contains(&k.as_str()) {
                ordered.insert(k.clone(), v.clone());
            }
        }
    }
    let front = crate::yaml::stringify(&Value::Object(ordered));
    // `body.startsWith('\n') ? body.slice(1) : body`——闭合围栏后面那个空行
    // 由围栏自己补上。
    let body = body.strip_prefix('\n').unwrap_or(body);
    format!("---\n{}\n---\n{}", front, body)
}
