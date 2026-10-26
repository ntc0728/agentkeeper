'use strict';
/**
 * 完整的 UI 审计：未翻译的字符串、失效的交互元素、主题
 * 违规。发现项都是可以直接动手的清单——去改代码，别写计划。
 *
 *   node scripts/ui-audit.js
 */
const fs = require('fs');
const path = require('path');

const ROOT = path.join(__dirname, '..');
const app = fs.readFileSync(path.join(ROOT, 'src/renderer/app.js'), 'utf8');
const html = fs.readFileSync(path.join(ROOT, 'src/renderer/index.html'), 'utf8');
const css = fs.readFileSync(path.join(ROOT, 'src/renderer/styles.css'), 'utf8');
const i18n = fs.readFileSync(path.join(ROOT, 'src/renderer/i18n.js'), 'utf8');

const problems = { i18n: [], interaction: [], theme: [], ux: [] };

/* ---------- 1. 未翻译的字符串 ---------- */

// index.html static text nodes. Text that the runtime replaces on boot is
// fine to keep in English in the markup: #nav-group-* is rewritten by
// applyLanguage(), #view-title/#view-sub by render() on every navigation.
const RUNTIME_OWNED = /id="(nav-group-overview|nav-group-operate|view-title|view-sub|modal-title)"/;
const RUNTIME_OWNED_CLASS = /class="nav-text"/;
html.split('\n').forEach((l, i) => {
  const m = l.match(/>([A-Za-z][A-Za-z .':-]{2,50})</);
  if (m && !/<\/(svg|path|option)>/.test(l) && m[1].trim() !== 'AgentKeeper'
    && !RUNTIME_OWNED.test(l) && !RUNTIME_OWNED_CLASS.test(l)) {
    problems.i18n.push(`index.html:${i + 1}  static text "${m[1].trim()}"`);
  }
  const a = l.match(/\s(title|placeholder)="([A-Za-z][^"]{2,})"/);
  if (a && !/data-i18n/.test(l)) problems.i18n.push(`index.html:${i + 1}  ${a[1]}="${a[2]}"`);
});

// app.js: bare el('th', {}, 'English') headers, titles, switch labels
app.split('\n').forEach((l, i) => {
  if (/^\s*(\/\/|\*|\/\*)/.test(l)) return;
  const th = l.match(/el\('th',\s*(?:\{[^}]*\},\s*)?'([A-Za-z][^']*)'\)/);
  if (th && !/t\('/.test(th[0])) problems.i18n.push(`app.js:${i + 1}  table header "${th[1]}"`);
  const ti = l.match(/title:\s*'([A-Za-z][^']{4,})'/);
  if (ti && !/t\('/.test(ti[0])) problems.i18n.push(`app.js:${i + 1}  title "${ti[1]}"`);
  const lbl = l.match(/el\('label',\s*\{\},\s*'([A-Za-z][^']{3,})'\)/);
  if (lbl && !/t\('/.test(lbl[0])) problems.i18n.push(`app.js:${i + 1}  label "${lbl[1]}"`);
  const empty = l.match(/class: 'empty'\s*\},\s*'([A-Za-z][^']{8,})'/);
  if (empty && !/t\('/.test(empty[0])) {
    problems.i18n.push(`app.js:${i + 1}  empty-state text "${empty[1].slice(0, 50)}"`);
  }
});

/* ---------- 2. dead interactive elements ---------- */

// Buttons whose click handler is attached on a later line is fine; what we
// hunt is buttons inside el(...) trees that never get a listener anywhere.
const appLines = app.split('\n');
const buttonLines = [];
appLines.forEach((l, i) => {
  const m = l.match(/el\('button',\s*\{(.*)\}/);
  if (m) buttonLines.push({ line: i + 1, attrs: m[1], text: l.trim().slice(0, 110) });
});
for (const b of buttonLines) {
  const hasInline = /onclick/.test(b.attrs);
  // Look ahead 4 lines for addEventListener with a matching class/variable —
  // approximate, but catches the real cases.
  const ctx = appLines.slice(b.line - 1, b.line + 4).join('\n');
  const hasLater = /addEventListener\('click'/.test(ctx);
  if (!hasInline && !hasLater) {
    problems.interaction.push(`app.js:${b.line}  button without any click handler — ${b.text}`);
  }
}

// Data rows without onclick while sibling rows are clickable. Legitimate
// exclusions: rows whose cell contains the action (empty states with a CTA
// button, the "+N more" expander) and tables explicitly marked .static.
if (/table\.tbl tbody tr \{ cursor: pointer/.test(css)) {
  appLines.forEach((l, i) => {
    if (!/el\('tr',\s*\{/.test(l)) return;
    // Header rows may wrap: the el('th') can be on the next line.
    const ctx3 = appLines.slice(i, i + 3).join('\n');
    if (/el\('th'/.test(ctx3) || /el\('thead'/.test(l)) return;   // header rows
    if (/class: 'empty'/.test(l)) return;                       // empty states
    if (/colspan/.test(l)) return;                              // expander / footer rows
    if (/onclick/.test(l)) return;                              // wired
    // A row inside a .static table is intentionally inert.
    const ctx = appLines.slice(Math.max(0, i - 6), i + 1).join('\n');
    if (/tbl static/.test(ctx)) return;
    problems.interaction.push(`app.js:${i + 1}  data row without onclick while other rows are clickable — verify intent`);
  });
}

// Elements that look like controls but are spans/divs
appLines.forEach((l, i) => {
  if (/class: '[^']*\bclickable\b/.test(l) && /'span'/.test(l) && !/onclick/.test(l)) {
    problems.interaction.push(`app.js:${i + 1}  span marked .clickable without onclick`);
  }
});

/* ---------- 3. theme violations (hardcoded colours) ---------- */

const cssLines = css.split('\n');
cssLines.forEach((l, i) => {
  const hex = l.match(/#[0-9a-fA-F]{3,8}\b/g);
  if (!hex) return;
  if (/\s--[\w-]+\s*:/.test(l)) return;               // variable definition
  if (i < 42 || (i >= 47 && i <= 105)) return;        // :root + dark/system blocks
  problems.theme.push(`styles.css:${i + 1}  hardcoded ${hex.join(' ')} — ${l.trim().slice(0, 80)}`);
});

/* ---------- 4. UX gaps (structural, from reading the views) ---------- */

const checks = [
  ['batch operations', /S\.checked/, 'no multi-select anywhere — bulk delete/distribute impossible'],
  ['context menu', /showCtxMenu/, 'no right-click menu'],
  ['pagination', /PER_PAGE|S\.page/, 'assets table renders unbounded rows'],
  ['keyboard nav in tables', /ArrowDown/, 'tables not keyboard navigable'],
  ['persisted UI state', /persistUiState/, 'filters/sort reset on restart'],
  ['update check wired', /updateCheck|checkUpdates/, 'updateCheck setting stored but nothing reads it'],
  ['notifications wired', /maybeNotify/, 'notifications setting stored but nothing sends one'],
  ['telemetry wired', /localStats/, 'telemetry setting stored but nothing sends counts'],
  ['empty-state CTA', /projects\.noneLong[\s\S]{0,200}doImportProject|empty[\s\S]{0,120}class: 'btn primary'/, 'empty states are plain text without a call to action'],
  ['truncation tooltips', /t-desc', title:/, 'long truncated text has no title attribute'],
  // Known deliberate gaps — kept visible so the decision stays documented:
  ['drag to move', /dragstart|draggable/, 'no drag to move between scopes (distribute dialog covers the capability; drag is a UX duplicate)'],
  ['virtual scrolling', /virtual|IntersectionObserver/, 'no windowing — pagination covers it; revisit only past ~5k rows per filter'],
];
for (const [name, re, msg] of checks) {
  if (!re.test(app)) problems.ux.push(msg);
}

/* ---------- report ---------- */

let n = 0;
for (const [cat, list] of Object.entries(problems)) {
  console.log(`\n== ${cat.toUpperCase()} (${list.length}) ==`);
  for (const p of list.slice(0, 60)) { console.log('  ' + p); n++; }
  if (list.length > 60) console.log(`  … ${list.length - 60} more`);
}
console.log(`\ntotal findings: ${n}`);