//! `stripJsonComments`——对现实世界里那些 MCP 配置的 JSONC 宽容处理。
//!
//! 参考实现用一串正则花活来做这件事（先屏蔽 `\"`，再用"字符串或注释"的择一
//! 分支）。一个小型扫描器做同样的事却不需要屏蔽套路，而且能正确地不碰字符串
//! 内部的 `//`。
//!
//! 全程按原始字节操作，并且只*删除* ASCII 字符，所以 UTF-8 内容（配置里的中文
//! 描述）会原样保留。

pub fn strip_json_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
        }
        out.push(c);
        i += 1;
    }

    strip_trailing_commas(&out)
}

/// `/,(\s*[}\]])/g → '$1'`
fn strip_trailing_commas(src: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(src.len());
    let mut i = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while i < src.len() {
        let c = src[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == b',' {
            let mut j = i + 1;
            while j < src.len() && src[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < src.len() && (src[j] == b'}' || src[j] == b']') {
                i += 1; // drop the comma, keep the whitespace
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
