#!/usr/bin/env node
'use strict';
/**
 * 只跑一次改动可能影响到的测试。
 *
 * `cargo test --workspace` 要花约 7 s，跑 317 个测试。迭代过程中
 * 那是错误的默认值：改 core 里的一个模块不该把更新子系统那 39 个集成测试
 * 重跑一遍。
 *
 * 这里把改动的文件映射到覆盖它们的测试目标：
 *
 *   core/src/update/**   →  core 单元测试（update 模块）+ tests/update.rs
 *                          + tests/boundary.rs（它在驱动更新管理器）
 *   core/src/store.rs    →  core 单元测试（store）+ 每一个集成套件
 *                          （store 是它们全都写穿过去的那层基底）
 *   src-tauri/src/lib.rs →  shell 自己的单元测试 + 每一个集成套件
 *   src/renderer/**      →  没有 Rust 测试；由渲染进程的那些关口负责（npm run check）
 *
 * 不带参数时它会跑全部，所以拿来当默认值是安全的。
 * 带 `--changed <ref>` 时它对着一个 git ref 做 diff 而不是对着工作区，
 * 这正是 CI 想要的。
 *
 * 用法：
 *   node scripts/test-affected.js                 # 全部
 *   node scripts/test-affected.js                 # 受未提交改动影响的
 *   node scripts/test-affected.js --changed HEAD~1
 *   node scripts/test-affected.js --all           # 忽略映射，跑全部
 *   node scripts/test-affected.js --list          # 只打印计划，什么都不跑
 *
 * 退出码 0 = 选中的测试都通过；非零 = 有测试失败。
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..');
const SHELL = path.join(ROOT, 'tauri-shell');

const argv = process.argv.slice(2);
const has = (f) => argv.includes(f);
const opt = (name) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : null;
};

const RUN_ALL = has('--all');
const LIST_ONLY = has('--list');
const CHANGED_REF = opt('--changed');

/**
 * `cargo` 的可执行路径。
 *
 * 这台机器上 rustup 装完后 `.cargo\bin` 不在用户 PATH（`scripts/rust-env.sh` 的
 * 第一段注释就写着这个原因）。之前的表现：不先 `source scripts/rust-env.sh` 就
 * 跑 `npm test`，每一个 Rust 目标都会以 `'cargo' 不是内部或外部命令` 失败——
 * 而渲染层的两道静态关是绿的，于是整条命令看起来像「测试有问题」，而不是
 * 「环境没接上」。脚本应当自己找到它，而不是要求调用者记得先 source。
 */
function resolveCargo() {
  const exe = process.platform === 'win32' ? 'cargo.exe' : 'cargo';
  const fromPath = spawnSync(exe, ['--version'], { encoding: 'utf8' });
  if (fromPath.status === 0) return exe;
  const home = process.env.USERPROFILE || process.env.HOME || '';
  const candidates = [
    process.env.CARGO_HOME && path.join(process.env.CARGO_HOME, 'bin', exe),
    home && path.join(home, '.cargo', 'bin', exe),
  ].filter(Boolean);
  for (const c of candidates) {
    if (!fs.existsSync(c)) continue;
    const probe = spawnSync(c, ['--version'], { encoding: 'utf8' });
    if (probe.status === 0) return c;
  }
  return null;
}
const CARGO = resolveCargo();

/* --------------------------------------------------------------- 映射表 */

/**
 * 哪个测试目标覆盖哪块源码区域。
 *
 * `core` 单元测试是这个 crate 自己的 `#[cfg(test)]` 模块——cargo 只能
 * 整体运行它们（`-p agentkeeper-core --lib`），所以 core 这边能拿到的
 * 最细粒度就是"core 的单元测试"。
 *
 * 集成套件是各自独立的二进制，可以用 `--test <name>`
 * 单独选中。
 */
