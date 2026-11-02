//! TOML 往返编解码、保留注释的分节手术，外加 `line_diff()`。
//!
//! 契约（不要改这些签名——`keeper.rs` 是照着它们编译的）：
//!
//! * 解析：`parse(src) -> Result<Value, String>`
//! * 序列化：`stringify(&Value) -> Result<String, String>`
//! * 拼接：`splice_server(raw, server, section) -> String` —— 用 `section` 替换
//!   `[mcp_servers.<server>]` 所在的整段区域（含嵌套子表），若这个 server 是
//!   新的则追加。该区域之外的每一个字节——注释也算在内——都必须活下来。
//! * 删除：`remove_server(raw, server) -> String`
//! * 行级差异：`line_diff(old, new) -> Vec<Value>`
//!
//! 这些手术辅助函数是 `spliceTomlServer`、`removeTomlServer`、
//! `findTomlServerRegion` 和 `lineDiff` 的逐字节移植，连怪癖也一并照搬：
//! 按 `\r?\n` 切行会留下末尾的空行；`\n{3,}` 折叠只对 LF 文件才会触发
//! （CRLF 里永远不会出现三个相邻的 `\n`）；`\s` 用的是这个格式本身定义的
//! 空白字符集。
//!
//! `stringify` 用 `toml::to_string` 输出文档，它会生成
//! `[mcp_servers.<name>]` 表头、其下的各个键，以及
//! `[mcp_servers.<name>.env]` 子表——和 `@iarna/toml` 写出来的形状一致。
//! 它具体的空格排布只需要是合法 TOML 就行，因为结果随后是按文本拼接、再重新
//! 解析的。

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::{ecma_trim, ecma_trim_end};

/// `TOML.parse()` —— 语法错误时返回 `Err(String)`。
pub fn parse(src: &str) -> Result<Value, String> {
    let doc: toml::Value = toml::from_str(src).map_err(|e| e.message().to_string())?;
    Ok(to_json(doc))
}

/// TOML 的 datetime 在 `serde_json::Value` 里没有对应类型；参考侧拿到的是一个
/// `Date`，而它只会把这东西再转回字符串，所以保留字面文本是最忠实的映射
/// （而且，与直接报错不同，这不会让一份带时间戳的配置看起来像是损坏了）。
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

/// 对应参考实现的 `TOML.stringify()`（值转 TOML 文本）。
pub fn stringify(value: &Value) -> Result<String, String> {
    toml::to_string(value).map_err(|e| e.to_string())
}

/* ------------------------------------------------------------------ */
/* 分节手术                                                             */
/* ------------------------------------------------------------------ */

/// `String(x).split(/\r?\n/)` —— 末尾的分隔符会产出一个末尾的空元素，而单独的
/// `\r` 不会触发切分。
fn split_lines(raw: &str) -> Vec<String> {
    raw.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect()
}

