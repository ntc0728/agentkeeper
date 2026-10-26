#!/usr/bin/env node
'use strict';
/**
 * Rust 引擎的 i18n 审计——「不许降级」保证的 Tauri 那一半。
 *
 * 策略：既然 UI 是中文的，引擎放到屏幕上的每一句话都必须渲染成中文。
 * 只有专有名词 / 协议词汇是允许的例外。
 *
 * 引擎是 `tauri-shell/src-tauri/src/**\/*.rs`，而每一条 Rust 消息
 * 都以 `{ ok:false, error:"…" }`、一个 `reason` 字段、一条
 * 校验问题或一个 `const &str` 的形式抵达渲染进程。这个脚本刮取所有字符串字面量，
 * 过滤掉那些到不了屏幕的（见 `looksSentence`），并断言每一条幸存者都有词典条目；
 * 它还会把词典*反向*对回引擎做审计（ORPHAN），这样死条目和加错位置的
 * 条目会浮出来，而不是悄无声息地堆积。
 *
 * 依赖：只有 `src/renderer/engine-zh.js` 和 Rust 源码。
 * ORPHAN 完全只对着 Rust 引擎判定，因为出货构建里只有它会发出句子。
 *
 * 它还检查词典可能弄错、却永远不会以 MISS 形式暴露的两件事：
 *   CODE-MISS  Rust 引擎发出、而 CODE_ZH 翻译不了的 `code` 字面量
 *              （这时用户看到的是一段生硬的英文代码）；
 *   CJK-KEY    出现在 EXACT / PATTERNS *键* 位置上的中文，也就是一个
 *              必须放着引擎英文的位置。
 *
 *   node scripts/tauri-i18n.js             关口：命中 MISS/AMBIGUOUS 时退出 1
 *   node scripts/tauri-i18n.js --report    打印每一个桶，始终退出 0
 *
 * 退出码：0 干净，1 MISS / AMBIGUOUS / CODE-MISS / CJK-KEY，2 跑不起来。
 */

const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
/**
 * 每棵能把句子放到屏幕上的 Rust 源码树。
 *
 * 引擎住在 `agentkeeper-core` crate 里；Tauri shell 有自己
 * 的一棵树。两棵都扫——只看其中一棵的关口会漏掉一半英文句子还不被察觉，
 * 而那次 crate 拆分把大部分句子搬到了另一棵里。
 */
const RUST_SRC_DIRS = [
  path.join(ROOT, 'tauri-shell', 'core', 'src'),
  path.join(ROOT, 'tauri-shell', 'src-tauri', 'src'),
];
const DICT = path.join(ROOT, 'src', 'renderer', 'engine-zh.js');

for (const dir of RUST_SRC_DIRS) {
  if (!fs.existsSync(dir)) {
    console.error(`[tauri-i18n] missing Rust source tree: ${dir}`);
    process.exit(2);
  }
}
if (!fs.existsSync(DICT)) {
  console.error(`[tauri-i18n] missing dictionary: ${DICT}`);
  process.exit(2);
}
const zh = require(DICT);
const DICT_SRC = fs.readFileSync(DICT, 'utf8');

const reportOnly = process.argv.includes('--report');

/* ------------------------------------------------------------------ */
/* 1. 判定规则                                                          */

/**
 * 这个字符串看起来像一句英语使用者会读出来的句子吗？
 * 同一个正则（含 140 字符上限）和同样那两个额外条件，
 * 所以在上一版实现里通过同一条规则的字符串，在这里也照样通过。
 */
