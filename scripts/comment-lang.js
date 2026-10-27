#!/usr/bin/env node
'use strict';
/**
 * 注释语言体检：中文化的进度尺。
 *
 *   node scripts/comment-lang.js            # 按文件列出「只有英文」的注释行
 *   node scripts/comment-lang.js --files    # 只打印文件清单
 *
 * 规则很简单：一行注释里只要有汉字就算中文；一个汉字都没有、却有三个以上连续字母
 * 的，就是还没翻译的英文注释。代码标识符、路径、`#fff` 这类不算句子，所以小写的
 * 标识符和纯符号行不计入。
 *
 * 为什么要有这个脚本：整仓的注释是几千行，一次性机器翻译会把技术含义译坏（尤其是
 * Rust 的 `///` 文档注释会被 doctest 编译）。先有尺子，再分批改，每批都能看到
 * 数字在下降。
 *
 * 退出码：0 = 有输出，2 = 用法错误。
 */

const fs = require('fs');
const path = require('path');

const ROOT = path.join(__dirname, '..');
/** 参与体检的源码。测试与构建脚本同样算：它们也是给人读的。 */
const PATTERNS = [
  'src/renderer/*.js',
  'scripts/*.js',
  'tauri-shell/*.js',
  'tauri-shell/src-tauri/build.rs',
  'tauri-shell/src-tauri/src/**/*.rs',
  'tauri-shell/src-tauri/tests/*.rs',
  'tauri-shell/core/src/**/*.rs',
  '.github/workflows/*.yml',
];
/** 注释行的起手式：`//`、`/*`、`*`、`///`、`//!`，以及 YAML 的 `#`。 */
const HAS_CJK = /[\u4e00-\u9fff]/u;
const HAS_WORD = /[A-Za-z]{3,}/;
/**
 * Rust 的 `#[test]` / `#[cfg(...)]` / `#![...]` 是属性，不是注释。
 *
 * 判据一开始只写了 `#`（为 YAML 注释），结果把集成测试里成片的 `#[test]` 全算成
 * 「没翻译的英文注释」—— 四个测试文件虚报 127 行、全仓虚报 432 行。尺子本身不准，
 * 覆盖率就没意义，所以这里先排除属性行。
 */
const RUST_ATTRIBUTE = /^#!?\[/;
/**
 * 块注释里以 `*` 开头、其实是代码续行的（`*slot = format!(…`、`*sink.lock()…`）。
 * 这些是解引用表达式，被 `*` 前缀误当成注释行。
 */
const BLOCK_CODE_LINE = /^\*(?:\w+\s*[.=[]|self\b)/;
/**
 * 命令行示例（`*   node scripts/x.js --flag`、`set VAR=1`、`cargo tauri build`）。
 * 注释里出现它们是让人照抄的，不是要翻译的句子。
 */
const COMMAND_EXAMPLE = /^[*#\/\s]*(?:node|npm|npx|cargo|rustup|set|export|powershell|\.\\|AK_[A-Z_]+=|tauri-shell\\|tauri-shell\/)\S*/i;
/**
 * 纯枚举/标识符行：`3. navigator.language`、`* common.*`、`// \`a\`\u3001\`b\`\u3002`。
 * 这些行的内容就是标识符本身，翻译它们等于改代码里的名字，只能保持原样。
 */
const IDENTIFIER_ONLY = /^[*#\/\s]*(?:\d+\.\s*)?[`'\w.*<>|/\\-]+[\u3000-\u303f\u4e00-\u9fff]*$/u;

function isCommentLine(s) {
  if (!s.startsWith('#') && !s.startsWith('//') && !s.startsWith('/*') && !s.startsWith('*')) return false;
  if (RUST_ATTRIBUTE.test(s)) return false;
  if (BLOCK_CODE_LINE.test(s)) return false;
  // shebang 与「命令行示例」不是散文：`#!/usr/bin/env node`、
  // `*   node scripts/x.js --flag`、`*   set VAR=1` 都是要照抄的命令。
  if (s.startsWith('#!')) return false;
  if (COMMAND_EXAMPLE.test(s)) return false;
  if (IDENTIFIER_ONLY.test(s)) return false;
  return true;
}

function walk(dir, out = []) {
  // `dist/` 是 copy-frontend.js 从 src/renderer 复制过去的产物，数它会把同一批
  // 注释算两遍。
  const skipDirs = new Set(['node_modules', 'target', 'dist']);
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (skipDirs.has(entry.name) || entry.name.startsWith('.')) continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(full, out);
    else out.push(full);
  }
  return out;
}

function filesFor(pattern) {
  const base = path.join(ROOT, path.dirname(pattern));
  const ext = path.basename(pattern);
  if (!fs.existsSync(base)) return [];
  const all = walk(base);
  if (ext === '*.js') return all.filter((f) => f.endsWith('.js'));
  if (ext === '*.rs') return all.filter((f) => f.endsWith('.rs'));
  if (ext === '*.yml') return all.filter((f) => f.endsWith('.yml'));
  if (ext === 'build.rs') return all.filter((f) => f.endsWith('build.rs'));
  return all.filter((f) => f.endsWith(path.extname(ext)));
}

const seen = new Set();
const rows = [];
let total = 0;
let english = 0;
for (const pattern of PATTERNS) {
  for (const file of filesFor(pattern)) {
    if (seen.has(file)) continue;
    seen.add(file);
    const lines = fs.readFileSync(file, 'utf8').split('\n');
    let comments = 0;
    let englishOnly = 0;
    for (const line of lines) {
      const s = line.trim();
      if (!s) continue;
      if (!isCommentLine(s)) continue;
      comments++;
      if (!HAS_CJK.test(s) && HAS_WORD.test(s)) englishOnly++;
    }
    total += comments;
    english += englishOnly;
    if (englishOnly) rows.push({ file: path.relative(ROOT, file).replace(/\\/g, '/'), englishOnly, comments });
  }
}
rows.sort((a, b) => b.englishOnly - a.englishOnly);
if (process.argv.includes('--files')) {
  for (const r of rows) console.log(`${r.englishOnly}\t${r.file}`);
} else {
  for (const r of rows) console.log(`  ${String(r.englishOnly).padStart(5)} /${String(r.comments).padStart(5)}  ${r.file}`);
}
const done = total ? Math.round(((total - english) / total) * 1000) / 10 : 100;
console.log(`[comment-lang] 注释共 ${total} 行，英文 ${english} 行，中文覆盖率 ${done}%`);
