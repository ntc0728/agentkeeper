#!/usr/bin/env node
'use strict';
/**
 * 从 src/renderer 生成 tauri-shell/dist。
 *
 * 前端只有 `src/renderer` 一个源头；`dist` 是派生产物，所以手改这里的东西永远留不住。
 * 全流程只有两处差异，都在这一个脚本里施加，因此不可能各自漂移：
 *
 *   1. <html data-shell="tauri">  —— 让 CSS/JS 能按宿主外壳分支
 *   2. <script src="tauri-bridge.js">  —— 铺在 invoke() 之上的 window.ak 垫片
 *
 * 每次构建外壳之前都要跑它。它是幂等的，并且会逐字节校验自己的产物，所以过期的
 * dist/ 不可能悄悄混进构建（那种故障看起来就像「我改的 UI 没生效」）。
 *
 * 用法：node tauri-shell/copy-frontend.js [--check]
 *   --check ：不写盘，只报告 dist/ 是不是最新的
 */
const fs = require('fs');
const path = require('path');

const SHELL = __dirname;
const ROOT = path.resolve(SHELL, '..');
const SRC = path.join(ROOT, 'src', 'renderer');
const DIST = path.join(SHELL, 'dist');

/** 原样复制，不做任何转换。顺序就是 <script> 的顺序。 */
const PLAIN = ['theme-init.js', 'engine-zh.js', 'i18n.js', 'app.js', 'styles.css'];

const BRIDGE_ANCHOR = '  <script src="engine-zh.js"></script>';
const BRIDGE_INJECT_LINES = [
  '  <!-- Tauri shell: exposes window.__TAURI__.core.invoke -->',
  '  <script src="tauri-bridge.js"></script>',
];

const HTML_ANCHOR = '<html lang="en" data-theme="light">';
const HTML_REPLACED = '<html lang="en" data-theme="light" data-shell="tauri">';

const checkOnly = process.argv.includes('--check');
const problems = [];
const report = [];

function fail(msg) {
  problems.push(msg);
}

/* ---- 1. index.html：施加那两处差异 ---- */
let html;
try {
  html = fs.readFileSync(path.join(SRC, 'index.html'), 'utf8');
} catch (e) {
  fail(`cannot read src/renderer/index.html: ${e.message}`);
}

if (html) {
  if (!html.includes(HTML_ANCHOR)) {
    fail(
      `src/renderer/index.html no longer contains ${JSON.stringify(HTML_ANCHOR)}.\n` +
        'The shell tag injection is anchored on that exact string — update HTML_ANCHOR in this script.'
    );
  }
  if (!html.includes(BRIDGE_ANCHOR)) {
    fail(
      `src/renderer/index.html no longer contains ${JSON.stringify(BRIDGE_ANCHOR)}.\n` +
        'The bridge injection is anchored on the first script tag so the shim loads before app.js — ' +
        'update BRIDGE_ANCHOR in this script.'
    );
  }
  if (!html.includes('src="theme-init.js"')) {
    fail('src/renderer/index.html no longer loads theme-init.js — dist/ would render untranslated/unthemed.');
  }
}

// 注入的行必须与源文件用**同一种**换行符。CRLF 的源配上 LF 的注入行，产出的字节就
// 与这个脚本算出来的对不上 —— 于是 `--check` 会永远报告 `dist/index.html` 过期，
// 而它写出去的副本与它校验的副本也不是同一个东西。换行符从源文件本身探测，不从锚点
// 探测（锚点只有一行，自己不带换行）。
const NEWLINE = html && html.includes('\r\n') ? '\r\n' : '\n';
const BRIDGE_INJECT = BRIDGE_INJECT_LINES.join(NEWLINE);

const htmlOut = html
  ? html
      .replace(HTML_ANCHOR, HTML_REPLACED)
      .replace(BRIDGE_ANCHOR, BRIDGE_INJECT + NEWLINE + BRIDGE_ANCHOR)
  : '';

/* ---- 2. 组装出期望的完整文件集合 ---- */
const expected = new Map();
if (htmlOut) expected.set('index.html', Buffer.from(htmlOut, 'utf8'));
for (const f of PLAIN) {
  try {
    expected.set(f, fs.readFileSync(path.join(SRC, f)));
  } catch (e) {
    fail(`cannot read src/renderer/${f}: ${e.message}`);
  }
}
try {
  expected.set('tauri-bridge.js', fs.readFileSync(path.join(SHELL, 'tauri-bridge.js')));
} catch (e) {
  fail(`cannot read tauri-shell/tauri-bridge.js: ${e.message}`);
}

/* ---- 3. 写入 / 比对 ---- */
if (problems.length === 0 && !checkOnly) {
  fs.mkdirSync(DIST, { recursive: true });
  for (const [name, buf] of expected) {
    const dst = path.join(DIST, name);
    let same = false;
    try {
      same = fs.readFileSync(dst).equals(buf);
    } catch (_e) {
      same = false;
    }
    if (!same) fs.writeFileSync(dst, buf);
    report.push(`  ${same ? 'same' : 'wrote'}  ${name.padEnd(16)} ${buf.length} bytes`);
  }
} else if (problems.length === 0) {
  for (const [name, buf] of expected) {
    let same = false;
    try {
      same = fs.readFileSync(path.join(DIST, name)).equals(buf);
    } catch (_e) {
      same = false;
    }
    if (!same) fail(`dist/${name} is stale — run: node tauri-shell/copy-frontend.js`);
    report.push(`  ${same ? 'ok  ' : 'STALE'}  ${name.padEnd(16)} ${buf.length} bytes`);
  }
}

/* ---- 4. dist/ 里永远不该多出任何东西 ---- */
const extra = fs.existsSync(DIST)
  ? fs.readdirSync(DIST).filter((n) => !expected.has(n))
  : [];
for (const e of extra) fail(`dist/${e} is not produced by this script (stale artifact?)`);

if (problems.length) {
  console.error('[copy-frontend] FAILED');
  for (const p of problems) console.error('  - ' + p);
  process.exit(1);
}

console.log(`[copy-frontend] ${checkOnly ? 'dist/ up to date' : 'dist/ synced from src/renderer'}`);
for (const line of report) console.log(line);