function looksSentence(s) {
  return /^[A-Za-z][A-Za-z0-9 ,.:;'‘’"()%`[\]{}\-—&+!?/\\_§]{4,140}$/.test(s)
    && /[a-z]{3}/.test(s)
    && /\s[a-z]{2,}/.test(s);
}

const CJK = /[\u4e00-\u9fff]/;

/* ------------------------------------------------------------------ */
/* 2. Rust 源码 → 生产代码（非测试）行                          */

/**
 * 丢掉 `#[cfg(test)] mod … { … }` 块里的每一行。
 *
 * 测试体是夹具和断言标签；它们永远到不了 WebView，
 * 而且装着这个 crate 里约 300 条像句子的字面量。深度是在一份
 * 把字符串字面量抹空后的行副本上跟踪的，所以 `"` 里面的花括号不会
 * 把计数带偏。
 */
function productionLines(src) {
  const out = [];
  let depth = 0;
  let skipUntilDepth = null;   // depth the test module opened at
  let pendingCfgTest = 0;      // lines since a bare `#[cfg(test)]` attribute

  src.split('\n').forEach((line, i) => {
    const bare = line
      .replace(/"(?:[^"\\]|\\.)*"/g, '""')           // string literals
      .replace(/\/\/.*$/, '');                       // trailing comments
    let braces = 0;
    for (const ch of bare) {
      if (ch === '{') braces += 1;
      else if (ch === '}') braces -= 1;
    }

    const before = depth;
    const t = line.trim();

    let testStart = false;
    if (/^#\[cfg\(test\)\]/.test(t)) {
      if (/mod\s+[\w]+\s*\{/.test(t)) testStart = true;   // one-liner form
      else pendingCfgTest = 1;                            // attribute, mod follows
    } else if (pendingCfgTest > 0 && /^(pub(\([^)]*\))?\s+)?mod\s+[\w]+\s*\{/.test(t)) {
      testStart = true;
    } else if (/^mod\s+(tests?|spec)\s*\{/.test(t)) {
      testStart = true;                                   // bare `mod tests {`
    }

    if (skipUntilDepth === null && testStart) skipUntilDepth = before;
    depth += braces;

    if (skipUntilDepth === null) out.push([i + 1, line]);

    if (skipUntilDepth !== null && depth <= skipUntilDepth) skipUntilDepth = null;
    if (pendingCfgTest > 0) pendingCfgTest = pendingCfgTest >= 2 ? 0 : pendingCfgTest + 1;
  });

  return out;
}

/* ------------------------------------------------------------------ */
/* 3. 字面量提取 + 归一化                               */

/** Rust 的 `{}` / `{name}` / `{:?}` / `{:#?}` / `{0}` → 按顺序的 §1..§n。 */
function normalizePlaceholders(s) {
  let n = 0;
  return s.replace(/\{[^{}]*\}/g, () => `§${++n}`);
}

/** 撤销扫描器同样撤销过的转义，好让引号写法与词典一致。 */
function unescape(s) {
  return s.replace(/\\"/g, '"').replace(/\\\\/g, '\\');
}

const STRING_LITERAL = /"((?:[^"\\\n]|\\.)*)"/g;

/*
 * 那些字面量并非用户所读文本的调用点。它们全都是
 * 结构性的（某一类调用点），绝不是按措辞开的白名单。
 *
 *   .expect(…) / panic!(…) / assert*!   会中止进程；WebView 根本看不到
 *   trace/log/eprintln!/…               写到 stderr 的开发者诊断信息
 *   .contains/.starts_with/…            该字面量是对一条外来错误串的
 *                                       INPUT 匹配器，不是我们发出的文本
 */
