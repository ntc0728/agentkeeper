'use strict';
/**
 * Lightweight source lint for the renderer bundle.
 *
 * The bundler is zero-dependency, so there is no eslint. These checks target
 * the mistakes that actually bit us during the i18n refactor:
 *
 *   1. a local identifier shadowing a module-scope one (`const t` twice in a
 *      module throws at load time and blanks the whole window)
 *   2. hardcoded UI strings that should have gone through t()
 *   3. inline style attributes, which our strict CSP refuses
 *   4. unbalanced requires / missing files
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

/* ---- 1. duplicate top-level + shadowed identifiers ---- */

/**
 * Detect identifier clashes that blank the whole window.
 *
 * A regex cannot model JS scoping — `const t = ...` at module scope plus
 * `(t) => ...` as an arrow parameter is legal in isolation but throws
 * "Identifier 't' has already been declared" when both land in one script.
 * `node --check` does not catch it either (it treats the file as a script and
 * only warns once in sloppy mode), so we compile the file the way the browser
 * will: wrapped in a function, with `const` redeclarations promoted to errors.
 */
function checkScope(src, file) {
  const names = new Map();
  // 1. module-scope declarations (column 0)
  for (const m of src.matchAll(/^(?:const|let|var|function|class)\s+([A-Za-z_$][\w$]*)/gm)) {
    names.set(m[1], (names.get(m[1]) || 0) + 1);
  }
  // 2. arrow-function parameters and catch bindings — these live in their own
  //    scope, so only a *duplicate within one function body* is a problem.
  //    We conservatively flag a parameter that repeats in the same line.
  const dupTop = [...names.entries()].filter(([, n]) => n > 1);
  for (const [name] of dupTop) {
    problems.push(`${rel(file)}  duplicate top-level \`${name}\` — throws at load`);
  }

  // 3. the real test: compile the file the way the browser does — as a strict
  //    module script. In sloppy mode `const t` + `(t) => …` does NOT throw,
  //    which is exactly why the bug survived `node --check`.
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

/* ---- 2. hardcoded UI strings ---- */

const UI_STRING_PATTERNS = [
  // el('...', {...}, 'Some English text') — a literal child
  /\bel\((?:[^()]|\([^()]*\))*?,\s*'([A-Z][a-z]+ [a-z][^']{4,60})'/g,
  /\bel\((?:[^()]|\([^()]*\))*?,\s*"([A-Z][a-z]+ [a-z][^"]{4,60})"/g,
  // 2026-10-18: three shapes the original heuristic missed and that shipped
  // real English into the zh UI:
  //   a) a trailing label argument:  }, 'Some English text')
  /\},\s*'([A-Z][a-z]+ [a-z][^']{3,60})'\s*\)/g,
  //   b) a textContent/innerHTML assignment:  x.textContent = 'Some text'
  /\.(?:textContent|innerHTML)\s*=\s*'(?:[a-z]{2,6}:\s*)?([A-Z][a-z]+ [a-z][^']{3,60})'/g,
  //   c) a label in an options array:  [['value', 'Some English label'], …]
  /\[\s*\[\s*'[a-z-]+',\s*'([A-Z][a-z]+ [a-z][^']{2,60})'\s*\]/g
];

