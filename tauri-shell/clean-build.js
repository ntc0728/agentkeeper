#!/usr/bin/env node
'use strict';
/**
 * 从 Rust 构建目录回收磁盘空间——但不动那些重新构建时必须重新生成的产物。
 *
 *   node tauri-shell/clean-build.js                 # 仅报告
 *   node tauri-shell/clean-build.js --apply         # 删除孤立的构建目录
 *   node tauri-shell/clean-build.js --apply --pdbs  # ……以及链接器 .pdb 文件
 *   node tauri-shell/clean-build.js --apply --all   # ……以及增量编译缓存
 *
 * 策略：构建产物一律保留。`*.rlib`、`*.rmeta`、`*.pdb`
 * 与 `incremental/` 都是输出而非残留——删掉它们只是把磁盘占用
 * 换成了下一次构建变慢。本脚本默认删除的只是任何构建都再也
 * 用不上的残渣：其 crate hash 已不再被任何 `.fingerprint` 引用的
 * 构建脚本输出目录（每次 Cargo.toml 变动会遗留一个，每个约 100 MB）。
 *
 * 这些可选开关是给确实需要把磁盘空间抢回来的机器准备的：
 *
 *   --pdbs   build/ 与 deps/ 下的 `*.pdb`。这是链接器输出，任何构建都不会读它；
 *            本项目不挂接原生调试器。约 0.2-0.4 GiB。
 *   --all    同时删除各 profile 下的 `incremental/`。RELEASE 缓存占
 *            约 0.8 GiB，但 `package-release.js` 依赖它来达成 18 秒的热
 *            打包构建——删掉它会让下一次打包回到约 80 秒。
 *
 * 任何开关都不会删除：`*.rlib` + `*.rmeta`（编译缓存：每个依赖一对匹配项；
 * 删掉意味着重新编译全部约 300 个 crate），以及第三方构建脚本的 `out/`
 * 目录（webview2-com-sys 附带的 WebView2LoaderStatic.lib 的 mtime 是 2006 年
 * ——那是已发布 crate 的时间戳，并非陈旧残留）。
 *
 * 目录体积上限写在 `scripts/check.js`（`gateTree`）里：那个关卡在
 * `target/` 超过 6.5 GiB 时失败，这是防止无节制增长的护栏——而不是本脚本
 * 的默认行为。
 *
 * 删除使用 PowerShell 的 `Remove-Item`，而不是 fs.rmSync：后者在这棵目录树上
 * 会卡住（此处 Node 的 fs 会把删除走回收站；成千上万个文件要花几分钟而不是
 * 几秒）。
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

// 工作区根目录是 `tauri-shell/`，cargo 的共享 target 目录
// 位于 `core/` 与 `src-tauri/` 旁边——不在它们内部。
const WORKSPACE = __dirname;
const TARGET = path.join(WORKSPACE, 'target');
const APPLY = process.argv.includes('--apply');
const WITH_PDBS = process.argv.includes('--pdbs') || process.argv.includes('--all');
const WITH_INCREMENTAL = process.argv.includes('--all');

/** `dir` 下匹配 `pred` 的所有文件，形如 {path, size}。目录不存在则返回 []。 */
function walk(dir, pred) {
  const out = [];
  const stack = [dir];
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
      else if (e.isFile() && pred(full)) {
        try {
          out.push({ path: full, size: fs.statSync(full).size });
        } catch {
          /* 与并发的构建发生竞争；跳过 */
        }
      }
    }
  }
  return out;
}

/**
 * 求和前先把硬链接合并。
 *
 * cargo 会把 `target/release/<name>.pdb` 与
 * `target/release/deps/<name>-<hash>.pdb` 硬链接在一起（链接数为 2）。两个条目
 * 都计入会重复上报字节数：一次 dry run 列出 128 个 pdb 条目 / 321 MB，
 * 而 `du` 与 inode 统计一致地给出 74 个文件 / 199 MB。按路径删除仍然是对的
 * （最后一个链接会释放数据块），但上报的*可回收*数字不能被夸大——
 * 一个夸大自身价值的清理脚本还不如没有。
 */
