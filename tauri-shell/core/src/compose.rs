//! 规则文件的区块标记（M4）—— 把「一段受管内容」放进用户自己的 markdown 里。
//!
//! 共享规则有两种形态（`docs/SHARED-ASSETS-IMPLEMENTATION.md` §6.2）：
//!
//! | 模式 | 盘上是什么 | 谁写它 |
//! |---|---|---|
//! | `link` | 一条指向库内主副本的链接 | 事务（`Op::link_file`） |
//! | `compose` | agent 自己的文件里多出一个带标记的区块 | 本模块算出来的整份文本 |
//!
//! `link` 之外还要有 `compose`，是因为好几个 agent 的规则文件是**用户自己手写
//! 的**（`~/.claude/CLAUDE.md`、`.cursorrules`……），把它换成链接等于把用户的
//! 文件搬走；而 `compose` 只往里面插一个区块，区块之外一个字节都不动。
//!
//! ## 标记形态（固定，不要在实现时改格式）
//!
//! 下面是目标文件里会被插进去的三行（markdown 包围盒），`begin` 行带 `slug` 与 `rev`：
//! <!-- agentkeeper:begin slug=team-conventions rev=9f2c1d -->   ← 区块开始，rev 是内容的指纹
//! …内容…
//! <!-- agentkeeper:end slug=team-conventions -->   ← 区块结束，只带 slug
//!
//! `rev` 是内容 SHA-256 的前 6 位（`model::hash16` 截断），写在这里是为了下次
//! 写回时能回答一个问题：**盘上这一份还是我们上次写的那份吗**。用户手动改过
//! 区块内的内容，`rev` 就对不上，此时写回就是静默覆盖用户的编辑 —— 所以
//! 调用方要先过 [`is_foreign_edit`] 这道闸（`compose_plan` 已经代办了）。
//!
//! ## 三条不变式（每一条都有单测钉着）
//!
//! 1. **标记之外逐字节不动**。本模块只做字符串拼接，从不读写文件；换行风格按
//!    目标文件检测（`\r\n` 与 `\n`），不混写。
//! 2. **幂等**。同一 `(target, slug, body)` 跑两遍，第二遍结果与第一遍逐字节相等。
//! 3. **认不出的标记一律当普通文本**。只有成对、slug 合法、`begin` 在 `end`
//!    之前的三元组才算一个区块；少一个都不算，因为「把用户的内容当成我们的区块
//!    删掉」是不可撤销的。

use crate::model::hash16;

/// `rev` 的长度：`hash16` 的前 6 个十六进制字符。
const REV_LEN: usize = 6;

/// 标记前缀。拼成完整标记时用 `format!`，这里只留判据。
const MARK: &str = "agentkeeper:";

/// 一个解析出来的区块。
///
/// 起止位置是**整份文本**里的字节偏移，并且包含标记行本身：`start` 指向
/// `<!-- agentkeeper:begin` 的 `<`，`end` 指向 `end` 标记行结束之后的那个
/// 字节（也就是下一行的开头）。所以 `target[start..end]` 能被整体替换或删掉，
/// 而不需要在别处补换行。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Block {
    /// 区块标识（`slug=...` 的值）。
    pub slug: String,
    /// 标记行里写着的 `rev`（6 位十六进制）。
    pub rev: String,
    /// 区块内容：`begin` 行的换行之后、`end` 行之前，**不含**两侧标记行。
    pub body: String,
    pub start: usize,
    pub end: usize,
}

/// 内容的 `rev`：`hash16` 的前 6 位（`model.rs::hash16`）。
pub fn rev_of(body: &str) -> String {
    hash16(body)[..REV_LEN].to_string()
}

/// slug 的合法字符集：`[A-Za-z0-9._-]`，非空，长度 ≤ 64。
///
/// 收紧是必须的：slug 会原样写进标记行，允许空格就会让「解析」变成猜
/// （`slug=a b` 到哪里结束？）；允许 `>` 就能在标记行里造出假内容。
pub fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// 目标文件用的换行风格：文件里**第一次出现**的换行；一个换行都没有时是 LF。
///
/// 为什么不用「出现过 CRLF 就算 CRLF」：一份存量 LF 的规则文件里只要有一行是
/// CRLF（任何 Windows 编辑器都会手打出来），旧判据就会把整个区块写成 CRLF。
/// 而区块内容每次都按 `eol_of` 归一，于是 `next != raw` 永远成立 —— 每次同步都
/// 写一次盘，幂等性没了（§11.9 点名的就是混写）。取「第一个换行」对纯 LF 与
/// 纯 CRLF 两种文件的结果与旧行为一致，只把混写这一种拉回中性。
///
/// 判据与 `toml_splice.rs::dominant_eol` 同一口径。
pub fn eol_of(target: &str) -> &'static str {
    match target.as_bytes().iter().position(|b| *b == b'\n') {
        Some(i) if i > 0 && target.as_bytes()[i - 1] == b'\r' => "\r\n",
        Some(_) => "\n",
        None => "\n",
    }
}

