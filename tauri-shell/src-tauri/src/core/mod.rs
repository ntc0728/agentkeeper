//! AgentKeeper Rust core.
//!
//! Deliberately free of any Tauri/UI dependency: the same code backs the
//! desktop command and the headless `ak-scan` binary used for parity checks
//! against the JavaScript modules in `core/`. Two implementations of the same
//! logic must never exist side by side — the invariants would drift silently.

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
/* JS `String.prototype.trim()`                                        */
/* ------------------------------------------------------------------ */

/// `String.prototype.trim()`'s whitespace class, i.e. ECMAScript
/// `WhiteSpace` + `LineTerminator` + U+FEFF.
///
/// It is *not* Unicode `White_Space`, which is what Rust's `str::trim` uses;
/// the two disagree at both edges — Rust strips U+0085 (JS keeps it) and keeps
/// U+FEFF (JS strips it). Every module that mirrors a JS `.trim()` must call
/// this one definition: the update/validate/toml paths each used to carry a
/// private copy, which is exactly the fork-then-drift shape this project has
/// already been burned by.
pub fn is_ecma_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'  // TAB, LF, VT, FF, CR
        | '\u{0020}'             // SPACE
        | '\u{00A0}'             // NBSP
        | '\u{1680}'             // OGHAM SPACE MARK
        | '\u{2000}'..='\u{200A}' // EN QUAD .. HAIR SPACE
        | '\u{2028}'             // LINE SEPARATOR
        | '\u{2029}'             // PARAGRAPH SEPARATOR
        | '\u{202F}'             // NARROW NBSP
        | '\u{205F}'             // MEDIUM MATHEMATICAL SPACE
        | '\u{3000}'             // IDEOGRAPHIC SPACE
        | '\u{FEFF}'             // ZWNBSP / BOM
    )
}

/// `s.trim()` — see [`is_ecma_whitespace`].
pub fn ecma_trim(s: &str) -> &str {
    s.trim_matches(is_ecma_whitespace)
}

/// `s.trimEnd()` — see [`is_ecma_whitespace`].
pub fn ecma_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_ecma_whitespace)
}

#[cfg(test)]
mod trim_tests {
    use super::{ecma_trim, ecma_trim_end};

    /// The two code points where Rust's `str::trim` and JS disagree.
    #[test]
    fn matches_js_not_rust() {
        // U+FEFF: JS trims it, Rust's `str::trim` does not.
        assert_eq!(ecma_trim("\u{feff}x\u{feff}"), "x");
        assert_ne!("\u{feff}x\u{feff}".trim(), "x");
        // U+0085 (NEL): Rust trims it, JS does not.
        assert_eq!(ecma_trim("\u{85}x\u{85}"), "\u{85}x\u{85}");
        assert_eq!("\u{85}x\u{85}".trim(), "x");
    }

    #[test]
    fn trims_the_ecma_set() {
        // WhiteSpace + LineTerminator, both ends.
        for c in [
            '\u{09}', '\u{0a}', '\u{0b}', '\u{0c}', '\u{0d}', '\u{20}', '\u{a0}', '\u{1680}',
            '\u{2000}', '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}',
            '\u{feff}',
        ] {
            let s = format!("{c}x{c}");
            assert_eq!(ecma_trim(&s), "x", "U+{:04X} must trim", c as u32);
        }
        // Neither U+0085 nor the zero-width non-joiners are JS whitespace.
        for c in ['\u{85}', '\u{180e}', '\u{200b}', '\u{200c}', '\u{200d}'] {
            let s = format!("{c}x{c}");
            assert_eq!(ecma_trim(&s), s.as_str(), "U+{:04X} must not trim", c as u32);
        }
    }

    #[test]
    fn leaves_the_middle_alone() {
        assert_eq!(ecma_trim(""), "");
        assert_eq!(ecma_trim("   "), "");
        assert_eq!(ecma_trim("a b"), "a b");
        assert_eq!(ecma_trim(" a b "), "a b");
        assert_eq!(ecma_trim("\u{feff} \t\u{a0}\u{3000}hello world \u{2028}\u{feff}"), "hello world");
    }

    #[test]
    fn trim_end_only_touches_the_tail() {
        assert_eq!(ecma_trim_end("  x  "), "  x");
        assert_eq!(ecma_trim_end("\u{feff}x\u{feff}"), "\u{feff}x");
        // U+0085 is not JS whitespace, so it survives at the tail too.
        assert_eq!(ecma_trim_end("x\u{85}"), "x\u{85}");
    }
}
