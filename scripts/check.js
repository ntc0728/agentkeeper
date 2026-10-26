#!/usr/bin/env node
'use strict';
/**
 * 一道关口跑完全部——整个静态校验套件都在这一个文件里。
 *
 *   node scripts/check.js          # 全量
 *   node scripts/check.js residue  # 按名字跑单道关口
 *
 * Why a single entry point: one script, one口径 — every gate below either
 * 通过与否的标准，和评审者否决这次改动的标准完全一致。
 * 整套跑完远不到一秒，所以每次保存都可以跑。
 *
 * 关口（按开销从低到高排列）：
 *   1. residue   — 变异测试框架留下的变异标记与备份文件
 *   2. bridge    — 每一个 `window.ak` 调用的形状都要对上它调用的 Rust 命令
 *                  （见下文——这是最划算的一道关口）
 *   3. tree      — `target/` 必须待在体积上限之内（pdb 与缓存一律保留）；
 *                  孤立的构建目录只上报、不删除（这里曾经涨到 7.9 GB）
 *   4. renderer  — `scripts/lint-renderer.js`（i18n 对齐、CSP、变量遮蔽）
 *   5. ui        — `scripts/ui-audit.js`（死控件、主题违规）
 *   6. i18n      — `scripts/tauri-i18n.js`（Rust 句子 → zh 词典）
 *   7. dist      — `tauri-shell/copy-frontend.js --check`（产物包是否最新）
 *
 * ## 这道 bridge 关口为什么存在
 *
 * Tauri 是按*名字*从 invoke 载荷里解析命令参数的
 * （`tauri-2.12.4/src/ipc/command.rs`：`deserialize_json` → 报错 missing required
 * key）。所以如果桥接层直接把载荷对象本身传过去，而不是包成
 * `{ payload }`，运行时就会悄无声息地失败：Rust 命令拒绝这次调用，
 * 渲染进程又没有 `catch`，按钮看上去就像死了。恰恰有三个
 * 命令是以这种方式坏掉的（`save_skill`、`upsert_mcp`、
 * `install_skill`），而 371 个 Rust 测试始终是绿的，因为这些命令是
 * 私有函数，集成测试根本调不到。这个 bug 唯一能被看见的地方就是
 * 调用的*形状*，所以这道关口检查的正是形状。
 *
 * 退出码 0 = 全部关口通过，1 = 至少有一道失败。
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const ROOT = path.join(__dirname, '..');
const BRIDGE = path.join(ROOT, 'tauri-shell', 'tauri-bridge.js');
const LIB_RS = path.join(ROOT, 'tauri-shell', 'src-tauri', 'src', 'lib.rs');
const DIST_CHECK = path.join(ROOT, 'tauri-shell', 'copy-frontend.js');

const SELF = path.relative(ROOT, __filename).replace(/\\/g, '/');
const failures = [];

function fail(gate, msg) {
  failures.push({ gate, msg });
  console.error(`  FAIL  ${msg}`);
}

function ok(msg) {
  console.log(`  ok    ${msg}`);
}

/* ------------------------------------------------------------------ 关口 1 */

/** 变异测试框架可能把变异体留在这些目录树里。 */
const TREES = ['src', 'scripts', 'tauri-shell/src-tauri/src', 'tauri-shell/src-tauri/tests'];
const MARKERS = [
  { re: /MUTANT[-_]?\w*/g, why: 'mutation-test marker' },
  { re: /MUTATION\s+M\d+/g, why: 'mutation-test marker' }
];
/** 回退失败会留下这类文件，而 `cargo test` 会照样把它们编译进去。 */
const BACKUP_RE = /(\.bak(-[A-Za-z0-9_.-]+)?|\.orig|\.origbak|~)$/;