/// 一行的 `(起点, 内容不含换行, 行尾换行)`。
///
/// 手写而不是 `lines()`：需要保留每行自己的换行符 —— 一份混写文件里 CRLF 行与
/// LF 行的「原样」是两个不同的字节序列，`lines()` 会把两者都抹成一样。
fn lines_of(target: &str) -> Vec<(usize, &str, &str)> {
    let bytes = target.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                // 前面若是 `\r`，它属于换行（除非它自己就是行首，那是空行 + CRLF）。
                let (body_end, next) = if i > start && bytes[i - 1] == b'\r' {
                    (i - 1, i + 1)
                } else {
                    (i, i + 1)
                };
                out.push((start, &target[start..body_end], &target[body_end..next]));
                start = next;
                i = next;
            }
            _ => i += 1,
        }
    }
    if start < target.len() {
        out.push((start, &target[start..], ""));
    }
    out
}

/// 标记行解析出来的东西。
enum Mark {
    Begin { slug: String, rev: String },
    End { slug: String },
}

/// 判一行是不是区块标记；不是（或形状不完整）返回 `None`。
///
/// 只认整行标记：`<!-- agentkeeper:begin ... -->` 前后只允许空白。宽松匹配会把
/// 「用户在自己的注释里引用了这个格式」也当成区块。
fn parse_mark(line: &str) -> Option<Mark> {
    let t = line.trim();
    let inner = t.strip_prefix("<!--")?.strip_suffix("-->")?.trim();
    let rest = inner.strip_prefix(MARK)?;
    let (head, attrs) = match rest.split_once(char::is_whitespace) {
        Some((h, a)) => (h, a.trim()),
        None => (rest, ""),
    };
    let mut slug = None;
    let mut rev = None;
    for attr in attrs.split_whitespace() {
        if let Some(v) = attr.strip_prefix("slug=") {
            slug = Some(v.to_string());
        } else if let Some(v) = attr.strip_prefix("rev=") {
            rev = Some(v.to_string());
        }
    }
    let slug = slug?;
    if !is_valid_slug(&slug) {
        return None;
    }
    match head {
        "begin" => rev.map(|rev| Mark::Begin { slug, rev }),
        // `end` 行不带 `rev`（内容要等读到 end 才知道），带上也不影响解析：
        // 写了 `rev` 的 end 行仍然只按 slug 配对，多出来的属性不参与判定。
        "end" => Some(Mark::End { slug }),
        _ => None,
    }
}

/// 解析出所有区块：按出现顺序，`slug` 重复时保留**第一个**完整的三元组。
///
/// 「完整」= `begin` 与 `end` 成对且 slug 相同；嵌套或错配的 `begin` 会让本次
/// 扫描作废（跳到下一个 `begin` 重新开始），因为把错配的两端当成一个区块，
/// 替换时就会吃掉中间那一段用户内容。
pub fn blocks(target: &str) -> Vec<Block> {
    let lines = lines_of(target);
    // `begin` 候选：(行号, 起点偏移, slug, rev, 内容起点)
    let mut out: Vec<Block> = Vec::new();
    let mut open: Option<(usize, usize, String, String)> = None;
    for (idx, (start, body, nl)) in lines.iter().enumerate() {
        match parse_mark(body) {
            Some(Mark::Begin { slug, rev }) => {
                // 上一个 begin 没等到 end：直接丢弃（它不是一个区块）。
                open = Some((idx, *start, slug, rev));
            }
            Some(Mark::End { slug }) => {
                let Some((begin_idx, begin_start, begin_slug, begin_rev)) = open.take() else {
                    continue;
                };
                if begin_slug != slug {
                    continue;
                }
                let content_start = begin_start + lines[begin_idx].1.len() + lines[begin_idx].2.len();
                let content_end = *start;
                if content_end < content_start {
                    continue;
                }
                let body_text = &target[content_start..content_end];
                let block_end = start + body.len() + nl.len();
                if out.iter().any(|b| b.slug == slug) {
                    continue; // 同 slug 只认第一个
                }
                out.push(Block {
                    slug,
                    rev: begin_rev,
                    body: body_text.to_string(),
                    start: begin_start,
                    end: block_end,
                });
            }
            None => {}
        }
    }
    out
}

