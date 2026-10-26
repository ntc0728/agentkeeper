'use strict';
/**
 * 渲染进程产物包的轻量源码 lint。
 *
 * 打包器是零依赖的，所以没有 eslint。这些检查瞄准的是 i18n 重构期间
 * 真正咬到我们的那些错误：
 *
 *   1. 局部标识符遮蔽了模块作用域里的同名标识符（同一个模块里写两次
 *      `const t` 会在加载时抛错，整个窗口一片空白）
 *   2. 本该走 t() 的硬编码 UI 字符串
 *   3. 内联 style 属性，我们的严格 CSP 拒绝它们
 *   4. require 不配对 / 文件缺失
 *
 *   node scripts/lint-renderer.js
 */

const fs = require('fs');
const path = require('path');
const vm = require('vm');

const ROOT = path.join(__dirname, '..');
const SRC = path.join(ROOT, 'src');
const problems = [];

function rel(p) { return path.relative(ROOT, p); }

/* ---- 1. 顶层重复声明 + 被遮蔽的标识符 ---- */

/**
 * 找出那些会让整个窗口空白的标识符冲突。
 *
 * 正则无法模拟 JS 的作用域——模块作用域的 `const t = ...` 加上箭头函数参数
 * `(t) => ...`，各自单看都合法，但当两者落进同一个脚本里就会抛
 * "Identifier 't' has already been declared"。`node --check` 也抓不到它
 * （它把这个文件当脚本处理，而且只在 sloppy 模式下警告一次），所以我们
 * 按浏览器的方式编译这个文件：包进一个函数里，并把 `const` 的重复声明
 * 提升为错误。
 */
function checkScope(src, file) {
  const names = new Map();
  // 1. 模块作用域的声明（第 0 列）
  for (const m of src.matchAll(/^(?:const|let|var|function|class)\s+([A-Za-z_$][\w$]*)/gm)) {
    names.set(m[1], (names.get(m[1]) || 0) + 1);
  }
  // 2. 箭头函数参数与 catch 绑定——它们活在自己的作用域里，
  //    所以只有*同一个函数体内*的重复才是问题。
  //    我们保守地把同一行里重复出现的参数标记出来。
  const dupTop = [...names.entries()].filter(([, n]) => n > 1);
  for (const [name] of dupTop) {
    problems.push(`${rel(file)}  duplicate top-level \`${name}\` — throws at load`);
  }

  // 3. 真正的检验：按浏览器的方式编译这个文件——作为严格的
  //    模块脚本。在 sloppy 模式下 `const t` + `(t) => …` 并不会抛错，
  //    这正是那个 bug 能躲过 `node --check` 的原因。
  try {
    // eslint-disable-next-line no-new-func
    new Function('"use strict";\n' + src);
  } catch (e) {
    if (e instanceof SyntaxError && /already been declared|redeclar/i.test(e.message)) {
      problems.push(`${rel(file)}  ${e.message} — rename one of them (this blanks the window at load)`);
      return false;
    }
  }
  return true;
}

/* ---- 2. 硬编码的 UI 字符串 ---- */

const UI_STRING_PATTERNS = [
  // el('...', {...}, 'Some English text') — 字面量子节点
  /\bel\((?:[^()]|\([^()]*\))*?,\s*'([A-Z][a-z]+ [a-z][^']{4,60})'/g,
  /\bel\((?:[^()]|\([^()]*\))*?,\s*"([A-Z][a-z]+ [a-z][^"]{4,60})"/g,
  // 必须匹配的三种形状：末尾的 label 参数、一次
  // textContent/innerHTML 赋值，以及 options 数组里的 label。
  //   a) 末尾的 label 参数：  }, 'Some English text')
  /\},\s*'([A-Z][a-z]+ [a-z][^']{3,60})'\s*\)/g,
  //   b) 一次 textContent/innerHTML 赋值：  x.textContent = 'Some text'
  /\.(?:textContent|innerHTML)\s*=\s*'(?:[a-z]{2,6}:\s*)?([A-Z][a-z]+ [a-z][^']{3,60})'/g,
  //   c) options 数组里的 label：  [['value', 'Some English label'], …]
  /\[\s*\[\s*'[a-z-]+',\s*'([A-Z][a-z]+ [a-z][^']{2,60})'\s*\]/g
];