function gateResidue() {
  const found = [];
  const seen = { files: 0 };

  function walk(dir) {
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch (_e) {
      return; // tree may not exist (a directory can be retired)
    }
    for (const e of entries) {
      if (e.name === 'node_modules' || e.name === 'target' || e.name === '.git') continue;
      const p = path.join(dir, e.name);
      if (e.isDirectory()) { walk(p); continue; }
      const rel = path.relative(ROOT, p).replace(/\\/g, '/');
      if (rel === SELF) continue; // this file names the markers it hunts
      if (BACKUP_RE.test(e.name)) {
        found.push(`${rel}  backup file left behind`);
        continue;
      }
      if (!/\.(rs|js|mjs|cjs|ts|json|toml)$/.test(e.name)) continue;
      seen.files++;
      let text;
      try { text = fs.readFileSync(p, 'utf8'); } catch (_e) { continue; }
      text.split('\n').forEach((line, i) => {
        for (const m of MARKERS) {
          m.re.lastIndex = 0;
          const hit = m.re.exec(line);
          if (hit) found.push(`${rel}:${i + 1}  ${m.why}: ${hit[0]}`);
        }
      });
    }
  }

  for (const t of TREES) walk(path.join(ROOT, t));

  if (found.length) {
    for (const f of found) fail('residue', f);
    console.error('  A mutant in the tree is a source bug, not a flaky test — revert it by hand.');
    return;
  }
  ok(`residue: clean across ${TREES.length} trees (${seen.files} files)`);
}

/* ------------------------------------------------------------------ 关口 2 */

/** Tauri 从命令消息里解析出来的参数，而不是从载荷体里解析的。 */
const FRAMEWORK_ARGS = /^(?:\w+::)*(?:AppHandle|Window|WebviewWindow|Webview|State|Request|CommandScope|GlobalScope)\b|<.*\b(?:State|Request)>/;

function splitTopLevel(s) {
  const out = [];
  let depth = 0;
  let cur = '';
  for (const c of s) {
    if ('<([{'.includes(c)) depth++;
    else if ('>)]}'.includes(c)) depth--;
    if (c === ',' && depth === 0) { out.push(cur); cur = ''; continue; }
    cur += c;
  }
  if (cur.trim()) out.push(cur);
  return out;
}

/** 形如 `{ args: { assetId: x } }` 的对象字面量的顶层键。 */
function topLevelKeys(expr) {
  const keys = [];
  let depth = 0;
  let expectKey = false;
  let i = 0;
  while (i < expr.length) {
    const c = expr[i];
    if (c === '{') { depth++; if (depth === 1) expectKey = true; i++; continue; }
    if (c === '}' || c === ']' || c === ')') { depth--; i++; continue; }
    if (c === '[' || c === '(') { depth++; i++; continue; }
    if (depth === 1) {
      if (c === ',') { expectKey = true; i++; continue; }
      if (expectKey) {
        if (/\s/.test(c)) { i++; continue; }
        let key = '';
        if (c === '"' || c === "'") {
          const q = c;
          i++;
          while (i < expr.length && expr[i] !== q) key += expr[i++];
          i++;
        } else {
          while (i < expr.length && /[\w$]/.test(expr[i])) key += expr[i++];
        }
        if (key) { keys.push(key); expectKey = false; }
        continue;
      }
    }
    i++;
  }
  return keys;
}

const camel = (s) => s.replace(/_([a-z0-9])/g, (_m, c) => c.toUpperCase());

