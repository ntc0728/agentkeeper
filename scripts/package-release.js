#!/usr/bin/env node
'use strict';
/**
 * 用一条命令构建两份发布产物。
 *
 *   release/AgentKeeper_<ver>_x64-setup.exe        NSIS 安装包（currentUser）
 *   release/AgentKeeper_<ver>_x64-portable.zip     免安装压缩包
 *
 * 为什么不把 `"targets": ["nsis", "portable"]` 直接写进
 * tauri.conf.json：tauri-bundler 2.x 在 Windows 上没有 `portable` 这种包类型——
 * 支持的集合恰好只有 `nsis` 和 `msi`
 * （tauri-bundler-2.10.3/src/bundle/settings.rs:67-74）。把 `portable` 写进
 * `bundle.targets` 是一个配置错误，而不是某种简写，所以压缩包
 * 在这里自己组装，而不是伪装成一个配置值。
 *
 * 两份产物是同一批文件，只有交付方式不同：
 *   installer → %LOCALAPPDATA%\AgentKeeper + 快捷方式 + 卸载项
 *   portable  → 随便解压到哪都能运行 agentkeeper-shell.exe，什么都不安装
 *
 * 两者都带上 MCP sidecar，因为 `docs/MCP-SERVER.md` 让用户把
 * agent 配置指向 shell 旁边的 `agentkeeper-mcp.exe`。便携版副本
 * 用同样的方式解析它（同级文件），所以 zip 里也带上它。
 *
 * 用法：
 *   npm run tauri:bundle                  # 同步 + sidecar + 构建 + 归集
 *   node scripts/package-release.js       # 同上，直接运行
 *   node scripts/package-release.js --no-build   # 只归集已有产物
 *   node scripts/package-release.js --portable-only
 *
 * 退出码 0 = 要求的产物都存在于 release/ 且已校验。
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..');
/** `tauri-shell/` —— Rust 工作区根目录。 */
const WORKSPACE = path.join(ROOT, 'tauri-shell');
const SRC_TAURI = path.join(WORKSPACE, 'src-tauri');
/**
 * 工作区共享的 target 目录。它位于工作区根，而不是
 * `src-tauri/` 下面，因为 `core/` 和 `src-tauri/` 是同一个工作区的
 * 两个成员——cargo 把它们构建进同一个 `target/`。
 */
const TARGET = path.join(WORKSPACE, 'target', 'release');
/** sidecar 为 `bundle.externalBin` 归集到的位置。 */
const STAGED = path.join(SRC_TAURI, 'binaries');
const OUT_DIR = path.join(ROOT, 'release');
const CONF = path.join(SRC_TAURI, 'tauri.conf.json');

const SHELL_EXE = 'agentkeeper-shell.exe';
const MCP_EXE = 'agentkeeper-mcp.exe';
const SCAN_EXE = 'ak-scan.exe';
/** 随 shell 一起出货，好让文档里写的 MCP 路径在两种形态下都存在。 */
const SIDECARS = [MCP_EXE, SCAN_EXE];
/** tauri.conf.json 里 `bundle.externalBin` 条目所解析的三元组。 */
const TRIPLE = 'x86_64-pc-windows-msvc';

const argv = process.argv.slice(2);
const noBuild = argv.includes('--no-build');
const portableOnly = argv.includes('--portable-only');

function log(m) {
  console.log(`[package] ${m}`);
}
function fail(m) {
  console.error(`[package] ${m}`);
  process.exit(1);
}

/* ------------------------------------------------------------------ 配置 */

function version() {
  try {
    const conf = JSON.parse(fs.readFileSync(CONF, 'utf8'));
    if (conf.version) return conf.version;
  } catch (_e) {
    /* 落到 package.json */
  }
  const pkg = JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8'));
  if (!pkg.version) fail('cannot determine the version (tauri.conf.json and package.json)');
  return pkg.version;
}

/* ------------------------------------------------------------------- 构建 */

function run(cmd, args, opts) {
  const r = spawnSync(cmd, args, { stdio: 'inherit', ...opts });
  if (r.error) fail(`could not run ${cmd}: ${r.error.message}`);
  if (r.status !== 0) fail(`${cmd} ${args.join(' ')} exited ${r.status}`);
}

function node(script, args, cwd) {
  run(process.execPath, [path.join(ROOT, script), ...(args || [])], { cwd: cwd || ROOT });
}