/// 某个 slug 的区块。
pub fn find_block(target: &str, slug: &str) -> Option<Block> {
    blocks(target).into_iter().find(|b| b.slug == slug)
}

/// 拼出 `begin` / `end` 两行（含各自的换行）。
fn marks(slug: &str, rev: &str, eol: &str) -> (String, String) {
    (
        format!("<!-- {}begin slug={} rev={} -->{}", MARK, slug, rev, eol),
        format!("<!-- {}end slug={} -->{}", MARK, slug, eol),
    )
}

/// 把 `body` 作为一个区块放进 `target`：已有同名区块则替换，否则追加到末尾。
///
/// 标记之外的内容**逐字节不动**。`target` 非空且末尾没有换行时先补一个，否则
/// 追加的 `begin` 行会接在用户最后一行后面，`parse_mark` 认不出它（那是整行标记）。
///
/// `slug` 非法时**原样返回 `target`**：写不出一个能被自己解析回来的区块，就不写。
pub fn upsert_block(target: &str, slug: &str, body: &str) -> String {
    if !is_valid_slug(slug) {
        return target.to_string();
    }
    let eol = eol_of(target);
    // 内容归一成目标文件的换行风格：区块内容来自库内主副本，它可能是 LF，
    // 而目标文件是 CRLF。混写是 §11.9 点名的坑，在这里一次性挡住。
    let body = normalize_eol(body, eol);
    let rev = rev_of(&body);
    let (begin_line, end_line) = marks(slug, &rev, eol);
    let fresh = format!("{}{}{}{}", begin_line, body, end_line, "");
    // 区块内容要自带结尾换行，`end` 标记才亮在独立一行上。
    let region = if body.is_empty() || body.ends_with('\n') {
        fresh
    } else {
        format!("{begin_line}{body}{eol}{end_line}")
    };

    if let Some(b) = find_block(target, slug) {
        let mut out = String::with_capacity(target.len() + region.len());
        out.push_str(&target[..b.start]);
        out.push_str(&region);
        out.push_str(&target[b.end..]);
        return out;
    }

    let mut out = String::with_capacity(target.len() + region.len() + 2);
    out.push_str(target);
    if !target.is_empty() && !target.ends_with('\n') {
        out.push_str(eol);
    }
    out.push_str(&region);
    out
}

/// 把文本里的换行统一成 `eol`。
///
/// 只做「LF → 目标换行」：先把已有的 `\r\n` 折叠成 `\n` 再展开，避免出现
/// `\r\r\n`（拿 LF 切一份 CRLF 文本再拼回去就会多出这个）。
fn normalize_eol(s: &str, eol: &str) -> String {
    let lf = s.replace("\r\n", "\n");
    if eol == "\n" {
        lf
    } else {
        lf.replace('\n', eol)
    }
}

/// 移除指定区块；区块不存在时原样返回。
///
/// 删除粒度是**整个区块**（含两侧标记行）。区块之后的那一行不动：`block.end`
/// 已经落在 `end` 标记行的换行之后，所以删掉之后前后自然接上，不会留空行。
pub fn remove_block(target: &str, slug: &str) -> String {
    let Some(b) = find_block(target, slug) else {
        return target.to_string();
    };
    let mut out = String::with_capacity(target.len());
    out.push_str(&target[..b.start]);
    out.push_str(&target[b.end..]);
    out
}

