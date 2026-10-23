#!/usr/bin/env node
'use strict';
/**
 * Stage the two headless binaries from the `agentkeeper-core` crate so Tauri
 * can bundle them as sidecars.
 *
 * Why this is needed at all: `cargo tauri build` collects the binaries of the
 * crate it builds — the Tauri shell — by scanning that crate's `src/bin/`.
 * The MCP server and the headless scanner live in a DIFFERENT crate
 * (`tauri-shell/core`), deliberately, so they do not inherit the WebView
 * dependency graph. Tauri therefore cannot see them, and `bundle.externalBin`
 * is the mechanism for shipping a binary from elsewhere.
 *
 * The circularity that used to make this impossible is gone: the sidecar no
 * longer belongs to the crate whose build script validates `externalBin`.
 * `tauri-build` runs as the SHELL crate's build script, and the shell crate
 * does not contain the sidecar — the core crate does, with no Tauri build
 * script of its own. So the shell's build can ask for a file that some other
 * crate produces, without either waiting on the other.
 *
 * Usage:
 *   node tauri-shell/stage-core-bins.js              # build + stage
 *   node tauri-shell/stage-core-bins.js --no-build   # stage existing artefacts
 *   AK_CARGO=<path>     explicit cargo (this repo keeps cargo off PATH; see
 *                       scripts/rust-env.ps1)
 *
 * Exit 0 = every file the bundler will look for is in place, exit 1 = not, with
 * the reason.
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

// `tauri-shell/` — the Rust workspace root, holding `core/` and `src-tauri/`.
const WORKSPACE = __dirname;
const CORE = path.join(WORKSPACE, 'core');
const OUT_DIR = path.join(WORKSPACE, 'src-tauri', 'binaries');

/** The two binaries the installer ships beside the shell. */
const BINS = ['agentkeeper-mcp', 'ak-scan'];

/** `bundle.externalBin` entries, in the order tauri.conf.json lists them. */
const EXTERNAL_BIN_ENTRIES = BINS.map((b) => `binaries/${b}`);

/** The only triple this app bundles: tauri.conf.json targets NSIS on win-x64. */
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

/* ------------------------------------------------------------------ config */

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

/* ------------------------------------------------------------------- build */

/** `cargo` is deliberately not on PATH in this repo's dev shell. */
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

/* ------------------------------------------------------------------- stage */

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
