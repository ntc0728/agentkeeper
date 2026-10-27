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

// index.html 里的静态文本节点。运行时会在启动时替换掉的文字，留在标记里是英文也没
// 关系：#nav-group-* 由 applyLanguage() 重写，#view-title/#view-sub 每次导航都由
// render() 重写。
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

// app.js：裸的 el('th', {}, 'English') 表头、标题、开关标签
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

/* ---------- 2. 死掉的交互元素 ---------- */

// 点击处理器挂在后面几行的按钮没问题；这里要抓的是那些躺在 el(...) 树里、从头到尾
// 没挂过任何监听器的按钮。
const appLines = app.split('\n');
const buttonLines = [];
appLines.forEach((l, i) => {
  const m = l.match(/el\('button',\s*\{(.*)\}/);
  if (m) buttonLines.push({ line: i + 1, attrs: m[1], text: l.trim().slice(0, 110) });
});
for (const b of buttonLines) {
  const hasInline = /onclick/.test(b.attrs);
  // 往后看 4 行，找类名/变量名匹配的 addEventListener —— 粗糙，但真出问题的都能抓到。
  const ctx = appLines.slice(b.line - 1, b.line + 4).join('\n');
  const hasLater = /addEventListener\('click'/.test(ctx);
  if (!hasInline && !hasLater) {
    problems.interaction.push(`app.js:${b.line}  button without any click handler — ${b.text}`);
  }
}

// 同级的行可点、它自己却没有 onclick 的数据行。合理例外：动作挂在单元格里的行
// （带 CTA 按钮的空态、"展开更多"），以及明确标了 .static 的表格。
//
// 判据以前只读 `el('tr', {` **这一行**，而 app.js 里 onclick 通常写在下一行
// （对象字面量换行展开）。于是每一个多行写法的可点行都会被报成缺陷 —— 例如
// `el('tr', {` 下一行紧跟 `onclick: () => drillAgent(id)` 或 `onclick: openFirst`
// 的写法。把窗口拉宽到整个对象字面量的开头几行，按代码实际结构判断。
if (/table\.tbl tbody tr \{ cursor: pointer/.test(css)) {
  appLines.forEach((l, i) => {
    if (!/el\('tr',\s*\{/.test(l)) return;
    // 表头行可能换行：el('th') 会出现在下一行。
    const ctx3 = appLines.slice(i, i + 3).join('\n');
    if (/el\('th'/.test(ctx3) || /el\('thead'/.test(l)) return;   // header rows
    if (/class: 'empty'/.test(l)) return;                       // empty states
    if (/colspan/.test(l)) return;                              // expander / footer rows
    // 属性写在紧跟的几行里：取到对象字面量的末尾（`}` 后接 `,` 或 `)`）为止。
    let block = '';
    for (let k = i; k < Math.min(appLines.length, i + 12); k++) {
      block += appLines[k] + '\n';
      if (/^\s*\},?\s*$/.test(appLines[k]) || /^\s*\},/.test(appLines[k])) break;
    }
    if (/onclick/.test(block)) return;                          // wired (multi-line)
    // 只读表格里的行是有意不可交互的。
    const ctx = appLines.slice(Math.max(0, i - 8), i + 1).join('\n');
    if (/tbl static/.test(ctx)) return;
    problems.interaction.push(`app.js:${i + 1}  data row without onclick while other rows are clickable — verify intent`);
  });
}

// 长得像控件、实际是 span/div 的元素
appLines.forEach((l, i) => {
  if (/class: '[^']*\bclickable\b/.test(l) && /'span'/.test(l) && !/onclick/.test(l)) {
    problems.interaction.push(`app.js:${i + 1}  span marked .clickable without onclick`);
  }
});

/* ---------- 3. 主题违规（硬编码颜色） ---------- */

const cssLines = css.split('\n');
cssLines.forEach((l, i) => {
  const hex = l.match(/#[0-9a-fA-F]{3,8}\b/g);
  if (!hex) return;
  if (/\s--[\w-]+\s*:/.test(l)) return;               // variable definition
  if (i < 42 || (i >= 47 && i <= 105)) return;        // :root + dark/system blocks
  problems.theme.push(`styles.css:${i + 1}  hardcoded ${hex.join(' ')} — ${l.trim().slice(0, 80)}`);
});

/* ---------- 4. 体验缺口（读代码看出来的结构问题） ---------- */

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
];
for (const [name, re, msg] of checks) {
  if (!re.test(app)) problems.ux.push(msg);
}

/* ---------- 5. 有意的取舍（不是缺陷，单独列出） ---------- */

/**
 * 这两条以前混在 findings 里，于是每次审计都报「有问题」，而它们其实是**已经
 * 做过决定、并且能给出判据**的取舍。混在一起的代价是真实的告警被稀释：一份永远
 * 亮着几条的记录，没人会去看下一条。
 *
 * 每条都写成可复核的形式：为什么不做、什么时候它才变成缺陷。
 */
const decisions = [
  {
    name: 'drag to move between scopes',
    probe: /dragstart|draggable/,
    reason: '顶部「分发到…」对话框已覆盖同一能力（选中目标 → 复制过去），拖拽只是第二个入口',
    becomes: '当分发对话框无法表达目标（例如需要跨多个项目作用域排序）时重新评估'
  },
  {
    name: 'virtual scrolling',
    probe: /virtual|IntersectionObserver/,
    reason: '分页把每页固定在 50 行；903 个资产 = 19 页，真机实测渲染 10.6 ms、翻页 14.3 ms（1440×900）',
    becomes: '当单次筛选结果超过约 5000 行（100 页）导致翻页本身成为负担时'
  }
];
// `probe` 命中 = 该能力已实现；没命中 = 它确实不在这次交付里，即「取舍成立」。
const accepted = decisions.filter((d) => !d.probe.test(app));

/* ---------- 报告 ---------- */

let n = 0;
for (const [cat, list] of Object.entries(problems)) {
  console.log(`
== ${cat.toUpperCase()} (${list.length}) ==`);
  for (const p of list.slice(0, 60)) { console.log('  ' + p); n++; }
  if (list.length > 60) console.log(`  … ${list.length - 60} more`);
}
if (accepted.length) {
  console.log(`
== ACCEPTED (${accepted.length}) — 有意的取舍，不计入 findings ==`);
  for (const d of accepted) {
    console.log(`  ${d.name}`);
    console.log(`    why  : ${d.reason}`);
    console.log(`    until: ${d.becomes}`);
  }
}
console.log(`
total findings: ${n}`);