const PANIC_SITE = /(?:^|[^\w])(?:panic!|\.expect|assert_eq!|assert_ne!|assert!)\(\s*(?:&)?(?:format!\()?\s*$/;
const DIAGNOSTIC_SITE = /\b(?:trace|log|debug|info|warn|eprintln!|println!|print!|dbg!)\(\s*(?:&)?(?:format!\()?\s*$/;
const INPUT_MATCHER = /\.(?:contains|starts_with|ends_with|find|replace|replacen|split|trim_start_matches|trim_end_matches|strip_prefix|strip_suffix)\(\s*$/;

/*
 * 日志面的 sink，按 OUTPUT CHANNEL 豁免——绝不按文件豁免，这样
 * `core/update/manager.rs` 里所有不是日志
 * 行的东西仍然会被审计。
 *
 * 更新管理器把进度行路由到一个注入进来的
 * `type LogFn = Arc<dyn Fn(&str) + Send + Sync>`，它在生产环境下的值是
 * `Arc::new(|_| {})`——一个空操作。除了这个 crate 的测试之外，
 * 没有任何地方会装上真正的 sink，所以这些字符串既到不了 WebView，也到不了
 * 终端，翻译它们只会让日志更难 grep。
 *
 * 两种方式匹配，因为 rustfmt 会把一个很长的 `format!` 单独放一行：
 *   LOG_SITE     字面量与 sink 调用在同一行
 *   LOG_SINK     sink 调用在这里打开；字面量在后面某一行
 * `logSurface` 永远被计数并完整列出，所以增长是看得见的。
 */
const LOG_SITE = /(?:^|[^\w.])(?:self\s*\.\s*log|logger|log::\w+|(?:log|info|warn|error|debug|trace)!)\s*\(\s*(?:&)?(?:format!\()?\s*$/;
const LOG_SINK = /(?:^|[^\w.])(?:self\s*\.\s*log|logger|log::\w+|(?:log|info|warn|error|debug|trace)!)\s*\(\s*(?:&)?(?:format!\()?/;
/** 更新管理器打上通道前缀的进度行。 */
const LOG_PREFIXES = ['[update]'];

/**
 * 唯一一处回答「这句英文属于日志面而不是 UI 吗」的地方。
 *
 * 它被用了两次——刮取时（row.log）和判定词典条目时（ORPHAN）——因为
 * 这两处检查绝不能对同一个字符串给出相反的结论：「引擎发出了它，但没发到
 * 屏幕」和「没人用它，删掉它」不可能同时成立。
 *
 * 只有 PREFIX 这一半是可以共享的。刮取侧检验的另一半
 * （LOG_SITE / inLogCall）是调用点的属性，而词典条目
 * 没有调用点；所以一条文本里不带通道前缀的日志行，是由刮取
 * 认出来的，而不是由这个谓词。
 */
function isLogSurface(s) {
  return LOG_PREFIXES.some((p) => s.replace(/^\^/, '').startsWith(p));
}

function extract(file) {
  const rows = [];
  const src = fs.readFileSync(file, 'utf8');
  const rel = path.relative(ROOT, file).replace(/\\/g, '/');

  // 先把原始字符串（r#"…"#, r##"…"##, r"…"）抹空：它们装的是正则
  // 和 JSON 夹具，绝不是散文，而且它们内嵌的引号（以及第 0 列上
  // 偶尔出现的 `}`）否则会同时毁掉字面量扫描和花括号计数。
  // `\b` 很重要：没有它，一个紧挨在引号前、以 "r" 结尾的单词
  // （"…frontmatter""）会被当成原始字符串的开头。
  const noRaw = src.replace(/\br(#*)"([\s\S]*?)"\1/g, '""');

  // 一个参数列表仍然打开着的日志 sink 调用的深度。rustfmt 会把一个
  // 很长的 `format!(…)` 单独放一行，所以字面量常常在拥有它的
  // sink 下面好几行。
  let logDepth = 0;

  for (const [line, text] of productionLines(noRaw)) {
    if (/^\s*(\/\/|\*|\/\*)/.test(text)) continue;      // a comment, not a sentence

    const code = text.replace(/\/\/.*$/, '');           // trailing // comment
    const opensLog = LOG_SINK.test(code);
    const inLogCall = logDepth > 0;

    let m;
    STRING_LITERAL.lastIndex = 0;
    while ((m = STRING_LITERAL.exec(code))) {
      const before = code.slice(0, m.index);
      if (PANIC_SITE.test(before)) continue;
      const raw = unescape(m[1]);
      const norm = normalizePlaceholders(raw);
      if (!norm) continue;
      /*
       * 日志面的检验在 DIAGNOSTIC_SITE 之前，而这个顺序正是
       * 全部要点。`self.log("…")` 会让 `before` 以 `log(` 结尾，而
       * DIAGNOSTIC_SITE 同样匹配它；如果先查诊断信息，这些
       * 字面量会在日志检验跑起来之前就被丢掉——既不算 MISS，又不出现在任何
       * 桶里，无影无踪。翻译它们的那些词典条目随后被报成 ORPHAN：
       * 一个字符串，两种相反的结论。只有被 rustfmt 换行的调用（字面量在下一行，
       * 由 inLogCall 捕获）才可能逃过这一劫。
       */
      if (LOG_SITE.test(before) || inLogCall || isLogSurface(norm) || isLogSurface(raw)) {
        rows.push({ file: rel, line, s: norm, raw, log: true });
        continue;
      }
      if (DIAGNOSTIC_SITE.test(before)) continue;
      if (INPUT_MATCHER.test(before)) continue;
      // 之所以保留 `raw`，是因为 `{}` 不总是格式占位符：在一个
      // 正则量词（"{0,63}"）里，它会原封不动地一路活到
      // 屏幕上，所以词典键里装的是花括号，而不是 § 标记。
      rows.push({ file: rel, line, s: norm, raw });
    }

    // 整行的括号平衡（字符串字面量已抹空），这样跨越多行的 sink
    // 调用会让 `logDepth` 一直保持打开，直到它的 `)` 落地。
    const bare = code.replace(/"(?:[^"\\]|\\.)*"/g, '""');
    let balance = 0;
    for (const ch of bare) {
      if (ch === '(') balance += 1;
      else if (ch === ')') balance -= 1;
    }
    if (logDepth > 0) logDepth = Math.max(0, logDepth + balance);
    else if (opensLog && balance > 0) logDepth = balance;
  }
  return rows;
}

/* ------------------------------------------------------------------ */
/* 4. 分桶——每一种排除都有结构性的理由                */

/*
 * 审计刻意不去判定的那些桶。它们是刮取规则
 * （什么算引擎散文），并且与词典里的 INTERNAL 严格分开
 * （那是引擎散文，但永远到不了屏幕）。
 * 这里没有任何一条是真漏检的后门：每个桶都在报告里被计数，
 * 而那些需要人为裁量的桶会完整列出。
 */

/**
 * 独立的 CLI 二进制（`src/bin/*.rs`）：散文去终端，永远不会去
 * WebView。按文件限定，并在报告里完整打印，让增长保持
 * 可见——这里承认的代价是，往某个 bin 文件里加的 UI 消息
 * 会被藏起来。今天 bin 里没有 UI 通道。
 */
const CLI_FILE = /\/bin\/[a-z0-9-]+\.rs$/;

/*
 * MCP stdio 协议面。`core/mcp.rs` 就是 MCP 服务器：它的 JSON-RPC
 * 错误和工具 schema 描述发给 MCP 客户端（一个 LLM），永远不会发给
 * WebView。按 FILE 排除，而不是按措辞——这里承认的代价
 * 是往 mcp.rs 里加的一条真 UI 消息会被藏起来。今天 mcp.rs 没有 UI
 * 通道；如果哪天有了，就把文件拆开并把这条规则收窄。
 *
 * 路径只按文件*名*匹配，不按它所在的目录：
 * 当 core 独立成自己的 crate 时，模块从 `src-tauri/src/core/mcp.rs` 搬到了
 * `core/src/mcp.rs`，而一条锚在旧目录上的规则会悄无声息地不再匹配——
 * 那样关口就会要求为那些只到得了 LLM 的句子准备中文
 * 翻译。
 */
const PROTOCOL_FILE = /(^|[\\/])mcp\.rs$/;

function classify(r) {
  if (r.log) return 'logSurface';
  if (CJK.test(r.s)) return 'chinese';
  // 解转义之后还含反斜杠的字面量是多行
  // 夹具（"line one\nline two\n"）、正则或路径——不是散文。
  if (r.s.includes('\\')) return 'escaped';
  if (!looksSentence(r.s)) return 'notProse';
  // `format!("{}", e)` 之类本身不带任何词。
  if (!/[A-Za-z]{3}/.test(r.s.replace(/§\d/g, ''))) return 'placeholder';
  if (CLI_FILE.test('/' + r.file)) return 'cli';
  if (PROTOCOL_FILE.test('/' + r.file)) return 'protocol';
  return null;
}

/* ------------------------------------------------------------------ */
/* 5. 覆盖率，附带来源                                        */

const compiledPatterns = (zh.PATTERNS || []).map(([p, z]) => ({
  src: p, re: zh.patternToRegExp(p), zh: z, i: -1
}));
compiledPatterns.forEach((c, i) => { c.i = i; });

/** 哪个词典条目渲染 `s`？null = 没覆盖。 */
function covering(s) {
  if (Object.prototype.hasOwnProperty.call(zh.EXACT, s)) {
    return { kind: 'EXACT', id: s, zh: zh.EXACT[s] };
  }
  if (zh.INTERNAL && zh.INTERNAL.has(s)) return { kind: 'INTERNAL', id: s, zh: '(passthrough)' };
  for (const c of compiledPatterns) {
    if (c.re.test(s)) return { kind: 'PATTERNS', id: `#${c.i}`, src: c.src, i: c.i, zh: c.zh };
  }
  return null;
}

/** 所有*可能*渲染 `s` 的条目——多于一个就意味着歧义。 */
function allCovering(s) {
  const out = [];
  if (Object.prototype.hasOwnProperty.call(zh.EXACT, s)) out.push({ kind: 'EXACT', zh: zh.EXACT[s] });
  for (const c of compiledPatterns) if (c.re.test(s)) out.push({ kind: `PATTERNS#${c.i}`, zh: c.zh });
  return out;
}

/* ------------------------------------------------------------------ */
/* 6. 词典 → 引擎的检查（死条目 / 加错位置的条目）     */

/**
 * 词典里被声明了不止一次的英文键。`require()` 会让
 * 一个重复的对象键悄无声息地胜出，所以这必须从源码上读，
 * 而不是从加载后的对象上读——这正是这项检查的全部意义。
 */
function duplicateExactKeys() {
  const from = DICT_SRC.indexOf('const EXACT');
  const to = DICT_SRC.indexOf('const PATTERNS');
  if (from < 0 || to < from) return [];
  const counts = new Map();
  const re = /^[ \t]{2}'((?:[^'\\]|\\.)*)'[ \t]*:/gm;
  let m;
  while ((m = re.exec(DICT_SRC.slice(from, to)))) {
    const k = m[1].replace(/\\'/g, "'");
    counts.set(k, (counts.get(k) || 0) + 1);
  }
  return [...counts.entries()].filter(([, n]) => n > 1);
}

/* ------------------------------------------------------------------ */
/* 6b. Rust 的 `code` 字面量 → CODE_ZH                                  */

/**
 * Rust 引擎能放到线路上的每一条 code 字面量，全部在运行时从
 * 源码里提取——绝不来自手抄的清单。三条结构性规则，
 * 按强度递减排列：
 *
 *   json      `"code": "x"`            构造进一个跨越边界的 Value/map
 *                                      ——总是被发出
 *   table     `const …CODES` 数组里 ("msg", "code") 键值的第 2 个元素
 *                                      ——查表项的 code 那一半
 *   field     struct 里的 `code: "x"`  只有当该 struct 派生了
 *                                      Serialize 时才会被发出；否则这个值根本
 *                                      到不了渲染进程
 */
function structIsSerialized(text, lineIdx) {
  const lines = text.split('\n');
  for (let i = lineIdx; i >= 0; i -= 1) {
    if (!/\b(?:pub(?:\([^)]*\))?\s+)?struct\s+\w+/.test(lines[i])) continue;
    for (let j = i; j >= Math.max(0, i - 12); j -= 1) {
      const d = lines[j].match(/#\[derive\(([^)]*)\)/);
      if (d) return /Serialize/.test(d[1]);
    }
    return false;
  }
  return false;
}

function extractCodes(file) {
  const src = fs.readFileSync(file, 'utf8');
  const rel = path.relative(ROOT, file).replace(/\\/g, '/');
  const noRaw = src.replace(/\br(#*)"([\s\S]*?)"\1/g, '""');
  const out = [];

  // 规则：table —— 扫整个 const 块，而不是一行一行扫（键值会换行）。
  const tableRe = /const\s+[A-Za-z_]*CODES?[A-Za-z_]*\s*:[^=]*=\s*\[/g;
  let t;
  while ((t = tableRe.exec(noRaw))) {
    const end = noRaw.indexOf('\n];', t.index);
    const block = noRaw.slice(t.index, end < 0 ? noRaw.length : end);
    const baseLine = noRaw.slice(0, t.index).split('\n').length;
    const pairRe = /\(\s*("(?:[^"\\]|\\.)*"|[A-Z_][A-Z0-9_]*)\s*,\s*"((?:[^"\\]|\\.)*)"\s*,?\s*\)/g;
    let p;
    while ((p = pairRe.exec(block))) {
      const line = baseLine + block.slice(0, p.index).split('\n').length - 1;
      out.push({ file: rel, line, code: unescape(p[2]), rule: 'table' });
    }
  }

  for (const [line, text] of productionLines(noRaw)) {
    if (/^\s*(\/\/|\*|\/\*)/.test(text)) continue;
    const code = text.replace(/\/\/.*$/, '');
    for (const m of code.matchAll(/"code"\s*:\s*"((?:[^"\\]|\\.)*)"/g)) {
      out.push({ file: rel, line, code: unescape(m[1]), rule: 'json' });
    }
    for (const m of code.matchAll(/\bcode\s*:\s*"((?:[^"\\]|\\.)*)"/g)) {
      out.push({
        file: rel, line, code: unescape(m[1]), rule: 'field',
        serialized: structIsSerialized(noRaw, line - 1)
      });
    }
  }
  return out;
}

/** 出现在「必须放英文」位置上的中文（或任何 CJK）。 */
const CJK_KEY = /[\u3040-\u30ff\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\uac00-\ud7af]/;

function cjkKeys() {
  const out = [];
  for (const k of Object.keys(zh.EXACT)) {
    if (CJK_KEY.test(k)) out.push({ where: 'EXACT', key: k, zh: zh.EXACT[k] });
  }
  (zh.PATTERNS || []).forEach(([p, z], i) => {
    if (CJK_KEY.test(p)) out.push({ where: `PATTERNS#${i}`, key: p, zh: z });
  });
  return out;
}

/* ------------------------------------------------------------------ */
/* 7. 运行                                                              */

function rustFiles(dir) {
  const out = [];
  (function walk(d) {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const f = path.join(d, e.name);
      if (e.isDirectory()) walk(f);
      else if (e.name.endsWith('.rs')) out.push(f);
    }
  })(dir);
  return out.sort();
}

const files = RUST_SRC_DIRS.flatMap((dir) => rustFiles(dir));
const all = [];
for (const f of files) all.push(...extract(f));

const seen = new Set();
const rows = all.filter((r) => {
  const k = `${r.file}|${r.s}`;
  if (seen.has(k)) return false;
  seen.add(k);
  return true;
});

const buckets = {
  miss: [], coveredExact: [], coveredPattern: [], coveredInternal: [],
  chinese: [], escaped: [], notProse: [], placeholder: [],
  cli: [], protocol: [], logSurface: []
};
const provenance = new Map();   // sentence -> covering entry

for (const r of rows) {
  const noise = classify(r);
  if (noise) { buckets[noise].push(r); continue; }
  const hit = covering(r.s);
  if (!hit) { buckets.miss.push(r); continue; }
  provenance.set(r.s, hit);
  if (hit.kind === 'EXACT') buckets.coveredExact.push(r);
  else if (hit.kind === 'INTERNAL') buckets.coveredInternal.push(r);
  else buckets.coveredPattern.push(r);
}

/* --- 词典侧的发现 --------------------------------------- */

// AMBIGUOUS-DUP：同一个英文键在 EXACT 里被声明了不止一次。
const dupKeys = duplicateExactKeys();

/*
 * AMBIGUOUS-MULTI：一个句子，两条或更多 PATTERNS 规则都能渲染
 * 它。谁胜出由它们在列表里的顺序决定，所以删掉或
 * 调整一条规则的顺序会悄无声息地换掉屏幕上的中文——这正是值得
 * 让构建失败的陷阱。
 *
 * 刻意不计入：既命中 EXACT、又匹配某条 PATTERNS 规则的条目。
 * `zhError()` 先查 EXACT，所以那是赢家确定的遮蔽，不是歧义。
 * 这类配对在下面单独报告（EXACT
 * SHADOWING）作为一条说明，让这层关系保持可见。
 */
const multi = [];
const shadowedByExact = [];
for (const r of rows) {
  if (classify(r)) continue;
  const pats = compiledPatterns.filter((c) => c.re.test(r.s));
  if (pats.length > 1) {
    multi.push({ r, cands: pats.map((c) => ({ kind: `PATTERNS#${c.i}`, zh: c.zh })) });
  } else if (pats.length === 1 && Object.prototype.hasOwnProperty.call(zh.EXACT, r.s)) {
    shadowedByExact.push({ r, exact: zh.EXACT[r.s], pat: pats[0] });
  }
}

// SHADOWED：一条永远不可能胜出的 PATTERNS 条目，因为前面某条规则
// 已经匹配了它本来能匹配的所有句子。
const shadowed = [];
compiledPatterns.forEach((c) => {
  const losers = rows.filter((r) => !classify(r) && c.re.test(r.s) && covering(r.s) !== null
    && covering(r.s).kind === 'PATTERNS' && covering(r.s).i !== c.i);
  if (losers.length) shadowed.push({ c, losers });
});

/*
 * ORPHAN：一条 Rust 引擎其实从不使用的词典条目。
 *
 * 按用法（USAGE）衡量，而不是按文本里是否出现：一个条目在没有任何
 * 抽取到的字面量由它渲染时就是死的。这样就避免了像
 * `asset not found` 这样的误报——引擎只会以插值形式发出它
 * （`asset not found: §1`），所以它其实是被某条 PATTERNS 条目渲染的，
 * 而那条裸的 EXACT 键在别处仍然有用。
 */
const usage = new Map();                       // "EXACT|<key>" / "PATTERNS|<i>" -> count
for (const r of rows) {
  const hit = covering(r.s);
  if (!hit) continue;
  const k = `${hit.kind}|${hit.kind === 'PATTERNS' ? hit.i : hit.id}`;
  usage.set(k, (usage.get(k) || 0) + 1);
}

/*
 * 补救轮——不让刮取上的局限伪造出死条目。
 *
 * `looksSentence`（规则保持原样）只接受一个固定字符类里的字符，
 * 而 `^` / `$` 不在其中。所以像
 * `MCP server name must match /^…$/` 这样的消息永远不会被刮到，而只按刮取
 * 结果衡量的用度会把它报成 ORPHAN——一个会删掉活着的翻译的
 * 误报。所以当某条抽取到的*生产字面量*能渲染一个条目时，它也
 * 算被用过，哪怕那个字面量没有被判成散文。
 */
const literalForms = new Set();
for (const r of all) { literalForms.add(r.s); if (r.raw) literalForms.add(r.raw); }
for (const s of literalForms) {
  const hit = covering(s);
  if (!hit) continue;
  const k = `${hit.kind}|${hit.kind === 'PATTERNS' ? hit.i : hit.id}`;
  usage.set(k, (usage.get(k) || 0) + 1);
}

const orphans = [];
for (const k of Object.keys(zh.EXACT)) {
  if (usage.get(`EXACT|${k}`)) continue;
  // 覆盖率检查用来把这个字符串路由到日志面的同一个
  // 入口点。在这里跳过它，正是让这两处检查不会彼此
  // 分歧的原因：「发出了但没发到屏幕」和「死了，删掉它」不可能同时成立。
  if (isLogSurface(k)) continue;
  orphans.push({ kind: 'EXACT', id: k, zh: zh.EXACT[k] });
}
compiledPatterns.forEach((c) => {
  if (usage.get(`PATTERNS|${c.i}`)) return;
  if (isLogSurface(c.src)) return;
  orphans.push({ kind: `PATTERNS#${c.i}`, id: c.src, zh: c.zh });
});

// ORPHAN 只对着 Rust 引擎判定：一个没被用到的条目就是
// 一个 ORPHAN。

/* ------------------------------------------------------------------ */
/* 7b. code 完整性 + 键语言完整性                         */

const codeLiterals = [];
for (const f of files) codeLiterals.push(...extractCodes(f));

const codeSeen = new Set();
const codes = codeLiterals.filter((c) => {
  const k = `${c.file}|${c.line}|${c.code}`;
  if (codeSeen.has(k)) return false;
  codeSeen.add(k);
  return true;
});

/** 一条渲染进程看得见的线路上的 code：json 面、code 表，或一个
 *  派生了 Serialize 的 struct 字段。其他一切（`code:` 落在一个永远
 *  不被序列化的 struct 上）单独上报，不参与把关。 */
const emittedCodes = codes.filter((c) => c.rule !== 'field' || c.serialized);
const internalCodes = codes.filter((c) => c.rule === 'field' && !c.serialized);

const codeMiss = emittedCodes.filter((c) => !Object.prototype.hasOwnProperty.call(zh.CODE_ZH || {}, c.code));
const badKeys = cjkKeys();

/* ------------------------------------------------------------------ */
/* 8. 报告                                                           */

const W = (s) => process.stdout.write(s + '\n');
const fmt = (r) => `    ${r.file}:${r.line}  "${r.s}"`;
/**
 * 靠人判断放进来的那些桶（而不是机械规则），总是完整列出——哪怕在
 * 关口模式下——这样它们就不会悄悄变大。机械性的桶只计数，除非加 --report。
 */
const ALWAYS_LIST = ['cli', 'protocol', 'logSurface', 'chinese'];
const show = (label, list, extra) => {
  W(`  ${label}: ${list.length}`);
  if (list.length && (reportOnly || ALWAYS_LIST.includes(label))) W(list.map(fmt).join('\n'));
  if (extra) W(extra);
};

W('');
W('Rust engine i18n audit — no-English-degradation gate');
W(`  source : ${RUST_SRC_DIRS.map((d) => path.relative(ROOT, d).replace(/\\/g, '/')).join(', ')}`);
W(`  dict   : ${path.relative(ROOT, DICT).replace(/\\/g, '/')}`);
W('');
W(`  rust files scanned        : ${files.length}`);
W(`  string literals extracted : ${all.length}`);
W('');
show('COVERED by EXACT', buckets.coveredExact, reportOnly ? undefined : '    (--report lists each)');
show('COVERED by PATTERNS', buckets.coveredPattern, reportOnly ? undefined : '    (--report lists each)');
show('COVERED by INTERNAL (never reaches the UI)', buckets.coveredInternal);
W('');
W('  -- MISS (fails the build) --');
W(`  MISS — English reaches the screen: ${buckets.miss.length}`);
if (buckets.miss.length) W(buckets.miss.map(fmt).join('\n'));
W('');
W('  -- excluded by scraping rule, not by the dictionary --');
show('cli', buckets.cli,
  '    ^ src/bin/*.rs: terminal output, never the WebView. File-scoped; watch this count.');
show('protocol', buckets.protocol);
show('logSurface', buckets.logSurface,
  '    ^ literals handed to a log sink (update manager `LogFn`, `log::*`, the\n' +
  `      ${LOG_PREFIXES.map((p) => `\`${p}\``).join(' / ')} channel prefix). Exempted by OUTPUT CHANNEL,\n` +
  '      not by file: production installs `|_| {}` as the sink, so these reach\n' +
  '      neither the WebView nor a terminal. Listed in full — watch this count.');
show('chinese', buckets.chinese,
  '    ^ hard-coded Chinese in the Rust engine. Renders correctly but bypasses\n' +
  '      the dictionary, so it cannot follow a UI-language switch. Engine-side\n' +
  '      debt, not a degradation of the guarantee.');
show('escaped', buckets.escaped);
show('notProse', buckets.notProse);
show('placeholder', buckets.placeholder);

const excluded = Object.keys(buckets)
  .filter((k) => !['miss', 'coveredExact', 'coveredPattern', 'coveredInternal'].includes(k))
  .reduce((n, k) => n + buckets[k].length, 0);
W('');
W(`  total excluded: ${excluded}   candidates judged: ${rows.length - excluded}`);
if (!reportOnly) W('  (--report lists every bucket)');

/* --- 词典侧报告 ----------------------------------------- */
W('');
W('  -- dictionary → engine (report only, nothing is deleted) --');
W(`  AMBIGUOUS-DUP (same key declared twice in EXACT): ${dupKeys.length}`);
for (const [k, n] of dupKeys) W(`    "${k}" declared ${n}x`);
if (dupKeys.length) {
  W('    ^ a duplicate object key silently wins in JS, so only one Chinese is');
  W('      ever used while the audit still counts the entry as covered.');
}
W(`  AMBIGUOUS-MULTI — FAILS THE BUILD (>1 PATTERNS rule renders one sentence): ${multi.length}`);
for (const { r, cands } of multi.slice(0, 10)) {
  W(`    ${r.file}:${r.line} "${r.s}"`);
  for (const c of cands) W(`        ${c.kind}: ${c.zh}`);
}
if (multi.length) {
  W('    ^ the first rule in PATTERNS wins; delete or reorder it and the same');
  W('      English silently renders as different Chinese.');
}
W(`  EXACT SHADOWING (expected — EXACT is checked before PATTERNS): ${shadowedByExact.length}`);
for (const { r, exact, pat } of shadowedByExact.slice(0, 10)) {
  W(`    ${r.file}:${r.line} "${r.s}"`);
  W(`        wins: EXACT "${exact}"   shadowed: PATTERNS#${pat.i} ${pat.src}`);
}
W(`  SHADOWED PATTERNS (can never win; an earlier rule matches first): ${shadowed.length}`);
for (const { c, losers } of shadowed.slice(0, 10)) {
  W(`    PATTERNS#${c.i} ${c.src}`);
  for (const l of losers.slice(0, 3)) W(`        loses to it on "${l.s}"`);
}
W(`  ORPHAN (dictionary entry no Rust source produces): ${orphans.length}`);
for (const o of orphans.slice(0, 10)) W(`    ${o.kind}: ${o.id}  →  ${o.zh}`);
if (orphans.length > 10) W(`    … ${orphans.length - 10} more (--report)`);
if (reportOnly) for (const o of orphans.slice(10)) W(`    ${o.kind}: ${o.id}  →  ${o.zh}`);

/* --- code / 键完整性 ------------------------------------------ */
W('');
W('  -- code integrity (Rust code literal → CODE_ZH) --');
W(`  code literals extracted from Rust : ${codes.length}` +
  `   (emitted ${emittedCodes.length}, struct-internal ${internalCodes.length})`);
W(`  CODE-MISS (fails the build — a code the renderer can see, untranslated): ${codeMiss.length}`);
for (const c of codeMiss) W(`    ${c.file}:${c.line}  "${c.code}"   [${c.rule}]`);
if (codeMiss.length) {
  W('    ^ add it to CODE_ZH in src/renderer/engine-zh.js, or stop emitting it.');
}
W('  CODE-INTERNAL (struct field `code`, struct does not derive Serialize:');
W('    cannot reach the renderer, reported not gated): ' + internalCodes.length);
for (const c of internalCodes) W(`    ${c.file}:${c.line}  "${c.code}"`);
if (internalCodes.length) {
  W('    ^ if the struct ever derives Serialize this flips to CODE-MISS.');
}
W(`  CJK-KEY (fails the build — Chinese in an English key slot): ${badKeys.length}`);
for (const b of badKeys) W(`    ${b.where}  "${b.key}"  →  ${b.zh}`);

if (buckets.miss.length) {
  W('');
  W('  Suggested dictionary additions (paste into src/renderer/engine-zh.js):');
  for (const r of buckets.miss) {
    if (/§\d/.test(r.s)) {
      const pat = `^${r.s}$`;
      const ok = zh.patternToRegExp(pat).test(r.s);
      W(`    // ${r.file}:${r.line}`);
      W(`    ['${pat.replace(/'/g, "\\'")}', '<中文>'],   // ${ok ? 'regex ok' : 'REGEX DOES NOT ROUND-TRIP'}`);
    } else {
      W(`    // ${r.file}:${r.line}`);
      W(`    '${r.s.replace(/'/g, "\\'")}': '<中文>',`);
    }
  }
}

const failed = buckets.miss.length + dupKeys.length + codeMiss.length + badKeys.length + multi.length;
if (failed) {
  W('');
  console.error(`[tauri-i18n] FAILED — ${buckets.miss.length} MISS, ${multi.length} ambiguous pattern(s), `
    + `${dupKeys.length} duplicate key(s), ${codeMiss.length} CODE-MISS, ${badKeys.length} CJK key(s).`);
  if (!reportOnly) process.exit(1);
  W('  (--report: exiting 0)');
} else {
  W('');
  W('[tauri-i18n] OK — every Rust engine sentence has a zh mapping.');
}
W('');
process.exit(0);