/// 盘上区块内容是否与「上次生成时」不一致 —— 不一致就是被外部改过。
///
/// 三种情形都算「被改过」，因为三种情形下写回都会丢掉用户手上的东西：
///
/// 1. 区块不见了（用户删掉了它）；
/// 2. 标记行的 `rev` 与内容的实际 `rev` 对不上（用户改了区块内）；
/// 3. 标记行的 `rev` 与 `last_generated` 对不上（我们上次写的不是这一份）。
pub fn is_foreign_edit(target: &str, slug: &str, last_generated: &str) -> bool {
    let Some(b) = find_block(target, slug) else {
        return true;
    };
    let want = rev_of(last_generated);
    b.rev != rev_of(&b.body) || b.rev != want
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLUG: &str = "team-conventions";

    fn wrap(target: &str, slug: &str) -> String {
        let b = find_block(target, slug).expect("block must exist");
        target[b.start..b.end].to_string()
    }

    /// 标记形态固定：一行 begin、内容、一行 end，`rev` 是内容前 6 位。
    #[test]
    fn the_mark_shape_is_exactly_as_documented() {
        let out = upsert_block("", SLUG, "be kind\n");
        let rev = rev_of("be kind\n");
        assert_eq!(rev.len(), 6);
        assert_eq!(
            out,
            format!(
                "<!-- agentkeeper:begin slug={} rev={} -->\nbe kind\n<!-- agentkeeper:end slug={} -->\n",
                SLUG, rev, SLUG
            )
        );
    }

    /// 幂等：同一 (target, slug, body) 跑两遍，第二遍逐字节等于第一遍。
    #[test]
    fn upsert_is_idempotent_for_every_eol_style() {
        for (name, head) in [("lf", "# mine\n"), ("crlf", "# mine\r\n")] {
            let once = upsert_block(head, SLUG, "line one\nline two\n");
            let twice = upsert_block(&once, SLUG, "line one\nline two\n");
            assert_eq!(once, twice, "{}: second run changed bytes", name);
            assert_eq!(blocks(&once).len(), 1, "{}: exactly one block", name);
        }
    }

    /// 标记之外的内容逐字节不动 —— 前缀与后缀都要保住。
    #[test]
    fn bytes_outside_the_block_are_untouched() {
        let before = "# Title\n\nmy own words\n";
        let after = "\n## Later\nmy own tail\n";
        let target = format!("{}{}{}", before, upsert_block("", SLUG, "first\n"), after);
        let out = upsert_block(&target, SLUG, "second\n");
        assert!(out.starts_with(before), "prefix changed: {:?}", out);
        assert!(out.ends_with(after), "suffix changed: {:?}", out);
        // 中间只有那一个区块，且内容换成了新的。
        let region = wrap(&out, SLUG);
        assert!(region.contains("second"), "{:?}", region);
        assert!(!region.contains("first"), "{:?}", region);
    }

    /// 替换发生在文件中间时，区块前后的原文一字不差。
    #[test]
    fn replacing_a_middle_block_leaves_the_neighbours_alone() {
        let target = "top\n\nmiddle text\nbottom\n";
        let with = upsert_block(target, SLUG, "body\n");
        let out = upsert_block(&with, SLUG, "body2\n");
        let b = find_block(&out, SLUG).unwrap();
        assert_eq!(&out[..b.start], target);
        assert_eq!(&out[b.end..], "");
    }

    /// 目标文件末尾没有换行时，追加的区块仍然成行（否则解析不回来）。
    #[test]
    fn appending_to_a_file_without_a_trailing_newline_still_parses() {
        let out = upsert_block("# no newline at eof", SLUG, "x\n");
        assert!(out.starts_with("# no newline at eof\n"), "{:?}", out);
        assert_eq!(blocks(&out).len(), 1);
        assert_eq!(find_block(&out, SLUG).unwrap().body, "x\n");
    }

    /// 删除区块后，文件回到「没有区块」的样子（不含标记行）。
    #[test]
    fn remove_block_restores_the_original_text() {
        let original = "# mine\n\nkeep this\n";
        let with = upsert_block(original, SLUG, "managed\n");
        assert_eq!(remove_block(&with, SLUG), original);
        // 不存在的区块：原样返回（不是空串，也不是删掉别的东西）。
        assert_eq!(remove_block(original, SLUG), original);
        assert_eq!(remove_block(original, "other-slug"), original);
    }

    /// 两个不同 slug 的区块可以共存，各删各的。
    #[test]
    fn blocks_are_independent_by_slug() {
        let a = upsert_block("# t\n", "alpha", "A\n");
        let b = upsert_block(&a, "beta", "B\n");
        assert_eq!(blocks(&b).len(), 2);
        let only_a = remove_block(&b, "beta");
        assert_eq!(only_a, a);
        assert_eq!(blocks(&only_a).len(), 1);
        assert_eq!(blocks(&only_a)[0].slug, "alpha");
    }

    /// 重复写同一个 slug：仍然只有一个区块（就地替换，不追加）。
    #[test]
    fn a_repeated_slug_never_duplicates_the_block() {
        let mut t = "# t\n".to_string();
        for i in 0..3 {
            t = upsert_block(&t, SLUG, &format!("run {}\n", i));
        }
        assert_eq!(blocks(&t).len(), 1);
        assert_eq!(t.matches("agentkeeper:begin").count(), 1);
    }

    /// CRLF 目标文件 compose 后仍是纯 CRLF：不出现裸 LF，也不出现 `\r\r\n`。
    #[test]
    fn a_crlf_target_stays_pure_crlf() {
        let target = "# Title\r\n\r\nmy words\r\n";
        let out = upsert_block(target, SLUG, "line one\nline two\n");
        // 裸 LF 检查：每个 `\n` 前面都必须是 `\r`。
        let bytes = out.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                assert!(i > 0 && bytes[i - 1] == b'\r', "bare LF at {}", i);
            }
        }
        assert!(!out.contains("\r\r\n"), "double CR: {:?}", out);
        // 前缀逐字节不动。
        assert!(out.starts_with(target));
        // 内容里的 LF 被换成了 CRLF。
        assert!(out.contains("line one\r\nline two\r\n"));
    }

    /// LF 目标文件里出现 CRLF 内容时被归一成 LF（反方向同样不混写）。
    #[test]
    fn an_lf_target_never_gains_a_crlf_line() {
        let out = upsert_block("# t\n", SLUG, "a\r\nb\r\n");
        assert!(!out.contains('\r'), "{:?}", out);
        assert_eq!(find_block(&out, SLUG).unwrap().body, "a\nb\n");
    }

    /// 存量 LF 文件里用户手打了一行 CRLF（任何 Windows 编辑器都会）→ 区块仍写 LF。
    ///
    /// 旧判据「出现过 CRLF 就算 CRLF」会把这个文件的新区块整段写成 CRLF，于是
    /// 每次同步 `next != raw`：rev 随换行漂移、盘上每次都有写入，幂等没了。
    #[test]
    fn one_stray_crlf_line_does_not_flip_the_whole_block() {
        let mixed = "line1\nline2\r\nline3\n";
        let out = upsert_block(mixed, SLUG, "x");
        assert!(out.contains("<!-- agentkeeper:begin"), "{:?}", out);
        // 区块两行标记都以 LF 结尾（不是 CRLF）。
        for line in out
            .split_inclusive('\n')
            .filter(|l| l.trim_start().starts_with("<!-- agentkeeper:"))
        {
            assert!(line.ends_with("-->\n"), "mark line must end with LF: {line:?}");
        }
        // 用户自己写的那一行 CRLF 一个字节没动。
        assert!(out.contains("line2\r\n"), "{:?}", out);
        // 再 upsert 一次必须完全不动（幂等）。
        assert_eq!(upsert_block(&out, SLUG, "x"), out, "not idempotent");
    }

    /// 空内容也能写出一个合法区块（只有两行标记）。
    #[test]
    fn an_empty_body_round_trips() {
        let out = upsert_block("# t\n", SLUG, "");
        let b = find_block(&out, SLUG).expect("block");
        assert_eq!(b.body, "");
        assert_eq!(blocks(&out).len(), 1);
        assert_eq!(remove_block(&out, SLUG), "# t\n");
    }

    /// 内容不以换行结尾时，`end` 标记仍然独占一行。
    #[test]
    fn a_body_without_a_trailing_newline_does_not_swallow_the_end_mark() {
        let out = upsert_block("", SLUG, "no newline");
        assert_eq!(blocks(&out).len(), 1, "{:?}", out);
        assert!(out.contains("no newline\n<!-- agentkeeper:end"), "{:?}", out);
    }

    /// 用户在区块内改了一行 → 外部改动为真。
    #[test]
    fn editing_inside_the_block_is_a_foreign_edit() {
        let last_generated = "keep me\n";
        let target = upsert_block("", SLUG, last_generated);
        assert!(!is_foreign_edit(&target, SLUG, last_generated));
        let hand_edited = target.replace("keep me", "keep you");
        assert!(is_foreign_edit(&hand_edited, SLUG, last_generated));
    }

    /// 区块被删掉 → 同样是外部改动（写回会把这些东西凭空塞回去）。
    #[test]
    fn a_missing_block_counts_as_a_foreign_edit() {
        assert!(is_foreign_edit("# mine\n", SLUG, "body\n"));
    }

    /// 有人改了标记行的 `rev` 让它看上去「对上了」→ 仍然判为外部改动。
    #[test]
    fn a_hand_patched_rev_is_still_a_foreign_edit() {
        let target = upsert_block("", SLUG, "body\n");
        let fixed = target.replace(&rev_of("body\n"), &rev_of("other\n"));
        assert!(is_foreign_edit(&fixed, SLUG, "body\n"));
    }

    /// 用户只改区块**外**的内容，不算外部改动（那是我们承诺不碰的地方）。
    #[test]
    fn edits_outside_the_block_are_not_foreign_edits() {
        let target = upsert_block("# t\n", SLUG, "body\n");
        let with_more = format!("{}extra line the user typed\n", target);
        assert!(!is_foreign_edit(&with_more, SLUG, "body\n"));
    }

    /// 非法 slug 不写（写出去解析不回来就不该写），合法字符集逐条验证。
    #[test]
    fn invalid_slugs_are_refused() {
        for bad in ["", "has space", "a>b", "斜杠/号", &"x".repeat(65)] {
            assert!(!is_valid_slug(bad), "{:?} should be invalid", bad);
            assert_eq!(upsert_block("# t\n", bad, "b\n"), "# t\n");
        }
        for good in ["a", "team-conventions", "a.b_c-1", &"x".repeat(64)] {
            assert!(is_valid_slug(good), "{:?} should be valid", good);
        }
    }

    /// 错配的标记（begin 的 slug 与 end 不同）不算区块 —— 不能把中间用户内容吃掉。
    #[test]
    fn mismatched_marks_are_not_a_block() {
        let t = "<!-- agentkeeper:begin slug=a rev=abc123 -->\nuser text\n<!-- agentkeeper:end slug=b -->\n";
        assert!(blocks(t).is_empty());
        // 也不会被「替换」或「删除」误伤。
        assert_eq!(remove_block(t, "a"), t);
        assert_eq!(upsert_block(t, "a", "new\n").matches("begin").count(), 2);
    }

    /// 只有 begin 没有 end：不算区块。
    #[test]
    fn an_unterminated_begin_is_not_a_block() {
        let t = "<!-- agentkeeper:begin slug=a rev=abc123 -->\nuser text\n";
        assert!(blocks(t).is_empty());
    }

    /// 标记不是整行（行首有正文）时不算标记 —— 那是用户在引用这个格式。
    #[test]
    fn an_inline_mention_is_not_a_mark() {
        let t = "text <!-- agentkeeper:begin slug=a rev=abc123 -->\nbody\n<!-- agentkeeper:end slug=a -->\n";
        assert!(blocks(t).is_empty());
    }

    /// 混写文件（既有 CRLF 也有 LF）不会被判成「整份 CRLF」而误伤已有行。
    #[test]
    fn a_mixed_file_keeps_its_own_layout_and_only_normalises_the_block() {
        let target = "a\r\nb\nc\r\n";
        let out = upsert_block(target, SLUG, "x\ny\n");
        assert!(out.starts_with(target), "{:?}", out);
        // 判据是「出现过 CRLF 就算 CRLF」，与 `toml_splice` 一致。
        assert!(out.contains("x\r\ny\r\n"), "{:?}", out);
    }

    /// 解析出的字节区间能原样切出区块（替换/删除都靠它）。
    #[test]
    fn block_offsets_cover_exactly_the_two_mark_lines_and_the_body() {
        let out = upsert_block("# top\n", SLUG, "body\n");
        let b = find_block(&out, SLUG).unwrap();
        let slice = &out[b.start..b.end];
        assert!(slice.starts_with("<!-- agentkeeper:begin"), "{:?}", slice);
        assert!(slice.ends_with("\n"), "{:?}", slice);
        assert!(slice.contains("<!-- agentkeeper:end"), "{:?}", slice);
        // 区间之外正好是文件里剩余的部分。
        assert_eq!(&out[..b.start], "# top\n");
        assert_eq!(&out[b.end..], "");
    }
}
