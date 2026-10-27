#!/usr/bin/env node
'use strict';
/**
 * 把 `agentkeeper-core` crate 里那两个无界面二进制暂存过来，好让 Tauri 把它们
 * 作为 sidecar 打进安装包。
 *
 * 为什么非要这一步：`cargo tauri build` 只会扫描它自己要构建的那个 crate 的
 * `src/bin/` 来收集二进制 —— 也就是 Tauri 外壳这个 crate。而 MCP 服务与无界面
 * 扫描器是**故意**放在另一个 crate（`tauri-shell/core`）里的，这样它们不会继承
 * WebView 那一整条依赖图。于是 Tauri 根本看不到它们，`bundle.externalBin` 就是
 * 把别处的二进制带进安装包的那个机制。
 *
 * 以前让这件事做不成的循环依赖已经没了：sidecar 不再属于「构建脚本要校验
 * `externalBin`」的那个 crate。`tauri-build` 是作为**外壳** crate 的构建脚本跑的，
 * 而外壳 crate 里没有 sidecar —— 它在 core crate 里，那个 crate 自己没有 Tauri
 * 构建脚本。所以外壳的构建可以要求一个由别的 crate 产出的文件，两边都不必互相等。
 *
 * 用法：
 *   node tauri-shell/stage-core-bins.js              # 构建并暂存
 *   node tauri-shell/stage-core-bins.js --no-build   # 只暂存已有产物
 *   AK_CARGO=<路径>      显式指定 cargo（这个仓库故意不把 cargo 放进 PATH；
 *                       见 scripts/rust-env.ps1）
 *
 * 退出 0 = 打包器要找的每个文件都已就位；退出 1 = 没就位，并给出原因。
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

// `tauri-shell/` —— Rust 工作区根目录，装着 `core/` 与 `src-tauri/`。
const WORKSPACE = __dirname;
const CORE = path.join(WORKSPACE, 'core');
const OUT_DIR = path.join(WORKSPACE, 'src-tauri', 'binaries');

/** 安装包里跟着外壳一起发布的两个二进制。 */
const BINS = ['agentkeeper-mcp', 'ak-scan'];

/** `bundle.externalBin` 的条目，顺序与 tauri.conf.json 里列的一致。 */
const EXTERNAL_BIN_ENTRIES = BINS.map((b) => `binaries/${b}`);

/** 这个应用只打这一个三元组：tauri.conf.json 的目标是 win-x64 上的 NSIS。 */
const DEFAULT_TRIPLE = 'x86_64-pc-windows-msvc';

const argv = process.argv.slice(2);
const noBuild = argv.includes('--no-build');

function log(m) {
  console.log(`[stage-core] ${m}`);
}
function fail(m) {
  console.error(`[stage-core] ${m}`);
  process.exit(1);
}

/* ------------------------------------------------------------------ 配置 */

function assertConfigListsThem() {
  const confPath = path.join(WORKSPACE, 'src-tauri', 'tauri.conf.json');
  let conf;
  try {
    conf = JSON.parse(fs.readFileSync(confPath, 'utf8'));
  } catch (e) {
    fail(`cannot read ${confPath}: ${e.message}`);
  }
  const listed = (conf.bundle && conf.bundle.externalBin) || [];
  for (const entry of EXTERNAL_BIN_ENTRIES) {
    if (!listed.includes(entry)) {
      fail(
        `tauri.conf.json bundle.externalBin does not list "${entry}" (it has ` +
          `${JSON.stringify(listed)}) — the staged file would not be bundled`
      );
    }
  }
}

/* ------------------------------------------------------------------- 构建 */

/** 这个仓库的开发 shell 里故意没把 `cargo` 放进 PATH。 */
function cargoPath() {
  const explicit = process.env.AK_CARGO;
  if (explicit) {
    if (!fs.existsSync(explicit)) fail(`AK_CARGO points at ${explicit}, which does not exist`);
    return explicit;
  }
  const home = process.env.USERPROFILE || process.env.HOME || '';
  const beside = home ? path.join(home, '.cargo', 'bin', 'cargo.exe') : '';
  if (beside && fs.existsSync(beside)) return beside;
  return 'cargo'; // last resort: PATH. A missing cargo is reported below.
}

function build() {
  const cargo = cargoPath();
  const args = ['build', '--release', '--bins', '-p', 'agentkeeper-core'];
  log(`building: ${cargo} ${args.join(' ')}  (cwd ${WORKSPACE})`);
  const run = spawnSync(cargo, args, { cwd: WORKSPACE, stdio: 'inherit' });
  if (run.error) {
    fail(
      `could not run cargo: ${run.error.message}. ` +
        'Source the toolchain first (. .\\scripts\\rust-env.ps1) or set AK_CARGO'
    );
  }
  if (run.status !== 0) fail(`cargo build exited ${run.status}`);
}

/* ------------------------------------------------------------------- 暂存 */

function artefactPath(name) {
  return path.join(WORKSPACE, 'target', 'release', `${name}.exe`);
}

function main() {
  assertConfigListsThem();
  if (!noBuild) build();
  else log('--no-build: using the existing artefacts');

  fs.mkdirSync(OUT_DIR, { recursive: true });
  for (const name of BINS) {
    const source = artefactPath(name);
    if (!fs.existsSync(source)) {
      fail(
        `no artefact at ${source}. Build it first: ` +
          `cargo build --release --bins -p agentkeeper-core`
      );
    }
    const destination = path.join(OUT_DIR, `${name}-${DEFAULT_TRIPLE}.exe`);
    fs.copyFileSync(source, destination);
    const from = fs.statSync(source).size;
    const to = fs.statSync(destination).size;
    if (from !== to || from === 0) {
      fail(`staged copy is ${to} bytes but the artefact is ${from} — copy did not complete`);
    }
    log(`${name}: ${from} bytes → ${path.relative(WORKSPACE, destination)}`);
  }
  log(`externalBin: ${EXTERNAL_BIN_ENTRIES.join(', ')}`);
}

main();