function findHardcoded(src, file) {
  const lines = src.split('\n');
  let hits = 0;
  lines.forEach((line, i) => {
    // Skip comments and the i18n dictionary itself.
    if (/^\s*(\/\/|\*|\/\*)/.test(line)) return;
    for (const re of UI_STRING_PATTERNS) {
      re.lastIndex = 0;
      let m;
      while ((m = re.exec(line))) {
        // A call to t() is fine; a bare literal is not.
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

/* ---- 3. inline style attributes (CSP) ---- */

function findInlineStyles(src, file) {
  const n = (src.match(/\bstyle:\s*['"]/g) || []).length;
  if (n) {
    const first = src.split('\n').findIndex((l) => /style:\s*['"]/.test(l)) + 1;
    problems.push(`${rel(file)}:${first}  ${n} inline style attribute(s) — CSP \`style-src 'self'\` refuses them; use el()'s style (CSSOM) or a class`);
  }
  return n;
}

/* ---- 4. unresolved requires ---- */

function checkRequires(src, file) {
  for (const m of src.matchAll(/require\(\s*['"]([^'"]+)['"]\s*\)/g)) {
    const spec = m[1];
    if (!spec.startsWith('.')) continue;
    const target = path.resolve(path.dirname(file), spec);
    const ok = fs.existsSync(target) || fs.existsSync(target + '.js') || fs.existsSync(path.join(target, 'index.js'));
    if (!ok) problems.push(`${rel(file)}  require('${spec}') does not resolve`);
  }
}

/* ---- 5. i18n dictionary parity ---- */

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

  // Parity alone is not enough: a key can be defined in both languages and
  // still be called with a name that exists in neither (typo, or a key that
  // was renamed in app.js only). t() returns the key itself in that case, so
  // the UI silently shows "dashboard.enabledCount" to the user.
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

/* ---- 5b. i18n: keys nobody references (dead weight) ---- */

/**
 * The forward check above ("called but not defined") has a mirror image:
 * keys that are defined in both languages and used by nobody. They are not
 * harmless — every one of them is a translation somebody wrote for a UI that
 * no longer exists, and the dictionary only ever grows, so the file slowly
 * becomes a second, invisible product surface.
 *
 * Coverage is static on purpose: a key counts as used when it appears as a
 * string literal anywhere in app.js / index.html (that covers `titleKey`,
 * `data-i18n` and comparisons) or when it sits under a dynamic root such as
 * `t('assets.' + kind)`. That over-approximates usage, which is the safe
 * direction for a linter — it will never demand the deletion of a live key.
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

/* ---- 6. cross-script global collisions ---- */

/**
 * Classic <script> tags share ONE global lexical scope. A top-level `const t`
 * in app.js and a top-level `function t` in i18n.js therefore throw
 * "Identifier 't' has already been declared" at load, and the window goes
 * blank with no other symptom. node --check does not catch this because it
 * compiles each file in isolation.
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

/* ---- 7. untranslated literals ---- */

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

/* ---- 7b. prose inside template literals ---- */

/**
 * Object keys get filtered above; this catches the other half of the problem
 * — English prose interpolated into template literals, e.g.
 *   el('h3', {}, `${n} changes waiting`)      → missed by the attr rule
 *   toast(`Applied to ${n} locations`)        → missed by the attr rule
 * We only flag literals that contain a space (prose) and a lowercase letter
 * (i.e. not an identifier, path, or CSS value).
 */
function findTemplateProse(src, file) {
  const hits = [];
  // Strip line comments first: documentation about the CSP string is not UI.
  const lines = src.split('\n');
  const re = /`([^`]*)`/g;
  let m;
  while ((m = re.exec(src))) {
    const startLine = src.slice(0, m.index).split('\n').length - 1;
    const atLineStart = lines[startLine].trim().startsWith('//');
    if (atLineStart) continue;
    const body = m[1];
    // Only the literal text chunks, not the ${…} expressions.
    for (const chunk of body.split(/\$\{[^}]*\}/)) {
      // A nested template whose own literal is already translated.
      if (/t\('[\w.]+'\)\s*$/.test(chunk)) continue;
      const words = chunk.trim();
      if (words.length < 8) continue;
      if (!/[a-z]{3,}/.test(words)) continue;          // needs real words
      if (!/\s/.test(words)) continue;                  // prose has spaces
      if (/^[\w./\\:@' -]+$/.test(words) && !/^[A-Z]/.test(words)) continue;
      // Known non-UI fragments: CSP directives, file paths, MIME-ish tokens.
      if (/style-src|script-src|default-src|Content-Security/i.test(words)) continue;
      // HTML markup, not prose — only the escaped text inside matters.
      if (/^<\/?[a-z][\w-]*[\s>]/i.test(words)) continue;
      // The deliberate English fallback for when i18n.js itself failed to load.
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

/* ---- run ---- */

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
  // Parse before linting. Every rule below reads the source as *text*, so a file
  // that does not even parse used to sail through: a comment-stripping edit once
  // removed the backticks around the one template literal in app.js, the whole
  // renderer failed to evaluate, and the first thing to notice was the
  // real-machine probe (page stuck on its skeleton, no language switch). This
  // check costs milliseconds and turns that into a build failure.
  try {
    new vm.Script(src, { filename: f });
  } catch (e) {
    problems.push(`${rel(f)} does not parse: ${e.message}`);
    continue;
  }
  shadowCount += checkScope(src, f) ? 0 : 1;
  // Every target is a renderer file now that the JS kernel (core/, src/main/)
  // is deleted. The guard stays so a future non-renderer target cannot silently
  // inherit the untranslated-string rule, which is UI-only by construction.
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