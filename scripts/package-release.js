#!/usr/bin/env node
'use strict';
/**
 * Build the two release artifacts from one command.
 *
 *   release/AgentKeeper_<ver>_x64-setup.exe        NSIS installer (currentUser)
 *   release/AgentKeeper_<ver>_x64-portable.zip     install-free archive
 *
 * Why this exists instead of putting `"targets": ["nsis", "portable"]` in
 * tauri.conf.json: tauri-bundler 2.x has no `portable` package type on Windows —
 * the supported set is exactly `nsis` and `msi`
 * (tauri-bundler-2.10.3/src/bundle/settings.rs:67-74). Writing `portable` into
 * `bundle.targets` is a config error, not a shorthand, so the archive is
 * assembled here instead of being faked as a config value.
 *
 * The two artifacts are the same files; only the delivery differs:
 *   installer → %LOCALAPPDATA%\AgentKeeper + shortcuts + uninstall entry
 *   portable  → unzip anywhere and run agentkeeper-shell.exe, nothing installed
 *
 * Both carry the MCP sidecar, because `docs/MCP-SERVER.md` tells users to point
 * their agent config at `agentkeeper-mcp.exe` next to the shell. A portable copy
 * resolves it the same way (sibling file), which is why the zip ships it too.
 *
 * Usage:
 *   npm run tauri:bundle                  # sync + sidecar + build + stage
 *   node scripts/package-release.js       # same, run directly
 *   node scripts/package-release.js --no-build   # stage existing artifacts only
 *   node scripts/package-release.js --portable-only
 *
 * Exit 0 = both requested artifacts exist in release/ and were verified.
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..');
/** `tauri-shell/` — the Rust workspace root. */
const WORKSPACE = path.join(ROOT, 'tauri-shell');
const SRC_TAURI = path.join(WORKSPACE, 'src-tauri');
/**
 * The workspace's shared target directory. It sits at the workspace root, not
 * under `src-tauri/`, because `core/` and `src-tauri/` are two members of one
 * workspace — cargo builds them into one `target/`.
 */
const TARGET = path.join(WORKSPACE, 'target', 'release');
/** Where the sidecars are staged for `bundle.externalBin`. */
const STAGED = path.join(SRC_TAURI, 'binaries');
const OUT_DIR = path.join(ROOT, 'release');
const CONF = path.join(SRC_TAURI, 'tauri.conf.json');

const SHELL_EXE = 'agentkeeper-shell.exe';
const MCP_EXE = 'agentkeeper-mcp.exe';
const SCAN_EXE = 'ak-scan.exe';
/** Shipped beside the shell so the documented MCP path exists in both forms. */
const SIDECARS = [MCP_EXE, SCAN_EXE];
/** The triple tauri.conf.json's `bundle.externalBin` entries resolve against. */
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

/* ------------------------------------------------------------------ config */

function version() {
  try {
    const conf = JSON.parse(fs.readFileSync(CONF, 'utf8'));
    if (conf.version) return conf.version;
  } catch (_e) {
    /* fall through to package.json */
  }
  const pkg = JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8'));
  if (!pkg.version) fail('cannot determine the version (tauri.conf.json and package.json)');
  return pkg.version;
}

/* ------------------------------------------------------------------- build */

function run(cmd, args, opts) {
  const r = spawnSync(cmd, args, { stdio: 'inherit', ...opts });
  if (r.error) fail(`could not run ${cmd}: ${r.error.message}`);
  if (r.status !== 0) fail(`${cmd} ${args.join(' ')} exited ${r.status}`);
}

function node(script, args, cwd) {
  run(process.execPath, [path.join(ROOT, script), ...(args || [])], { cwd: cwd || ROOT });
}

function buildAll() {
  // Three steps, in this order:
  //   1. dist — `src/renderer` is compiled into the executable by tauri-build
  //   2. core binaries — `agentkeeper-mcp` and `ak-scan` live in the
  //      `agentkeeper-core` crate, so `cargo tauri build` cannot see them
  //      (it collects binaries from the crate it builds). They are staged as
  //      sidecars for `bundle.externalBin`.
  //   3. cargo tauri build — builds the shell and bundles the sidecars.
  //
  // Step 2 is not circular. It used to be, when the sidecars were `[[bin]]`
  // targets of the shell crate itself: `tauri-build` runs as that crate's
  // build script for every target, so a clean `target/` could not build a
  // sidecar the build script insisted already existed. The core crate has no
  // build script, so the sidecar is built by something that does not care.
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

/* ---------------------------------------------------------------- collect */

function must(p, label) {
  if (!fs.existsSync(p)) fail(`${label} missing: ${p}`);
  return p;
}

/** Copy a file and prove the copy is byte-identical. */
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
    // A version bump in Cargo.toml without re-running the build leaves a
    // differently-named installer here. Take it, but say so — silently
    // publishing the old name is how a release ships the previous binary.
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
 * Locate a sidecar by its installed name (`agentkeeper-mcp.exe`).
 *
 * The staged copy is named `agentkeeper-mcp-<triple>.exe` because that is what
 * `bundle.externalBin` resolves against; the installer renames it on the way
 * in. The portable zip has to carry the plain name, so the triple is stripped
 * here. The build output is the fallback for a `--no-build` run whose staging
 * directory was cleared.
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
  // Zip creation uses Windows' bundled bsdtar by ABSOLUTE path.
  //
  // Not `tar` off PATH: in Git Bash that resolves to GNU tar 1.35 (/usr/bin/tar),
  // where `-a` does not select the archive format for `.zip` and the result is a
  // plain tar with a .zip name (verified: signature 'A','g' instead of 'P','K').
  // bsdtar 3.8+ ships with Windows 10 1803+ and does write a real deflate zip.
  //
  // `-f` gets the bare filename: bsdtar reads a drive-letter argument as a
  // remote `host:path` spec and fails with "Cannot connect to <drive>:".
  // cwd is OUT_DIR, so a relative name is also unambiguous.
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

/* ------------------------------------------------------------------- main */

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