function dedupeByInode(files) {
  const seen = new Set();
  const out = [];
  for (const f of files) {
    let key = null;
    try {
      const st = fs.statSync(f.path, { bigint: true });
      key = `${st.dev}:${st.ino}`;
    } catch {
      continue; // vanished between walk and stat
    }
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(f);
  }
  return out;
}

function mb(bytes) {
  return `${(bytes / 1048576).toFixed(1)} MB`;
}

/** 把一组路径归入顶层的 build/ 或 deps/ 分组。 */
function byBucket(files) {
  const g = new Map();
  for (const f of files) {
    const rel = path.relative(TARGET, f.path).split(path.sep);
    const key = rel[0] + '/' + (rel[1] || '');
    const cur = g.get(key) || { size: 0, n: 0 };
    cur.size += f.size;
    cur.n += 1;
    g.set(key, cur);
  }
  return g;
}

// -- 1. pdb 文件（需显式开启：--pdbs / --all）-------------------------------
// 这是链接器输出而非残留——除非机器确实需要腾出磁盘，否则保留。
const pdbsAll = WITH_PDBS ? walk(TARGET, (p) => p.toLowerCase().endsWith('.pdb')) : [];
const pdbs = dedupeByInode(pdbsAll);
/** 即使不删除也会统计体积，这样报告才能说明它占多少。 */
const pdbAllBytes = dedupeByInode(walk(TARGET, (p) => p.toLowerCase().endsWith('.pdb')))
  .reduce((a, f) => a + f.size, 0);

// -- 2. 孤立的构建脚本输出目录 ---------------------------------------------
// `build/<crate>-<hash>/` 当且仅当 `.fingerprint/<crate>-<hash>/` 存在时才算活跃。
// Cargo 会为该 crate 的每个构建单元复用同一个 fingerprint 目录；构建目录成为
// 孤儿意味着已没有任何单元引用那个 hash。
const buildRoot = path.join(TARGET, 'release', 'build');
const fpRoot = path.join(TARGET, 'release', '.fingerprint');
const orphans = [];
let fpNames = new Set();
try {
  fpNames = new Set(fs.readdirSync(fpRoot));
} catch {
  /* 还没有任何 fingerprint */
}
let buildDirs = [];
try {
  buildDirs = fs.readdirSync(buildRoot, { withFileTypes: true })
    .filter((e) => e.isDirectory())
    .map((e) => e.name);
} catch {
  /* 从未构建过 */
}
for (const name of buildDirs) {
  // 比较完整名称（crate + hash）。只按 crate 名匹配会把每个历史 hash
  // 都算成"活跃"：目录树里曾有
  // agentkeeper-shell-{0dab…, ab74…, d8bc…} 这些构建目录，而按 crate 名匹配
  // 会报出零个孤儿，因为至少总有一个能匹配上。
  //
  // 只要*任意* fingerprint 目录带着它的 hash，该构建目录就仍然需要——
  // 同一个 crate 会被编译成多个单元（lib / bin / test），而它们
  // 共用同一个构建脚本输出目录。
  const hash = (name.match(/-([0-9a-f]{16})$/) || [])[1];
  const live = hash && [...fpNames].some((f) => f.endsWith('-' + hash));
  if (!live) {
    const dir = path.join(buildRoot, name);
    const files = walk(dir, () => true);
    orphans.push({ path: dir, size: files.reduce((a, f) => a + f.size, 0) });
  }
}

// -- 3. 增量编译缓存（需显式开启：--all）-----------------------------------
// RELEASE 缓存正是 package-release.js 依赖的对象，用来达成 18 秒的热打包
// 构建（打包路径上 CARGO_INCREMENTAL=1）；debug 缓存则毫无价值
// （开发重建两种情况都只要约 4 秒）。两者都是缓存，所以默认都不删除
// ——`--all` 是给需要拿回那约 1 GiB 的机器用的。
const incEntries = WITH_INCREMENTAL
  ? ['release', 'debug']
      .map((p) => path.join(TARGET, p, 'incremental'))
      .filter((p) => {
        try {
          return fs.readdirSync(p).length > 0;
        } catch {
          return false;
        }
      })
      .map((p) => ({ path: p, size: walk(p, () => true).reduce((a, f) => a + f.size, 0) }))
  : [];