const AREAS = [
  {
    name: 'core',
    // 改动这些文件可能会弄坏 core 自己的单元测试。
    match: (f) => f.startsWith('tauri-shell/core/src/'),
    tests: [{ pkg: 'agentkeeper-core', args: ['--lib'] }],
  },
  {
    name: 'update',
    match: (f) => /tauri-shell\/core\/src\/update\//.test(f) || /tauri-shell\/core\/src\/http\.rs$/.test(f),
    tests: [
      { pkg: 'agentkeeper-core', args: ['--lib'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'update'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'boundary'] },
    ],
  },
  {
    name: 'validate',
    match: (f) => /tauri-shell\/core\/src\/validate\.rs$/.test(f) || /tauri-shell\/core\/src\/security\.rs$/.test(f) || /tauri-shell\/core\/src\/yaml\.rs$/.test(f),
    tests: [
      { pkg: 'agentkeeper-core', args: ['--lib'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'write_path'] },
    ],
  },
  {
    name: 'store',
    match: (f) => /tauri-shell\/core\/src\/store\.rs$/.test(f),
    // store 是每一个集成套件写穿过去的那层基底。
    tests: [
      { pkg: 'agentkeeper-core', args: ['--lib'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'contract'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'update'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'boundary'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'write_path'] },
      { pkg: 'agentkeeper-core', args: ['--test', 'mcp'] },
    ],
  },
  {
    name: 'keeper',
    match: (f) => /tauri-shell\/core\/src\/keeper\.rs$/.test(f) || /tauri-shell\/core\/src\/assets\.rs$/.test(f),
    tests: [
      { pkg: 'agentkeeper-core', args: ['--lib'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'contract'] },
      { pkg: 'agentkeeper-core', args: ['--test', 'mcp'] },
    ],
  },
  {
    name: 'scan',
    match: (f) => /tauri-shell\/core\/src\/scan\.rs$/.test(f) || /tauri-shell\/core\/src\/discover\.rs$/.test(f) || /tauri-shell\/core\/src\/adapters\.rs$/.test(f),
    tests: [
      { pkg: 'agentkeeper-core', args: ['--lib'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'contract'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'boundary'] },
    ],
  },
  {
    name: 'shell',
    match: (f) => f.startsWith('tauri-shell/src-tauri/src/'),
    tests: [
      { pkg: 'agentkeeper-shell', args: ['--lib'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'contract'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'update'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'boundary'] },
      { pkg: 'agentkeeper-shell', args: ['--test', 'write_path'] },
      { pkg: 'agentkeeper-core', args: ['--test', 'mcp'] },
    ],
  },
  {
    name: 'renderer',
    match: (f) => f.startsWith('src/renderer/') || f.startsWith('tauri-shell/dist/'),
    // 没有 Rust 测试覆盖渲染进程；那些静态关口负责。
    tests: [],
  },
];

const ALL_TESTS = [
  { pkg: 'agentkeeper-core', args: ['--lib'] },
  { pkg: 'agentkeeper-core', args: ['--test', 'mcp'] },
  { pkg: 'agentkeeper-shell', args: ['--lib'] },
  { pkg: 'agentkeeper-shell', args: ['--test', 'contract'] },
  { pkg: 'agentkeeper-shell', args: ['--test', 'update'] },
  { pkg: 'agentkeeper-shell', args: ['--test', 'boundary'] },
  { pkg: 'agentkeeper-shell', args: ['--test', 'write_path'] },
];

/* ------------------------------------------------------------- 改动的文件 */

function changedFiles() {
  if (CHANGED_REF) {
    const r = spawnSync('git', ['diff', '--name-only', CHANGED_REF, '--'], {
      cwd: ROOT,
      encoding: 'utf8',
    });
    if (r.status !== 0) {
      console.error(`git diff against ${CHANGED_REF} failed: ${r.stderr}`);
      process.exit(2);
    }
    return r.stdout.split('\n').filter(Boolean);
  }
  const r = spawnSync('git', ['status', '--porcelain'], { cwd: ROOT, encoding: 'utf8' });
  if (r.status !== 0) {
    console.error('git status failed');
    process.exit(2);
  }
  const out = [];
  for (const line of r.stdout.split('\n')) {
    if (!line.trim()) continue;
    // "XY path"——路径从 3 个字符的状态字段之后开始。
    const p = line.slice(3).trim().replace(/^"|"$/g, '');
    if (p) out.push(p);
  }
  return out;
}

/** 改动清单文件、锁文件或构建脚本，可能弄坏任何东西。 */
function isGlobalChange(f) {
  return (
    /Cargo\.toml$/.test(f) ||
    /Cargo\.lock$/.test(f) ||
    /build\.rs$/.test(f) ||
    /tauri\.conf\.json$/.test(f) ||
    /tauri-shell\/Cargo\.toml$/.test(f)
  );
}

/* ------------------------------------------------------------------ 计划 */