/// 把一段文本的换行统一成 `eol`（`\\r\n` 先归一成 `\n`，再按需要换回去）。
///
/// 参考侧不需要这一步：它那边的 `stringify` 产出目标风格的换行。本仓库的
/// `stringify` 走 `toml::to_string`，产出的永远是 LF —— 原样拼进一份 CRLF 文件
/// 会留下半份 CRLF、半份 LF，用户看到的 diff 是整段都变了。
fn match_nl(text: &str, eol: &str) -> String {
    let lf = text.replace("\r\n", "\n");
    if eol == "\n" {
        lf
    } else {
        lf.replace('\n', eol)
    }
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

/// `cleanTomlKey` —— 去掉方括号，trim，再剥掉一层引号。
fn clean_key(h: &str) -> String {
    let t = ecma_trim(h.trim_start_matches('[').trim_end_matches(']'));
    let b = t.as_bytes();
    if t.len() >= 2 && b[0] == b'"' && b[t.len() - 1] == b'"' && !t[1..t.len() - 1].contains('"') {
        return t[1..t.len() - 1].to_string();
    }
    if t.len() >= 2 && b[0] == b'\'' && b[t.len() - 1] == b'\'' && !t[1..t.len() - 1].contains('\'')
    {
        return t[1..t.len() - 1].to_string();
    }
    t.to_string()
}

struct Region {
    start: usize,
    end: usize,
}

/// 带**各自原换行**的行：`(去掉换行的文本, 这一行原来的换行符)`。
///
/// 为什么不能用 `Vec<String>` + 一个全局 `eol` 拼回去：那样一次「改一条服务器」
/// 会把整份文件的行尾统一成一种风格。一份手工维护的 TOML 里混着 CRLF 与 LF 是
/// 常态（Windows 编辑器、脚本、`echo` 各写各的），整份改写意味着用户的 `git diff`
/// 显示全文都变了 —— 这正是 §18.2 声称清零的那类缺陷，只是它当时只修了段落内。
type Line = (String, String);

/// 按 `\r\n` / `\n` / `\r` 切成「正文 + 换行」，两者都原样保留。
fn split_keep_eol(raw: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                out.push((raw[start..i].to_string(), "\n".to_string()));
                i += 1;
                start = i;
            }
            b'\r' => {
                let (next, nl) = if bytes.get(i + 1) == Some(&b'\n') {
                    (i + 2, "\r\n")
                } else {
                    (i + 1, "\r")
                };
                out.push((raw[start..i].to_string(), nl.to_string()));
                i = next;
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        // 末行没有换行结尾：分隔符记空串，拼回去时不会凭空补一个。
        out.push((raw[start..].to_string(), String::new()));
    }
    out
}

fn join_keep_eol(lines: &[Line]) -> String {
    let mut out = String::new();
    for (text, eol) in lines {
        out.push_str(text);
        out.push_str(eol);
    }
    out
}

fn text_of(lines: &[Line]) -> Vec<String> {
    lines.iter().map(|(t, _)| t.clone()).collect()
}

/// 新写出来的行用哪种换行：文件里**第一次**出现的换行。
///
/// 不用「含有 CRLF 就算 CRLF」：一份 LF 文件里只要有一行是 CRLF（任何 Windows
/// 编辑器都会手打出来），整段新内容就会被写成 CRLF，此后每次同步都 `next != raw`，
/// 永不幂等。取第一次出现的换行，对纯 LF / 纯 CRLF 两种文件的结果与旧行为一致。
fn dominant_eol(raw: &str) -> &'static str {
    match raw.as_bytes().iter().position(|b| *b == b'\n') {
        // `\r\n` 里 `\n` 的前一个字节是 `\r`。
        Some(i) if i > 0 && raw.as_bytes()[i - 1] == b'\r' => "\r\n",
        Some(_) => "\n",
        // 没有 `\n`：看有没有单独的 `\r`。
        None => {
            if raw.contains('\r') {
                "\r"
            } else {
                "\n"
            }
        }
    }
}

/// `findTomlServerRegion` —— 表头那一行，加上紧随其后的每一行，直到
/// （但不包含）下一个指向别的表的表头为止。
fn find_region(lines: &[String], server: &str) -> Option<Region> {
    let head = header_re(server);
    let start = lines.iter().position(|l| head.is_match(l))?;
    let any = any_header_re();
    let base = clean_key(any.captures(lines[start].as_str())?.get(1)?.as_str());
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        let Some(caps) = any.captures(line) else {
            continue;
        };
        let key = clean_key(&caps[1]);
        if key == base || key.starts_with(&format!("{}.", base)) {
            continue;
        }
        end = i;
        break;
    }
    Some(Region { start, end })
}

/// 删除时的区域边界。
///
/// **刻意与参考实现一致**：区域在下一个表头那一行结束，中间夹着的注释算**上一节**
/// 的（`remove_drops_the_region_and_absent_is_a_no_op` 与
/// `splice_replaces_only_the_server_region` 两条用例把这个语义钉死了）。
///
/// 这一轮只修「其余行被整份改写换行」的问题 —— 语义边界不动：改语义会让用户
/// 已有的 `git diff` 预期整体变化，而那不是本轮要解决的事。
fn region_for_delete(region: Region) -> Region {
    region
}

