//! AgentKeeper 的 Rust 内核。
//!
//! 刻意不依赖任何 Tauri/UI：同一份代码同时支撑桌面端命令和无界面的 `ak-scan`
//! 二进制。同一套逻辑的两份实现绝不能并存——不变量会悄无声息地漂移。
//!
//! 它自己独立成一个 crate 而不是挨着 Tauri 外壳，就是为了让这条保证由编译器
//! 来强制执行，而不是靠约定：这里连意外碰到 Tauri 类型都不可能，因为那个依赖
//! 根本不存在。外壳依赖本 crate；本 crate 谁也不依赖。

pub mod adapters;
pub mod assets;
pub mod discover;
pub mod frontmatter;
pub mod http;
pub mod jsonc;
pub mod keeper;
pub mod mcp;
pub mod model;
pub mod registry;
pub mod scan;
pub mod security;
pub mod settings;
pub mod store;
pub mod toml_splice;
pub mod update;
pub mod validate;
pub mod yaml;

/* ------------------------------------------------------------------ */
/* JS 的 `String.prototype.trim()`                                    */
/* ------------------------------------------------------------------ */

/// `String.prototype.trim()` 的空白字符类，即 ECMAScript 的
/// `WhiteSpace` + `LineTerminator` + U+FEFF 三部分之和。
///
/// 它*不是* Unicode 的 `White_Space`——Rust 的 `str::trim` 用的才是那个；
/// 两者在两个边界上判断相反：Rust 会去掉 U+0085（JS 保留它），却保留
/// U+FEFF（JS 去掉它）。凡是模仿 JS `.trim()` 的模块都调用这唯一一份
/// 定义——update/validate/toml 几条路径共用它，而不是各自拷一份，
/// 那正是"先分叉再漂移"、必须避免的形态。
pub fn is_ecma_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'  // TAB、LF、VT、FF、CR
        | '\u{0020}'             // 空格
        | '\u{00A0}'             // NBSP（不换行空格）
        | '\u{1680}'             // 欧甘文空格
        | '\u{2000}'..='\u{200A}' // EN QUAD .. HAIR SPACE（各种宽度的空格）
        | '\u{2028}'             // 行分隔符
        | '\u{2029}'             // 段分隔符
        | '\u{202F}'             // 窄 NBSP
        | '\u{205F}'             // 中数学空格
        | '\u{3000}'             // 全角空格
        | '\u{FEFF}'             // ZWNBSP / BOM
    )
}

/// `s.trim()`——见 [`is_ecma_whitespace`]。
pub fn ecma_trim(s: &str) -> &str {
    s.trim_matches(is_ecma_whitespace)
}

/// `s.trimEnd()`——见 [`is_ecma_whitespace`]。
pub fn ecma_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_ecma_whitespace)
}

#[cfg(test)]
mod trim_tests {
    use super::{ecma_trim, ecma_trim_end};

    /// Rust 的 `str::trim` 与 JS 判断不一致的那两个码点。
    #[test]
    fn trims_the_ecma_set_not_rusts_whitespace_set() {
        // U+FEFF：JS 会 trim 掉，Rust 的 `str::trim` 不会。
        assert_eq!(ecma_trim("\u{feff}x\u{feff}"), "x");
        assert_ne!("\u{feff}x\u{feff}".trim(), "x");
        // U+0085（NEL）：Rust 会 trim 掉，JS 不会。
        assert_eq!(ecma_trim("\u{85}x\u{85}"), "\u{85}x\u{85}");
        assert_eq!("\u{85}x\u{85}".trim(), "x");
    }

    #[test]
    fn trims_the_ecma_set() {
        // WhiteSpace + LineTerminator，两端都要 trim。
        for c in [
            '\u{09}', '\u{0a}', '\u{0b}', '\u{0c}', '\u{0d}', '\u{20}', '\u{a0}', '\u{1680}',
            '\u{2000}', '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}',
            '\u{feff}',
        ] {
            let s = format!("{c}x{c}");
            assert_eq!(ecma_trim(&s), "x", "U+{:04X} must trim", c as u32);
        }
        // U+0085 和零宽不连字符都不是 JS 的空白字符。
        for c in ['\u{85}', '\u{180e}', '\u{200b}', '\u{200c}', '\u{200d}'] {
            let s = format!("{c}x{c}");
            assert_eq!(
                ecma_trim(&s),
                s.as_str(),
                "U+{:04X} must not trim",
                c as u32
            );
        }
    }

    #[test]
    fn leaves_the_middle_alone() {
        assert_eq!(ecma_trim(""), "");
        assert_eq!(ecma_trim("   "), "");
        assert_eq!(ecma_trim("a b"), "a b");
        assert_eq!(ecma_trim(" a b "), "a b");
        assert_eq!(
            ecma_trim("\u{feff} \t\u{a0}\u{3000}hello world \u{2028}\u{feff}"),
            "hello world"
        );
    }

    #[test]
    fn trim_end_only_touches_the_tail() {
        assert_eq!(ecma_trim_end("  x  "), "  x");
        assert_eq!(ecma_trim_end("\u{feff}x\u{feff}"), "\u{feff}x");
        // U+0085 不是 JS 的空白字符，所以它在尾部也照样保留。
        assert_eq!(ecma_trim_end("x\u{85}"), "x\u{85}");
    }
}
