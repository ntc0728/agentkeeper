#!/usr/bin/env node
'use strict';
/**
 * Build tauri-shell/dist from src/renderer.
 *
 * `src/renderer` is the only source of the frontend; `dist` is derived, so no
 * hand edit ever survives here. Only two deltas exist, and both are applied
 * here so they can never drift:
 *
 *   1. <html data-shell="tauri">  — lets CSS/JS branch on the host shell
 *   2. <script src="tauri-bridge.js">  — the window.ak shim over invoke()
 *
 * Run this before every `cargo build` of the shell. It is idempotent and
 * verifies its own output byte-for-byte, so a stale dist/ cannot sneak into a
 * build silently (that failure mode looks like "my UI fix didn't apply").
 *
 * Usage: node tauri-shell/copy-frontend.js [--check]
 *   --check : do not write, only report whether dist/ is up to date
 */
const fs = require('fs');
const path = require('path');

const SHELL = __dirname;
const ROOT = path.resolve(SHELL, '..');
const SRC = path.join(ROOT, 'src', 'renderer');
const DIST = path.join(SHELL, 'dist');

/** Copied verbatim, no transformation. Order is the <script> order. */
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

/* ---- 1. index.html: apply the two deltas ---- */
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

// The injected lines must use the SAME line ending the source file uses.
// A CRLF source with LF-injected lines produces a file whose bytes no longer
// match what this script computes — so `--check` would report `dist/index.html`
// is stale forever, and the copy it writes would differ from the copy it
// verifies. The newline is detected from the source itself, not from the
// anchor (the anchor is one line and carries no newline).
const NEWLINE = html && html.includes('\r\n') ? '\r\n' : '\n';
const BRIDGE_INJECT = BRIDGE_INJECT_LINES.join(NEWLINE);

const htmlOut = html
  ? html
      .replace(HTML_ANCHOR, HTML_REPLACED)
      .replace(BRIDGE_ANCHOR, BRIDGE_INJECT + NEWLINE + BRIDGE_ANCHOR)
  : '';

/* ---- 2. assemble the full expected file set ---- */
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

/* ---- 3. write / compare ---- */
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

/* ---- 4. nothing extra should ever end up in dist/ ---- */
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