/// `s.replace(/\n{3,}/g, '\n\n')` —— 只处理纯 `\n` 构成的连续段，所以 CRLF 文件
/// 不受影响，与参考正则完全一致。
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

/// 对应参考实现的 `spliceTomlServer(raw, server, sectionText)`：换掉或追加一整段。
///
/// **只有被替换/新增的那一段会被重写**：其余每一行连同它**自己的**换行符原样拼回
/// （见 [`split_keep_eol`]）。之前这里把整份文件用同一个 `eol` 拼回去，于是一份
/// 手工维护、混着 CRLF 与 LF 的 TOML，只要动一条服务器，全文行尾都会被改成一种
/// 风格 —— 用户的 diff 显示全文都变了，正是 §18.2 声称清零的那类缺陷。
pub fn splice_server(raw: &str, server: &str, section: &str) -> String {
    // 新段落自己的换行风格：按「文件里第一次出现的换行」定，全 LF 的文件仍是 LF。
    let eol = dominant_eol(raw);
    // 末尾的空行在这里去掉：段落后面由下一段自己开头的空行把它接上 —— 留着会让
    // 「换掉一段」多长出一个空行（`keeper::upsert_mcp` 与既有夹具都按这个行为）。
    let section_body = match_nl(section, eol);
    let section_body = section_body.trim_end_matches(['\r', '\n']).to_string();
    let mut lines = split_keep_eol(raw);
    let texts = text_of(&lines);
    match find_region(&texts, server) {
        Some(region) => {
            // 被替换的那一段用**它自己原来的**换行风格写新内容：一份混着行尾的文件
            // 里，`[mcp_servers.a]` 这一节是 LF，就不该因为文件别处有 CRLF 而把
            // 这一节也改成 CRLF（那会让 diff 显示出与「改了一条服务器」无关的行）。
            // 区域里每一行都取自己的换行；区域是空行则退回文件主导风格。
            let region_eol = lines
                .get(region.start)
                .map(|(_, e)| e.as_str())
                .filter(|e| !e.is_empty())
                .unwrap_or(eol);
            let body = match_nl(section, region_eol);
            let body = body.trim_end_matches(['\r', '\n']).to_string();
            let section_lines: Vec<Line> = body
                .split(region_eol)
                .map(|l| (l.to_string(), region_eol.to_string()))
                .collect();
            lines.splice(region.start..region.end, section_lines);
            collapse_newlines(&join_keep_eol(&lines))
        }
        None => {
            let prefix = ecma_trim_end(raw);
            if prefix.is_empty() {
                format!("{}{}", section_body, eol)
            } else {
                format!("{}{}{}{}{}", prefix, eol, eol, section_body, eol)
            }
        }
    }
}