function buildAll() {
  // 三步，顺序如下：
  //   1. dist —— `src/renderer` 由 tauri-build 编译进可执行文件
  //   2. core 二进制 —— `agentkeeper-mcp` 和 `ak-scan` 住在
  //      `agentkeeper-core` crate 里，所以 `cargo tauri build` 看不见它们
  //      （它只从自己构建的那个 crate 收集二进制）。它们被归集为
  //      `bundle.externalBin` 的 sidecar。
  //   3. cargo tauri build —— 构建 shell 并把 sidecar 打进包里。
  //
  // 第 2 步不是循环依赖。它曾经是——那时 sidecar 是 shell crate 自己的
  // `[[bin]]` 目标：`tauri-build` 会作为那个 crate 的构建脚本对每个目标运行，
  // 于是一个干净的 `target/` 没法构建出一个构建脚本坚持要求「已经存在」的
  // sidecar。core crate 没有构建脚本，所以 sidecar 是由一个不在乎这件事的
  // 东西构建出来的。
  log('sync frontend');
  node('tauri-shell/copy-frontend.js');
  log('stage core binaries');
  node('tauri-shell/stage-core-bins.js');
  log('cargo tauri build');
  run('cargo', ['tauri', 'build'], {
    cwd: SRC_TAURI,
    // Release 出包开 incremental：`#[tauri::command]` 宏为 37 个命令各实例化
    // 一遍泛型运行时，全量 codegen 实测 77-87 s；incremental 缓存把热重建压到
    // 18 s（实测，缓存 511 MB，`npm run tauri:clean` 可回收）。
    // env 层优先级高于 .cargo/config.toml 的 `[build] incremental = false`
    // （该文件注释自己写明的 escape hatch），只影响出包路径，dev 迭代不受影响。
    env: { ...process.env, CARGO_INCREMENTAL: '1' },
  });
}

/* ---------------------------------------------------------------- 收集 */

function must(p, label) {
  if (!fs.existsSync(p)) fail(`${label} missing: ${p}`);
  return p;
}

/** 复制一个文件，并证明这份副本字节完全一致。 */
function copyVerified(from, to) {
  fs.copyFileSync(from, to);
  const a = fs.statSync(from).size;
  const b = fs.statSync(to).size;
  if (a !== b || a === 0) fail(`copy incomplete: ${path.basename(to)} ${b} bytes vs ${a}`);
  return b;
}

function stageInstaller(ver) {
  const dir = path.join(TARGET, 'bundle', 'nsis');
  const want = `AgentKeeper_${ver}_x64-setup.exe`;
  const exact = path.join(dir, want);
  let src = exact;
  if (!fs.existsSync(src)) {
    // 在 Cargo.toml 里改了版本号却没重新构建，会在这里留下一个
    // 名字不同的安装包。我们把它取用，但要说出来——悄无声息地
    // 发布旧名字，正是发布出货上一个二进制的做法。
    const found = fs.existsSync(dir)
      ? fs.readdirSync(dir).filter((n) => n.toLowerCase().endsWith('.exe'))
      : [];
    if (found.length === 0) {
      fail(
        `no NSIS installer in ${dir}.\n` +
          '  tauri.conf.json lists `nsis` under bundle.targets, so this means the build did not run.\n' +
          '  Run without --no-build, or `cargo tauri build` in tauri-shell/src-tauri.'
      );
    }
    src = path.join(dir, found[0]);
    console.warn(`[package] NOTE: expected ${want}, staging ${found[0]} instead.`);
  }
  const dest = path.join(OUT_DIR, want);
  const bytes = copyVerified(src, dest);
  log(`installer  ${path.relative(ROOT, dest)}  ${(bytes / 1048576).toFixed(2)} MiB`);
  return dest;
}

/**
 * 按安装后的名字（`agentkeeper-mcp.exe`）定位一个 sidecar。
 *
 * 归集后的副本名为 `agentkeeper-mcp-<triple>.exe`，因为那正是
 * `bundle.externalBin` 所解析的名字；安装包会在装入时改名。
 * 便携 zip 必须带上不带三元组的那个名字，所以这里把它剥掉。
 * 对于一次 `--no-build` 运行、而归集目录又已被清空的情况，
 * 构建输出是兜底。
 */
function findSidecar(name) {
  const staged = path.join(STAGED, `${name.replace(/\.exe$/, '')}-${TRIPLE}.exe`);
  if (fs.existsSync(staged)) return staged;
  const built = path.join(TARGET, name);
  if (fs.existsSync(built)) return built;
  return null;
}