function planFor(files) {
  if (RUN_ALL) return { tests: ALL_TESTS, areas: ['all'], gates: false, global: true };

  const areas = new Set();
  const tests = [];
  let gates = false;
  let global = false;

  for (const f of files) {
    if (isGlobalChange(f)) {
      global = true;
      break;
    }
    for (const a of AREAS) {
      if (!a.match(f)) continue;
      areas.add(a.name);
      for (const t of a.tests) tests.push(t);
      if (a.name === 'renderer') gates = true;
    }
  }

  if (global) {
    return { tests: ALL_TESTS, areas: ['all'], gates: true, global: true };
  }
  if (areas.size === 0) {
    return { tests: [], areas: [], gates: false, global: false };
  }
  // 去重，保留顺序
  const seen = new Set();
  const uniq = [];
  for (const t of tests) {
    const key = t.pkg + ' ' + t.args.join(' ');
    if (seen.has(key)) continue;
    seen.add(key);
    uniq.push(t);
  }
  return { tests: uniq, areas: [...areas], gates, global: false };
}

/* ------------------------------------------------------------------- 运行 */

function key(t) {
  return t.pkg + ' ' + t.args.join(' ');
}

function main() {
  const files = changedFiles();
  const plan = planFor(files);

  console.log('changed files:');
  for (const f of files.slice(0, 20)) console.log('  ' + f);
  if (files.length > 20) console.log(`  … and ${files.length - 20} more`);
  console.log('');
  console.log('areas      : ' + (plan.areas.join(', ') || '(none)'));
  console.log('test targets:');
  for (const t of plan.tests) console.log('  ' + key(t));
  if (plan.gates) console.log('static gates: npm run check   (the renderer changed)');
  console.log('');

  if (LIST_ONLY) {
    console.log('--list: nothing was run');
    return;
  }
  if (plan.tests.length === 0 && !plan.gates) {
    console.log('no Rust test covers the changed files — nothing to run.');
    return;
  }

  let failed = 0;
  for (const t of plan.tests) {
    console.log('=== cargo test -p ' + t.pkg + ' ' + t.args.join(' ') + ' ===');
    if (!CARGO) {
      failed++;
      console.error('FAILED: ' + key(t));
      console.error('  cargo not found — install Rust or set CARGO_HOME/PATH.');
      console.error('  On this repo: source scripts/rust-env.sh (Git Bash) or scripts/rust-env.ps1.');
      continue;
    }
    // `shell: true` 仅用于 npm/`.cmd` shim；cargo 已解析成绝对路径，两种情形都交给
    // 同一个调用，避免「有的目标能跑、有的不能」。参数全部来自本文件的字面量与映射表。
    const r = spawnSync(CARGO, ['test', '-p', t.pkg, ...t.args, '--offline'], {
      cwd: SHELL,
      stdio: 'inherit',
    });
    if (r.status !== 0) {
      failed++;
      console.error('FAILED: ' + key(t));
      if (r.error) console.error('  spawn error: ' + r.error.code + ' — ' + r.error.message);
    }
  }

  if (plan.gates) {
    // 渲染进程变了，所以在它被重新生成之前 `dist/` 是过期的——
    // 而 `dist` 那道关口正好会因此失败。先同步，再过关。
    //
    // `shell: true` 是 Windows 上的必要条件：Node 从 18 起拒绝在没有 shell 的
    // 情况下 spawn `.cmd`（EACCES/ENOENT），而 npm 在 Windows 上就是 `npm.cmd`。
    // 缺这一条的表现是「每次改渲染层，`npm test` 都报 2 个 target 失败」——
    // 而两道关单独跑都是绿的，于是失败被当成噪声忽略。参数全部是本文件写死的
    // 字面量，没有用户输入经过这条命令。
    console.log('=== npm run tauri:sync (the renderer changed) ===');
    const s = spawnSync('npm', ['run', 'tauri:sync'], { cwd: ROOT, stdio: 'inherit', shell: true });
    if (s.status !== 0) failed++;

    console.log('=== npm run check ===');
    const r = spawnSync('npm', ['run', 'check'], { cwd: ROOT, stdio: 'inherit', shell: true });
    if (r.status !== 0) failed++;
  }

  if (failed) {
    console.error('');
    console.error(failed + ' target(s) failed');
    process.exit(1);
  }
  console.log('');
  console.log('all selected targets passed');
}

main();