/// 对应参考实现的 `removeTomlServer(raw, server)`：删掉该 server 的整段区域。
///
/// 只删这一段自己的行；紧挨在下一段表头之前的注释**留下**（它描述的是下一节），
/// 其余每一行连同自己的换行符原样保留。
///
/// ## 为什么保留空行分隔
///
/// `find_region` 的区域是「表头 → 下一个表头之前」，里面的空行是**这一段的正文**
/// （例如值后面那个空行）。删掉它之后如果不再补回，`[a]` 与 `[b]` 就会贴在一起：
///
/// ```text
/// # about alpha          # about alpha
/// [a]                →   # about beta        ← 少了空行
/// command = "x"          [b]
///                        command = "y"
/// ```
///
/// 所以先把区域**末尾**的空行（连同它自己的换行）摘出来，区域删掉后再放回去。
pub fn remove_server(raw: &str, server: &str) -> String {
    let mut lines = split_keep_eol(raw);
    let texts = text_of(&lines);
    let Some(region) = find_region(&texts, server) else {
        return raw.to_string();
    };
    let region = region_for_delete(region);
    // 区域**末尾**的空行是「这一段自己发出的分隔」：`[a]` 与 `[b]` 之间的那个空行
    // 属于 `[a]`（`splice_server` 追加时就是 `eol eol`，多出来的一个空行挂在新区段
    // 前面）。所以删 `[a]` 时它跟着走，删 `[b]`（文件最后一节）时文件回到不带
    // 尾随空行的样子 —— 加一次、删一次正好回到原文。
    //
    // 但**区域起点之前**已经存在的空行不能动：那是用户写的分段，属于上一节与这一节
    // 之间。
    let mut end = region.end;
    while end > region.start + 1 && texts[end - 1].trim().is_empty() {
        end -= 1;
    }
    // 区域最后一行原来的换行符会在 drain 时一起消失；把它补回到「留下来的最后一行」
    // 上，否则两段会粘在同一个换行里。区域从文件第一行开始时没有上一行，不补。
    let region_last_eol = if end > region.start {
        lines[end - 1].1.clone()
    } else {
        String::new()
    };
    lines.drain(region.start..end);
    if region.start > 0 && !region_last_eol.is_empty() {
        // 上一行原本没有换行（它是文件末行且无结尾换行）时，现在后面还有内容，
        // 必须补一个；否则保持它自己的换行风格即可。
        if lines[region.start - 1].1.is_empty() {
            lines[region.start - 1].1 = region_last_eol;
        }
    }
    let joined = join_keep_eol(&lines);
    // 删完可能留下连续空行（区域里原来有空行、下一段前面也有一个）：压成一个，
    // 与 `splice_server` 的口径一致。
    let joined = collapse_newlines(&joined);
    // 参考实现的既有契约：删完**总是**以恰好一个换行结尾（原文即使不带结尾换行，
    // 结果也会补一个）。`remove_drops_the_region_and_absent_is_a_no_op` 把这条钉死了。
    // 同时这也顺手带走了删最后一节时残留的分隔空行 —— 加一次、删一次回到原文。
    let body = joined.trim_end_matches(['\r', '\n']);
    if body.is_empty() {
        return String::new();
    }
    format!("{}{}", body, dominant_eol(raw))
}

/* ------------------------------------------------------------------ */
/* 差异                                                                 */
/* ------------------------------------------------------------------ */