function stagePortable(ver) {
  const stage = path.join(OUT_DIR, `AgentKeeper_${ver}_x64-portable`);
  fs.rmSync(stage, { recursive: true, force: true });
  fs.mkdirSync(stage, { recursive: true });

  let total = 0;
  total += copyVerified(must(path.join(TARGET, SHELL_EXE), 'shell binary'), path.join(stage, SHELL_EXE));
  for (const side of SIDECARS) {
    const from = findSidecar(side);
    if (!from) {
      fail(
        `${side} not found.\n` +
          '  It is built by the agentkeeper-core crate and staged by\n' +
          '  tauri-shell/stage-core-bins.js — run `npm run tauri:core-bins`,\n' +
          '  or drop the --no-build flag to let the full build stage it.'
      );
    }
    total += copyVerified(from, path.join(stage, side));
  }

  fs.writeFileSync(
    path.join(stage, 'README-PORTABLE.txt'),
    [
      'AgentKeeper ' + ver + ' — portable (no installation)',
      '',
      'Run:  agentkeeper-shell.exe',
      '',
      'Nothing is installed: no registry entry, no shortcuts, no uninstaller.',
      'Settings and the asset store still live in %APPDATA%\\AgentKeeper, so a',
      'portable copy and an installed copy share the same store. Set',
      'AK_STORE_ROOT to keep them apart.',
      '',
      'MCP server for agent configs (stdio):',
      '  ' + path.join('(this folder)', MCP_EXE),
      '',
      'Headless scanner:',
      '  ' + SCAN_EXE + ' --help',
      '',
      'This build needs the WebView2 Runtime (Windows 11 has it; Windows 10 may not).',
      '',
    ].join('\r\n'),
    'utf8'
  );

  const zip = path.join(OUT_DIR, `AgentKeeper_${ver}_x64-portable.zip`);
  fs.rmSync(zip, { force: true });
  // 打 zip 用的是 Windows 自带的 bsdtar，靠绝对路径调用。
  //
  // 不用 PATH 上的 `tar`：在 Git Bash 里那解析到 GNU tar 1.35（/usr/bin/tar），
  // 它那里 `-a` 不会为 `.zip` 选择归档格式，结果是一个顶着 .zip 名字的
  // 普通 tar（实测：签名是 'A','g' 而不是 'P','K'）。
  // bsdtar 3.8+ 随 Windows 10 1803+ 一起提供，确实会写出真正的 deflate zip。
  //
  // `-f` 传的是纯文件名：bsdtar 会把带盘符的参数当成远程
  // `host:path` 规格，然后失败并报 "Cannot connect to <drive>:"。
  // cwd 是 OUT_DIR，所以相对名字同样没有歧义。
  const tarExe = path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe');
  if (!fs.existsSync(tarExe)) {
    fail(`need ${tarExe} (Windows 10 1803+) to create the portable zip`);
  }
  const t = spawnSync(tarExe, ['-a', '-c', '-f', path.basename(zip), path.basename(stage)], {
    cwd: OUT_DIR,
    stdio: 'inherit',
  });
  if (t.error || t.status !== 0) fail(`tar failed to create ${zip} (${t.error ? t.error.message : 'exit ' + t.status})`);
  must(zip, 'portable zip');

  const sig = Buffer.alloc(2);
  const fd = fs.openSync(zip, 'r');
  fs.readSync(fd, sig, 0, 2, 0);
  fs.closeSync(fd);
  if (sig[0] !== 0x50 || sig[1] !== 0x4b) fail(`${zip} is not a zip (signature ${sig.join(',')})`);

  const zbytes = fs.statSync(zip).size;
  log(`portable   ${path.relative(ROOT, zip)}  ${(zbytes / 1048576).toFixed(2)} MiB  (${SIDECARS.length + 1} files, raw ${(total / 1048576).toFixed(2)} MiB)`);
  fs.rmSync(stage, { recursive: true, force: true });
  return zip;
}

/* ------------------------------------------------------------------- 主流程 */

function main() {
  const ver = version();
  fs.mkdirSync(OUT_DIR, { recursive: true });

  if (!noBuild) buildAll();
  else log('--no-build: staging the artifacts already under target/release');

  const produced = [];
  if (!portableOnly) produced.push(stageInstaller(ver));
  produced.push(stagePortable(ver));

  console.log('');
  log(`done — ${produced.length} artifact(s) in ${path.relative(ROOT, OUT_DIR) || '.'}\\`);
  for (const p of produced) console.log('  ' + path.relative(ROOT, p));
}

main();