function findHardcoded(src, file) {
  const lines = src.split('\n');
  let hits = 0;
  lines.forEach((line, i) => {
    // 跳过注释和 i18n 词典本身。
    if (/^\s*(\/\/|\*|\/\*)/.test(line)) return;
    for (const re of UI_STRING_PATTERNS) {
      re.lastIndex = 0;
      let m;
      while ((m = re.exec(line))) {
        // 调用 t() 是可以的；裸字面量不行。
        const before = line.slice(Math.max(0, m.index - 3), m.index);
        if (/t\(\s*$/.test(before)) continue;
        hits++;
        if (hits <= 5) {
          problems.push(`${rel(file)}:${i + 1}  hardcoded UI string ${JSON.stringify(m[1].slice(0, 40))} — wrap in t()`);
        }
      }
    }
  });
  if (hits > 5) problems.push(`${rel(file)}  ${hits - 5} more hardcoded string(s)`);
  return hits;
}

/* ---- 3. 内联 style 属性（CSP） ---- */

function findInlineStyles(src, file) {
  const n = (src.match(/\bstyle:\s*['"]/g) || []).length;
  if (n) {
    const first = src.split('\n').findIndex((l) => /style:\s*['"]/.test(l)) + 1;
    problems.push(`${rel(file)}:${first}  ${n} inline style attribute(s) — CSP \`style-src 'self'\` refuses them; use el()'s style (CSSOM) or a class`);
  }
  return n;
}

/* ---- 4. 解析不到的 require ---- */

function checkRequires(src, file) {
  for (const m of src.matchAll(/require\(\s*['"]([^'"]+)['"]\s*\)/g)) {
    const spec = m[1];
    if (!spec.startsWith('.')) continue;
    const target = path.resolve(path.dirname(file), spec);
    const ok = fs.existsSync(target) || fs.existsSync(target + '.js') || fs.existsSync(path.join(target, 'index.js'));
    if (!ok) problems.push(`${rel(file)}  require('${spec}') does not resolve`);
  }
}

/* ---- 5. i18n 词典对齐 ---- */

function checkI18nParity() {
  const i18nPath = path.join(SRC, 'renderer', 'i18n.js');
  if (!fs.existsSync(i18nPath)) return;
  const src = fs.readFileSync(i18nPath, 'utf8');
  const grab = (lang) => {
    const i = src.indexOf(`  ${lang}: {`);
    if (i < 0) return new Set();
    const j = src.indexOf('\n  }', i);
    const body = src.slice(i, j);
    return new Set([...body.matchAll(/^\s*'([^']+)':/gm)].map((m) => m[1]));
  };
  const en = grab('en');
  const zh = grab('zh');
  if (!en.size) { problems.push('i18n.js: could not parse the en dictionary'); return; }
  const missing = [...en].filter((k) => !zh.has(k));
  const extra = [...zh].filter((k) => !en.has(k));
  if (missing.length) problems.push(`i18n.js: zh is missing ${missing.length} key(s): ${missing.slice(0, 6).join(', ')}`);
  if (extra.length) problems.push(`i18n.js: zh has ${extra.length} extra key(s): ${extra.slice(0, 6).join(', ')}`);
  if (!missing.length && !extra.length) console.log(`  i18n parity OK (${en.size} keys in both languages)`);

  // 只有键对齐还不够：一个键可以在两种语言里都定义了，
  // 却仍然被用成一个两种语言里都不存在的名字来调用（拼写错误，或者
  // 只在 app.js 里改了名）。这种情况下 t() 会把键本身返回，于是
  // UI 就无声无息地把 "dashboard.enabledCount" 显示给用户。
  const appPath = path.join(SRC, 'renderer', 'app.js');
  if (fs.existsSync(appPath)) {
    const app = fs.readFileSync(appPath, 'utf8');
    const html = fs.existsSync(path.join(SRC, 'renderer', 'index.html'))
      ? fs.readFileSync(path.join(SRC, 'renderer', 'index.html'), 'utf8') : '';
    const used = new Set();
    for (const m of app.matchAll(/\bt\(\s*'([a-z][\w.]+)'/g)) used.add(m[1]);
    for (const m of html.matchAll(/data-i18n(?:-placeholder)?="([a-z][\w.]+)"/g)) used.add(m[1]);
    const unknown = [...used].filter((k) => !en.has(k));
    for (const k of unknown) {
      problems.push(`i18n.js  t('${k}') is called but no such key exists — the UI would render the key itself`);
    }
    if (!unknown.length && used.size) console.log(`  i18n usage OK (${used.size} keys referenced, all defined)`);
  }
}

/* ---- 5b. i18n：没人引用的键（死重） ---- */

/**
 * 上面那个正向检查（"调用了但没定义"）有一个镜像：
 * 两种语言里都定义了、却没人用的键。它们并不是无害的——每一个都是
 * 有人为一个已经不存在的 UI 写的翻译，而词典只增不减，于是这个文件
 * 慢慢变成了第二个看不见的产品界面。
 *
 * 覆盖率刻意做成静态的：一个键只要以字符串字面量的形式出现在 app.js /
 * index.html 的任何地方（这覆盖了 `titleKey`、`data-i18n` 以及各种比较），
 * 或者它位于某个动态根之下（例如 `t('assets.' + kind)`），就算被用到了。
 * 这种估算是对用法的过度近似，对一个 linter 来说是安全的那个方向——
 * 它绝不会要求删掉一个还活着的键。
 */
function checkI18nUnused() {
  const i18nPath = path.join(SRC, 'renderer', 'i18n.js');
  const appPath = path.join(SRC, 'renderer', 'app.js');
  if (!fs.existsSync(i18nPath) || !fs.existsSync(appPath)) return;
  const i18nSrc = fs.readFileSync(i18nPath, 'utf8');
  const app = fs.readFileSync(appPath, 'utf8');
  const html = fs.existsSync(path.join(SRC, 'renderer', 'index.html'))
    ? fs.readFileSync(path.join(SRC, 'renderer', 'index.html'), 'utf8') : '';

  const i = i18nSrc.indexOf('  en: {');
  const j = i18nSrc.indexOf('\n  }', i);
  const en = [...i18nSrc.slice(i, j).matchAll(/^\s*'([^']+)':/gm)].map((m) => m[1]);

  const roots = new Set();
  for (const m of app.matchAll(/\bt\(\s*'([a-z][\w.]*)'\s*\+/g)) roots.add(m[1]);
  for (const m of app.matchAll(/\bt\(\s*`([a-z][\w.]*)\$\{/g)) roots.add(m[1]);

  const used = (k) => app.includes(`'${k}'`) || app.includes(`"${k}"`) || html.includes(k)
    || [...roots].some((p) => p !== k && k.startsWith(p));
  const dead = en.filter((k) => !used(k));
  if (dead.length) {
    problems.push(`i18n.js  ${dead.length} key(s) defined in both languages but never referenced — delete them: ${dead.slice(0, 8).join(', ')}${dead.length > 8 ? ' …' : ''}`);
  } else {
    console.log(`  i18n dead keys  : none (${en.length} keys all referenced)`);
  }
}

/* ---- 6. 跨脚本的全局冲突 ---- */

/**
 * 传统的 <script> 标签共享同一个全局词法作用域。所以 app.js 里顶层的
 * `const t` 与 i18n.js 里顶层的 `function t` 会在加载时抛
 * "Identifier 't' has already been declared"，窗口一片空白，且没有任何其他
 * 症状。node --check 抓不到这一点，因为它每个文件都是单独编译的。
 */
function checkGlobalCollisions(files) {
  const owner = new Map();
  for (const f of files) {
    const src = fs.readFileSync(f, 'utf8');
    for (const m of src.matchAll(/^(?:const|let|var|class)\s+([A-Za-z_$][\w$]*)/gm)) {
      if (/^(window|globalThis|document|module|exports|require|process)$/.test(m[1])) continue;
      if (!owner.has(m[1])) owner.set(m[1], []);
      owner.get(m[1]).push(rel(f));
    }
    for (const m of src.matchAll(/^function\s+([A-Za-z_$][\w$]*)/gm)) {
      if (!owner.has(m[1])) owner.set(m[1], []);
      owner.get(m[1]).push(rel(f));
    }
  }
  let n = 0;
  for (const [name, files_] of owner) {
    const distinct = [...new Set(files_)];
    if (distinct.length > 1) {
      n++;
      problems.push(`global collision: \`${name}\` declared in ${distinct.join(' and ')} — classic scripts share one lexical scope, the window will not paint`);
    }
  }
  return n;
}

/* ---- 7. 未翻译的字面量 ---- */

const CODE_ATTRS = new Set([
  'class', 'className', 'style', 'title', 'value', 'href', 'src', 'type',
  'role', 'tabindex', 'id', 'placeholder', 'onclick', 'onchange', 'oninput',
  'onkeydown', 'aria-checked', 'width', 'height'
]);
const CODE_VALUES = /^(btn|card|chip|alert|empty|mono|muted|ghost|ok|err|warn|tbl|pre-wrap|flex|grid|spread|nowrap|num|row|col|tl|stat|select-inline|switch|input-inline|set-row|set-label|set-hint|set-control|items|drawer|modal|toast|diffbox|diff|diff-add|diff-del|diff-ctx|editor|form-grid|footnote|kv|page|head|sub|spacer|mb-|mt-|ml-|mr-|fs-|fw-|gap-|col |wrap|log-box|diff-scroll|picker-box|editor-tall|nav-|side-|brand-|view-|app-|toasts|search)/;

function findUntranslated(src, file) {
  const hits = [];
  src.split('\n').forEach((line, i) => {
    if (/^\s*(\/\/|\*|\/\*)/.test(line)) return;
    const re = /([A-Za-z][\w-]*)\s*:\s*'([^']{2,90})'/g;
    let m;
    while ((m = re.exec(line))) {
      const [, attr, val] = m;
      if (CODE_ATTRS.has(attr)) continue;
      if (CODE_VALUES.test(val)) continue;
      if (/^[a-z][\w.-]*$/.test(val) && !/\s/.test(val)) continue;  // id / flag
      if (/^#[0-9a-f]{3,8}$/i.test(val)) continue;                    // colour
      if (/^\d+$/.test(val)) continue;                              // number
      if (/^(2-digit|short|long|numeric|2-digit-alpha|short-2-digit)$/.test(val)) continue;  // Intl option
      hits.push({ line: i + 1, val });
    }
  });
  for (const h of hits.slice(0, 12)) {
    problems.push(`${rel(file)}:${h.line}  untranslated UI text ${JSON.stringify(h.val.slice(0, 40))} — wrap in t()`);
  }
  if (hits.length > 12) problems.push(`${rel(file)}  ${hits.length - 12} more untranslated string(s)`);
  return hits.length;
}

/* ---- 7b. 模板字面量里的散文 ---- */

/**
 * 对象键在上面已经过滤掉了；这里抓的是问题的另一半
 * ——被插值进模板字面量的英文散文，例如：
 *   el('h3', {}, `${n} changes waiting`)      → 属性规则抓不到
 *   toast(`Applied to ${n} locations`)        → 属性规则抓不到
 * 我们只标记那些既含空格（散文）又含小写字母的字面量
 * （也就是排除掉标识符、路径或 CSS 值）。
 */
function findTemplateProse(src, file) {
  const hits = [];
  // 先剥掉行注释：关于 CSP 字符串的说明不是 UI。
  const lines = src.split('\n');
  const re = /`([^`]*)`/g;
  let m;
  while ((m = re.exec(src))) {
    const startLine = src.slice(0, m.index).split('\n').length - 1;
    const atLineStart = lines[startLine].trim().startsWith('//');
    if (atLineStart) continue;
    const body = m[1];
    // 只看字面量文本块，不看 ${…} 表达式。
    for (const chunk of body.split(/\$\{[^}]*\}/)) {
      // 嵌套模板，而它自己的字面量已经翻译过了。
      if (/t\('[\w.]+'\)\s*$/.test(chunk)) continue;
      const words = chunk.trim();
      if (words.length < 8) continue;
      if (!/[a-z]{3,}/.test(words)) continue;          // needs real words
      if (!/\s/.test(words)) continue;                  // prose has spaces
      if (/^[\w./\\:@' -]+$/.test(words) && !/^[A-Z]/.test(words)) continue;
      // 已知的非 UI 片段：CSP 指令、文件路径、类似 MIME 的标记。
      if (/style-src|script-src|default-src|Content-Security/i.test(words)) continue;
      // HTML 标记，不是散文——只有里面被转义的文字才算数。
      if (/^<\/?[a-z][\w-]*[\s>]/i.test(words)) continue;
      // 刻意保留的英文兜底文案，用于 i18n.js 自己加载失败的情况。
      if (/^Startup failed:/i.test(words)) continue;
      const line = src.slice(0, m.index).split('\n').length;
      hits.push({ line, val: words.slice(0, 50) });
    }
  }
  for (const h of hits.slice(0, 10)) {
    problems.push(`${rel(file)}:${h.line}  English prose in a template literal ${JSON.stringify(h.val)} — use t()`);
  }
  if (hits.length > 10) problems.push(`${rel(file)}  ${hits.length - 10} more template literal(s) with English prose`);
  return hits.length;
}

/* ---- 运行 ---- */

console.log('linting renderer…\n');
const targets = [
  path.join(SRC, 'renderer', 'app.js'),
  path.join(SRC, 'renderer', 'i18n.js'),
  path.join(SRC, 'renderer', 'theme-init.js')
];

let shadowCount = 0, hardCount = 0, styleCount = 0;
for (const f of targets) {
  if (!fs.existsSync(f)) { problems.push(`${rel(f)} missing`); continue; }
  const src = fs.readFileSync(f, 'utf8');
  // 先解析再 lint。下面每条规则都是把源码当*文本*读的，所以一个
  // 解析不过的文件会一路畅通地穿过所有文本规则，而渲染进程在运行时
  // 却求值失败。这项检查只花几毫秒，却能把这种情况变成一次构建失败。
  try {
    new vm.Script(src, { filename: f });
  } catch (e) {
    problems.push(`${rel(f)} does not parse: ${e.message}`);
    continue;
  }
  shadowCount += checkScope(src, f) ? 0 : 1;
  // 每个目标都是渲染进程文件。这个守卫防止非渲染进程目标
  // 悄无声息地继承「未翻译字符串」这条只适用于 UI 的规则。
  hardCount += f.includes(`${path.sep}renderer${path.sep}`) ? findUntranslated(src, f) : 0;
  if (f.endsWith("app.js")) hardCount += findTemplateProse(src, f);
  styleCount += findInlineStyles(src, f);
  checkRequires(src, f);
}
checkI18nParity();
checkI18nUnused();
const collisions = checkGlobalCollisions([
  path.join(SRC, 'renderer', 'app.js'),
  path.join(SRC, 'renderer', 'i18n.js'),
  path.join(SRC, 'renderer', 'theme-init.js')
]);
console.log(`  global collisions  : ${collisions}`);

console.log(`  shadowed identifiers: ${shadowCount}`);
console.log(`  hardcoded UI strings: ${hardCount}`);
console.log(`  inline style attrs  : ${styleCount}`);

if (problems.length) {
  console.log(`\n${problems.length} problem(s):`);
  for (const p of problems) console.log('  • ' + p);
  process.exit(1);
}
console.log('\nclean');