/// `lineDiff(a, b)` 产出 `[{t:'mod',n,old,next} | {t:'del',n,old} | {t:'add',n,next}]`，
/// 截断到 400 条。
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
        // 夹在两张表之间的注释属于它*前面*那个区域（参考实现的区域在下一行
        // 表头处就结束了，不管中间放的是什么），所以 "# between" 会跟着 alpha
        // 一起消失；"# tail comment" 仍然属于 beta，不会被碰。
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
        // 段落不混写：`stringify` 产出的是 LF，但目标文件是 CRLF，所以写进去的
        // 每一行都必须是 CRLF —— 混写会让用户的 diff 显示整段都变了。
        let out = splice_server(raw, "a", "[mcp_servers.a]\ncommand = \"y\"\n");
        assert!(out.starts_with("# top\r\n[mcp_servers.a]\r\ncommand = \"y\""), "{}", out.escape_debug());
        assert!(!out.replace("\r\n", "").contains('\n'), "no bare LF is introduced");

        let removed = remove_server(raw, "a");
        assert_eq!(removed, "# top\r\n");
    }

    /// **段落之外的行，连同它自己的换行符，一个字节都不许动。**
    ///
    /// 这是 §18.2 那轮修复没覆盖到的半边：当时只保证了「段落内不混写」。一份手工
    /// 维护、CRLF 与 LF 混存的 TOML（Windows 编辑器 + 脚本各写各的，很常见），
    /// 只要动一条服务器，旧实现会把**全文**行尾统一成一种风格。
    #[test]
    fn lines_outside_the_region_keep_their_own_newlines() {
        let raw = "# head\r\nmodel = \"x\"\n\n[mcp_servers.a]\ncommand = \"old\"\n";
        let out = splice_server(raw, "a", "[mcp_servers.a]\ncommand = \"new\"\n");
        // `# head` 仍是 CRLF，`model` 仍是 LF（一个字节都不能被顺手改成 CRLF）。
        assert!(out.starts_with("# head\r\nmodel = \"x\"\n\n"), "{}", out.escape_debug());
        // 被替换的那一段沿用**它自己**原来的换行（这里是 LF），而不是文件里第一次
        // 出现的 CRLF —— 否则 diff 会显示出与「改了一条服务器」无关的行。
        assert_eq!(
            out,
            "# head\r\nmodel = \"x\"\n\n[mcp_servers.a]\ncommand = \"new\"\n"
        );
    }

    /// 开启再关闭必须**逐字节**回到原文（§5.5 验收 1/2）。
    ///
    /// 旧实现删掉最后一节时会留下一个尾随空行，于是「开一次、关一次」永久多一行；
    /// 删中间一节时则会把后面的空行一起吃掉。
    #[test]
    fn enable_then_disable_round_trips_byte_for_byte() {
        // 尾部小节
        let lf = "[mcp_servers.a]\ncommand = \"x\"\n";
        let on = splice_server(lf, "b", "[mcp_servers.b]\ncommand = \"y\"\n");
        assert_eq!(remove_server(&on, "b"), lf);

        // 中间小节：`[a]` 后面那个空行属于 `[a]`，删 `[a]` 时它跟着走，`[b]` 前面的
        // 结构保持不变。
        let mid = "# about alpha\n[mcp_servers.a]\ncommand = \"x\"\n\n[mcp_servers.b]\ncommand = \"y\"\n";
        assert_eq!(
            remove_server(mid, "a"),
            "# about alpha\n\n[mcp_servers.b]\ncommand = \"y\"\n"
        );

        // CRLF 文件同样逐字节
        let crlf = "# my config\r\n\r\n[mcp_servers.other]\r\ncommand = \"z\"\r\n";
        let on = splice_server(crlf, "github", "[mcp_servers.github]\ncommand = \"gh\"\n");
        assert_eq!(remove_server(&on, "github"), crlf);
    }

    #[test]
    fn collapse_newlines_collapses_three_or_more() {
        let raw = "[mcp_servers.a]\ncommand = \"x\"\n[other]\n\n\n\nk = 1\n";
        let out = splice_server(raw, "a", "[mcp_servers.a]\ncommand = \"y\"\n");
        assert_eq!(out, "[mcp_servers.a]\ncommand = \"y\"\n[other]\n\nk = 1\n");
        // 两个换行还是两个；只有三个及以上才会被折叠。
        assert_eq!(collapse_newlines("a\n\nb"), "a\n\nb");
        assert_eq!(collapse_newlines("a\n\n\nb"), "a\n\nb");
        assert_eq!(collapse_newlines("a\n\n\n\n\nb"), "a\n\nb");
        assert_eq!(collapse_newlines("a\r\n\r\n\r\nb"), "a\r\n\r\n\r\nb");
    }

    #[test]
    fn quoted_header_and_key_forms_are_found() {
        let raw = "[\"mcp_servers\".'a.b']\ncommand = \"x\"\n\n[other]\nk = 1\n";
        let out = splice_server(raw, "a.b", "[\"mcp_servers\".'a.b']\ncommand = \"y\"\n");
        assert_eq!(
            out,
            "[\"mcp_servers\".'a.b']\ncommand = \"y\"\n[other]\nk = 1\n"
        );
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
        assert_eq!(
            d,
            vec![
                json!({"t": "mod", "n": 2, "old": "l2", "next": "L2"}),
                json!({"t": "add", "n": 4, "next": "l4"}),
            ]
        );

        let d = line_diff("l1\nl2\nl3", "l1");
        assert_eq!(
            d,
            vec![
                json!({"t": "del", "n": 2, "old": "l2"}),
                json!({"t": "del", "n": 3, "old": "l3"}),
            ]
        );

        let d = line_diff("a\r\nb", "a\r\nc");
        assert_eq!(
            d,
            vec![json!({"t": "mod", "n": 2, "old": "b", "next": "c"})]
        );

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