function gateBridge() {
  const bridge = fs.readFileSync(BRIDGE, 'utf8');
  const lib = fs.readFileSync(LIB_RS, 'utf8');

  /* 已注册命令：generate_handler! 列表 */
  const handler = /generate_handler!\s*\[([\s\S]*?)\]/.exec(lib);
  const registered = new Set(
    handler ? handler[1].split(',').map((s) => s.trim()).filter(Boolean) : []
  );
  if (!registered.size) return fail('bridge', `no generate_handler! list found in ${path.relative(ROOT, LIB_RS)}`);

  /* Rust 命令签名 */
  const commands = new Map();
  const cmdRe = /#\[tauri::command\][\s\S]{0,300}?\bfn\s+(\w+)\s*\(([\s\S]*?)\)\s*(?:->|\{)/g;
  let m;
  while ((m = cmdRe.exec(lib))) {
    const params = splitTopLevel(m[2]);
    const all = [];
    const required = [];
    for (const raw of params) {
      const p = raw.trim();
      if (!p) continue;
      const sep = p.indexOf(':');
      if (sep < 0) continue;
      const name = p.slice(0, sep).replace(/^mut\s+/, '').trim();
      const type = p.slice(sep + 1).trim();
      if (!name || name === '_') continue;
      if (FRAMEWORK_ARGS.test(type)) continue; // read from the command message
      all.push(camel(name));
      if (!/^Option\s*</.test(type)) required.push(camel(name));
    }
    commands.set(m[1], { all, required });
  }
  if (!commands.size) return fail('bridge', 'no #[tauri::command] functions parsed from lib.rs');

  /* 桥接调用点 */
  let calls = 0;
  bridge.split('\n').forEach((line, idx) => {
    const at = line.search(/call\('/);
    if (at < 0) return;
    const re = /call\('([A-Za-z0-9_]+)'(?:\s*,\s*([\s\S]*?))?\)/;
    const hit = re.exec(line.slice(at));
    if (!hit) return;
    const [, name, argExpr] = hit;
    const where = `${path.relative(ROOT, BRIDGE).replace(/\\/g, '/')}:${idx + 1}`;
    calls++;

    if (!registered.has(name)) {
      return fail('bridge', `${where}  call('${name}') is not in generate_handler!`);
    }
    const spec = commands.get(name);
    if (!spec) {
      return fail('bridge', `${where}  call('${name}') has no matching #[tauri::command] fn`);
    }
    // 参数全部来自命令消息（Request、AppHandle 等）的命令，把载荷当作
    // 一个不透明的请求体接收：没有键可查。
    if (!spec.all.length) return;

    const required = spec.required;
    const expr = (argExpr || '').trim();
    if (!expr) {
      if (required.length) {
        fail('bridge', `${where}  call('${name}') passes no args; Rust requires { ${required.join(', ')} }`);
      }
      return;
    }
    if (!expr.startsWith('{')) {
      if (required.length) {
        fail(
          'bridge',
          `${where}  call('${name}', ${expr}) passes a bare value; Rust requires the named ` +
            `argument(s) { ${required.join(', ')} } — the command will be rejected at runtime`
        );
      }
      return;
    }
    const keys = topLevelKeys(expr);
    const missing = required.filter((k) => !keys.includes(k));
    if (missing.length) {
      fail(
        'bridge',
        `${where}  call('${name}') sends { ${keys.join(', ')} }; Rust requires { ${required.join(', ')} }` +
          ` — missing ${missing.join(', ')}`
      );
    }
    // 可选参数也必须拼写正确：`opts: Option<…>` 对应的是 `{ opt }`，
    // 写成 `{ opt }` 之外的形式会反序列化成 None，功能就悄无声息地空转。
    const unknown = keys.filter((k) => !spec.all.includes(k));
    if (unknown.length) {
      fail(
        'bridge',
        `${where}  call('${name}') sends unknown key(s) ${unknown.join(', ')}; ` +
          `Rust accepts { ${spec.all.join(', ')} }`
      );
    }
  });

  /* 没人调用的命令——只作提示，不算失败 */
  const calledNames = new Set(
    [...bridge.matchAll(/call\('([A-Za-z0-9_]+)'/g)].map((x) => x[1])
  );
  const dead = [...registered].filter((c) => !calledNames.has(c));
  ok(`bridge: ${calls} call sites ↔ ${registered.size} registered commands, 0 shape mismatches`);
  if (dead.length) console.log(`        unused commands (${dead.length}): ${dead.join(', ')}`);
}

/* ------------------------------------------------------------- 关口 3–7 */

/**
 * gate: tree — `target/` 必须是有界的（BOUNDED），而不是干净的。
 *
 * 策略：构建产物一律保留。`*.pdb`、`*.rlib`、`*.rmeta`
 * 以及 `incremental/` 是链接器吐出来的，或者下次构建要读的；
 * 删掉它们确实省了磁盘，代价是每次重建都更慢。这道关口并不
 * 要求把它们清掉。
 *
 * 它要守的是「有界」。这棵树曾经涨到 7.9 GB（2.9 debug +
 * 5.0 release），原因是每次 Cargo.toml 变动都会留下一个孤立的
 * `build/<crate>-<hash>/`，而一个意料之外的增量缓存可能在所有人
 * 眼皮底下把它重新填满，而它变大的时候什么都不会失败。
 *
 *   HARD  总量 > CEILING_GIB       → 失败，并给出点名最大占用者的
 *                                     明细，好让修复有针对性
 *   INFO  pdb / 孤立的构建目录 / 增量缓存——带体积上报，这道关口
 *         永不删除它们，单凭它们也永远不算失败
 *
 * 本项目实测的基线（两个 profile 都算，release 增量缓存已预热）：
 * 不含 `.pdb` 为 4.5 GiB，含它们约 4.9 GiB。6.5 GiB 的上限既给
 * 多余一个 profile 或一套工具链留了余量，又仍然远低于 7.9 GB 那次
 * 事故——而且单独一个 2.5 GiB 的野增量缓存（目前见过的最坏情况）
 * 自己就能把它顶破。
 *
 * 下面两处配置锁定仍然是硬失败：`[build] incremental = false`
 * 一旦缺失或被放松，正是它悄无声息地又把那个缓存填了回去。
 */
function gateTree() {
  // 工作区共享的 target 目录：`tauri-shell/target`，与
  // `core/` 和 `src-tauri/` 并排——cargo 把每个工作区成员都构建到它里面。
  const TARGET = path.join(ROOT, 'tauri-shell', 'target');
  const REL = 'tauri-shell/target';
  const GIB = 1024 ** 3;
  /** 整棵树的体积上限。见关口说明：基线 4.5–4.9 GiB。 */
  const CEILING_GIB = 6.5;
  if (!fs.existsSync(TARGET)) {
    ok('tree: no target/ yet — nothing built, nothing to measure');
    return;
  }

  // 先把所有发现收集起来，再一次性上报。一个既失败*又*
  // 还打印自己 ok 行的关口比没有关口更糟：汇总读起来是 "ok"，
  // 失败信息就被刷走了。（这道关口第一次跑时就这样发生过。）
  const problems = [];

  /** 遍历时不跟随链接；对文件返回 {path,size,inode}。 */
  const files = [];
  const stack = [TARGET];
  while (stack.length) {
    const cur = stack.pop();
    let entries;
    try {
      entries = fs.readdirSync(cur, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const e of entries) {
      const full = path.join(cur, e.name);
      if (e.isDirectory()) stack.push(full);
      else if (e.isFile()) {
        try {
          const st = fs.statSync(full, { bigint: true });
          files.push({ path: full, size: Number(st.size), inode: `${st.dev}:${st.ino}` });
        } catch {
          /* 与并发构建撞车了 */
        }
      }
    }
  }

  /**
   * 累加字节数，同一个 inode 只算一次。
   *
   * cargo 会把 `target/<profile>/<name>.exe|.pdb` 与 `deps/` 下那个带哈希的
   * 副本硬链接在一起，所以朴素求和会把最终产物重复计算一遍。
   * 上报值和上限判定必须和 `du` 一致，而不是和目录项的数量一致。
   */
  const seenInode = new Set();
  const unique = files.filter((f) => {
    if (seenInode.has(f.inode)) return false;
    seenInode.add(f.inode);
    return true;
  });
  const sumOf = (pred) => unique.filter(pred).reduce((a, f) => a + f.size, 0);
  const totalBytes = unique.reduce((a, f) => a + f.size, 0);

  /* ---- 信息项：这棵树由什么构成（永远不算失败） ------------------ */
  const pdbBytes = sumOf((f) => f.path.toLowerCase().endsWith('.pdb'));
  const pdbCount = unique.filter((f) => f.path.toLowerCase().endsWith('.pdb')).length;
  const rlibBytes = sumOf((f) => f.path.endsWith('.rlib'));
  const rmetaBytes = sumOf((f) => f.path.endsWith('.rmeta'));
  const incBytes = sumOf((f) => f.path.split(path.sep).includes('incremental'));

  /* 孤立的 build-script 输出目录（按哈希精确匹配，规则与
     tauri-shell/clean-build.js 相同——只按 crate 名字匹配会把所有历史哈希
     都当成活的，从而把增长藏起来） */
  const buildRoot = path.join(TARGET, 'release', 'build');
  const fpRoot = path.join(TARGET, 'release', '.fingerprint');
  let fps = [];
  try {
    fps = fs.readdirSync(fpRoot);
  } catch {
    /* 还没构建过 */
  }
  const fpHashes = new Set(
    fps.map((f) => (f.match(/-([0-9a-f]{16})$/) || [])[1]).filter(Boolean)
  );
  let orphanDirs = [];
  try {
    orphanDirs = fs
      .readdirSync(buildRoot, { withFileTypes: true })
      .filter((e) => e.isDirectory())
      .map((e) => e.name)
      .filter((name) => {
        const hash = (name.match(/-([0-9a-f]{16})$/) || [])[1];
        return !hash || !fpHashes.has(hash);
      });
  } catch {
    /* 没有 build 目录 */
  }
  const orphanBytes = orphanDirs.reduce((a, name) => {
    const prefix = path.join(buildRoot, name) + path.sep;
    return a + unique.filter((f) => f.path.startsWith(prefix)).reduce((x, f) => x + f.size, 0);
  }, 0);

  /* ---- 硬失败：体积上限 ------------------------------------------------- */
  const gib = (b) => (b / GIB).toFixed(2);
  if (totalBytes > CEILING_GIB * GIB) {
    const reclaim = [];
    if (orphanDirs.length) {
      reclaim.push(
        `${orphanDirs.length} orphaned build dir(s), ${gib(orphanBytes)} GiB — no .fingerprint references them`
      );
    }
    reclaim.push('a stray incremental cache (debug, or a second release profile)');
    reclaim.push('`cargo clean -p <crate>` for a crate whose source changed shape');
    reclaim.push('rlib+rmeta last — they are the compile cache, deleting them means a full rebuild');
    problems.push(
      `${REL} is ${gib(totalBytes)} GiB, over the ${CEILING_GIB} GiB ceiling.\n` +
        `        breakdown: rlib ${gib(rlibBytes)} · rmeta ${gib(rmetaBytes)} · ` +
        `pdb ${gib(pdbBytes)} · incremental ${gib(incBytes)} · rest ${gib(totalBytes - rlibBytes - rmetaBytes - pdbBytes - incBytes)} GiB\n` +
        `        reclaim in this order, largest stale item first:\n` +
        reclaim.map((line, i) => `          ${i + 1}. ${line}`).join('\n')
    );
  }

  /* ---- 信息行（只有在什么都没失败时才打印） ----------- */
  const notes = [];
  if (pdbBytes) {
    notes.push(
      `tree: keeps ${pdbCount} .pdb (${gib(pdbBytes)} GiB) — linker symbols, kept as artefacts`
    );
  }
  if (orphanDirs.length) {
    notes.push(
      `tree: ${orphanDirs.length} orphaned build dir(s) (${gib(orphanBytes)} GiB) — ` +
        `left behind by Cargo.toml hash changes; \`npm run tauri:clean\` can reclaim them`
    );
  }
  if (incBytes) {
    notes.push(`tree: incremental cache ${gib(incBytes)} GiB — kept (hot pack builds depend on it)`);
  }

  /* 守住干净的那两处配置锁定自己也必须原地不动 */
  const cfg = path.join(ROOT, 'tauri-shell', '.cargo', 'config.toml');
  let cfgText = '';
  try {
    cfgText = fs.readFileSync(cfg, 'utf8');
  } catch {
    problems.push(
      'tauri-shell/.cargo/config.toml is missing — the workspace would fall back\n' +
        '        to the machine-wide [build] incremental = true and refill the cache.'
    );
  }
  if (cfgText && !/^\[build\][\s\S]*?^incremental\s*=\s*false/m.test(cfgText)) {
    problems.push(
      'tauri-shell/.cargo/config.toml no longer pins [build] incremental = false.\n' +
        '        The [profile.dev] key is NOT equivalent — a [build] incremental anywhere in\n' +
        '        the config chain wins over [profile.*] (measured, cargo 1.99.1).'
    );
  }

  if (problems.length) {
    for (const p of problems) {
      fail('tree', p);
    }
    return;
  }

  // 每条说明一行 ok，然后才是汇总。一个失败的关口绝不能打印自己的 ok 行
  //（这个顺序 bug 曾经被修过一次）；这里每条说明都只在上方的成功路径上打印。
  for (const n of notes) ok(n);
  const cacheRlib = unique.filter((f) => f.path.endsWith('.rlib')).length;
  ok(
    `tree: ${gib(totalBytes)} GiB of ${CEILING_GIB} GiB ceiling ` +
      `(kept ${cacheRlib} .rlib + matching .rmeta as compile cache)`
  );
}

/* ------------------------------------------------------------- 关口：untracked */

/**
 * gate: untracked — 构建产物绝不能被 git 跟踪。
 *
 * `tauri-shell/dist/` 是由 `tauri-shell/copy-frontend.js` 从
 * `src/renderer` 生成的，而 `cargo tauri build` 会把 `dist/` 里有的东西
 * 直接编译进去，并不检查它是否与源码一致。所以一份被跟踪的副本
 * 是个陷阱：它可能比它声称要镜像的渲染进程更旧，而可执行文件就带着旧 UI
 * 出货了。`tauri.conf.json` 把生成器配成 `beforeBuildCommand`，所以不跟踪
 * 它不会损失什么——而且 `release/` 里的产物同样是从源码重建的。
 *
 * 检查的是索引（`git ls-files`），而不是 `.gitignore`：一个文件可以被忽略
 * 却仍然被跟踪，而这恰恰就是值得抓的那一类情况。
 */
const MUST_BE_UNTRACKED = ['tauri-shell/dist/', 'release/', 'tauri-shell/target/'];

function gateUntracked() {
  const r = spawnSync('git', ['ls-files'], { cwd: ROOT, encoding: 'utf8' });
  if (r.status !== 0) {
    // 没有 git（tarball 导出、全新容器）：没什么可查的。
    ok('untracked: no git index — skipped');
    return;
  }
  const tracked = r.stdout.split('\n').filter(Boolean);
  const problems = [];
  for (const prefix of MUST_BE_UNTRACKED) {
    const hits = tracked.filter((f) => f === prefix.replace(/\/$/, '') || f.startsWith(prefix));
    if (hits.length) {
      problems.push(
        `${hits.length} file(s) under ${prefix} are tracked by git: ${hits.slice(0, 3).join(', ')}` +
          (hits.length > 3 ? ', …' : '') +
          '\n        these are build artefacts; a tracked copy can be stale and the build ' +
          'does not check\n        freshness. Untrack them: git rm --cached -r <path>'
      );
    }
  }
  if (problems.length) {
    for (const p of problems) fail('untracked', p);
    return;
  }
  ok(`untracked: ${MUST_BE_UNTRACKED.length} artefact trees are not in the git index`);
}

/** 在进程内跑一个仓库脚本；退出码为 0 时返回 true。 */
function runScript(gate, script, args, label) {
  const r = spawnSync(process.execPath, [script, ...(args || [])], { cwd: ROOT, stdio: 'inherit' });
  if (r.status === 0) {
    ok(label);
    return;
  }
  fail(gate, `${path.relative(ROOT, script)} exited ${r.status}`);
}

/* -------------------------------------------------------------------- 主流程 */

const GATES = {
  residue: gateResidue,
  bridge: gateBridge,
  tree: gateTree,
  renderer: () => runScript('renderer', path.join(ROOT, 'scripts', 'lint-renderer.js'), [], 'renderer: lint + i18n parity clean'),
  ui: () => runScript('ui', path.join(ROOT, 'scripts', 'ui-audit.js'), [], 'ui: no dead controls / theme violations'),
  i18n: () => runScript('i18n', path.join(ROOT, 'scripts', 'tauri-i18n.js'), [], 'i18n: every Rust sentence has a zh mapping'),
  dist: () => runScript('dist', DIST_CHECK, ['--check'], 'dist: tauri-shell/dist matches src/renderer'),
  // `tauri-shell/dist/` 是构建产物。如果它又被跟踪了，一份过期的
  // 副本可能被提交进去，然后被编译进可执行文件——构建过程并不检查新鲜度。
  // tauri.conf.json 里的 `beforeBuildCommand` 每次构建都会重新生成它，
  // 所以跟踪它没有任何好处，却有出货旧 UI 的风险。
  untracked: gateUntracked
};

const wanted = process.argv.slice(2).filter((a) => !a.startsWith('-'));
const names = wanted.length ? wanted : Object.keys(GATES);
for (const n of names) {
  if (!GATES[n]) {
    console.error(`unknown gate "${n}" — available: ${Object.keys(GATES).join(', ')}`);
    process.exit(2);
  }
}

console.log(`[check] ${names.join(' → ')}\n`);
for (const n of names) {
  try {
    GATES[n]();
  } catch (e) {
    fail(n, `${n} threw: ${e && e.message}`);
  }
}

const gateCount = names.length;
if (failures.length) {
  console.error(`\n[check] FAILED — ${failures.length} finding(s) in ${gateCount} gate(s)`);
  process.exit(1);
}
console.log(`\n[check] all ${gateCount} gates passed`);