/** 无论是否删除都会统计，这样报告可以显示这些缓存占多少。 */
const incAllBytes = ['release', 'debug']
  .map((p) => path.join(TARGET, p, 'incremental'))
  .reduce((a, p) => {
    try {
      return a + walk(p, () => true).reduce((x, f) => x + f.size, 0);
    } catch {
      return a;
    }
  }, 0);

// -- 报告 ------------------------------------------------------------------
const pdbBytes = pdbs.reduce((a, f) => a + f.size, 0);
const orphanBytes = orphans.reduce((a, f) => a + f.size, 0);
const incBytes = incEntries.reduce((a, e) => a + e.size, 0);

console.log(`target root : ${TARGET}`);
console.log(`mode        : ${APPLY ? 'APPLY (deleting)' : 'dry-run (pass --apply to delete)'}`);
console.log('');

if (WITH_PDBS) {
  console.log(`pdb files            ${String(pdbs.length).padStart(5)} files  ${mb(pdbBytes).padStart(10)}`);
  for (const [k, v] of [...byBucket(pdbs)].sort((a, b) => b[1].size - a[1].size)) {
    console.log(`  ${k.padEnd(20)} ${String(v.n).padStart(5)} files  ${mb(v.size).padStart(10)}`);
  }
} else {
  console.log(`pdb files            ${'kept'.padStart(5)}         ${mb(pdbAllBytes).padStart(10)}  (artefacts; --pdbs to drop)`);
}

console.log(`orphaned build dirs  ${String(orphans.length).padStart(5)} dirs   ${mb(orphanBytes).padStart(10)}`);
for (const o of orphans) {
  console.log(`  ${path.basename(o.path).padEnd(40)} ${mb(o.size).padStart(10)}`);
}

if (WITH_INCREMENTAL && incEntries.length) {
  console.log('');
  console.log(`incremental cache    ${String(incEntries.length).padStart(5)} dirs   ${mb(incBytes).padStart(10)}`);
  for (const e of incEntries) console.log(`  ${e.path}`);
} else {
  console.log(`incremental cache    ${'kept'.padStart(5)}         ${mb(incAllBytes).padStart(10)}  (caches; --all to drop)`);
}

console.log('');
console.log(`reclaimable  ${mb(pdbBytes + orphanBytes + incBytes)}  (this run's selection)`);
console.log('kept         *.rlib + *.rmeta — the compile cache, never removed');

if (!APPLY) {
  console.log('');
  console.log('Nothing deleted. Re-run with --apply.');
  process.exit(0);
}

// -- 删除 ------------------------------------------------------------------
// 每个路径调用一次 Remove-Item：PowerShell 处理这棵目录树只要几秒，而
// fs.rmSync 会卡住（见文件头说明）。
//
// 删除用 `pdbsAll`，而不是去重后的 `pdbs`：target/release/ 下的 pdb 与
// deps/ 下的孪生文件是指向同一个 inode 的两个目录项，所以只删掉
// 其中一个路径什么也释放不了，文件还在。报告使用去重后的
// 列表；删除则必须触及每一个链接。
const victims = [
  ...pdbsAll.map((f) => f.path),
  ...orphans.map((o) => o.path),
  ...incEntries.map((e) => e.path),
];
if (!victims.length) {
  console.log('');
  console.log('nothing to delete');
  process.exit(0);
}

// 使用 -LiteralPath，这样含 [ 或 ] 的路径（这里没有，但比留下一个 bug 更划算）
// 不会被当成通配符。分批执行以保持在 Windows 命令行长度上限之内。
const BATCH = 40;
let failed = 0;
for (let i = 0; i < victims.length; i += BATCH) {
  const chunk = victims.slice(i, i + BATCH);
  const ps = chunk
    .map((p) => `Remove-Item -LiteralPath '${p.replace(/'/g, "''")}' -Recurse -Force -ErrorAction SilentlyContinue`)
    .join('; ');
  const r = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', ps], {
    stdio: 'inherit',
  });
  if (r.status !== 0) failed += chunk.length;
}

// 校验而不是相信退出码。
const leftPdb = walk(TARGET, (p) => p.toLowerCase().endsWith('.pdb')).length;
console.log('');
console.log(`deleted ${victims.length - failed} path(s); ${leftPdb} pdb file(s) remain`);
process.exit(failed ? 1 : 0);
