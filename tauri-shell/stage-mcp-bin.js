#!/usr/bin/env node
'use strict';
/**
 * Stage the MCP stdio server so `cargo tauri build` can bundle it (B3).
 *
 * `tauri.conf.json`'s `bundle.externalBin` takes a path *without* a target
 * triple: Tauri appends `-<triple>` (+ `.exe` on Windows) itself and copies the
 * result next to the app executable inside the installer. So the file it looks
 * for is
 *
 *   tauri-shell/src-tauri/binaries/agentkeeper-mcp-x86_64-pc-windows-msvc.exe
 *
 * and what the user ends up with is
 * `%LOCALAPPDATA%\AgentKeeper\agentkeeper-mcp.exe` — the path
 * `docs/MCP-SERVER.md` tells people to paste into their agent config.
 *
 * Nothing produced that file: `cargo tauri build` builds the app binary only,
 * never the other `[[bin]]` targets, and this directory is git-ignored (a
 * multi-MB executable is rebuilt from source, not stored). The installer
 * therefore shipped no MCP server and the documented path did not exist.
 *
 * This script is the one producer, and it has to run BEFORE `cargo tauri build`:
 * a missing external bin is a hard config error, not a warning.
 *
 *   node tauri-shell/stage-mcp-bin.js              # build + stage
 *   node tauri-shell/stage-mcp-bin.js --no-build   # stage the existing artefact
 *   node tauri-shell/stage-mcp-bin.js --target aarch64-pc-windows-msvc
 *   AK_TARGET=<triple>  same as --target
 *   AK_CARGO=<path>     explicit cargo (this repo keeps cargo off PATH; see
 *                       scripts/rust-env.ps1)
 *
 * Idempotent: re-running overwrites the staged copy. Exit 0 = the file the
 * bundler will pick up is in place, exit 1 = it is not, with the reason.
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const SRC_TAURI = path.join(__dirname, 'src-tauri');
const CONF = path.join(SRC_TAURI, 'tauri.conf.json');
const OUT_DIR = path.join(SRC_TAURI, 'binaries');
const BIN = 'agentkeeper-mcp';
/** The only triple this app bundles: tauri.conf.json targets NSIS on win-x64. */
const DEFAULT_TRIPLE = 'x86_64-pc-windows-msvc';
/** What `bundle.externalBin` must contain for the staged file to be shipped. */
const EXTERNAL_BIN_ENTRY = `binaries/${BIN}`;

function log(message) {
  console.log(`[stage-mcp] ${message}`);
}

function fail(message) {
  console.error(`[stage-mcp] ${message}`);
  process.exit(1);
}

/* ------------------------------------------------------------------ args */

const argv = process.argv.slice(2);
const noBuild = argv.includes('--no-build');

const targetFlag = argv.indexOf('--target');
if (targetFlag >= 0 && !argv[targetFlag + 1]) {
  fail('--target needs a triple, e.g. x86_64-pc-windows-msvc');
}
const target = (targetFlag >= 0 ? argv[targetFlag + 1] : '') || process.env.AK_TARGET || DEFAULT_TRIPLE;

/* --------------------------------------------------------------- guard */

if (process.platform !== 'win32') {
  // Not a portability nicety: the destination name encodes the Windows triple,
  // so staging on Linux/macOS would produce a file the bundler claims is a
  // Windows PE while it is not. There is no non-Windows bundle target today.
  fail(`the bundled MCP server is Windows-only (NSIS/win-x64); run this on Windows, got ${process.platform}`);
}

/* ------------------------------------------------------------- config */

/** Tauri silently ignores `externalBin` entries that do not resolve, so the
 *  config half of this fix is asserted here rather than trusted. */
function assertConfigPointsAtTheStagedFile() {
  let conf;
  try {
    conf = JSON.parse(fs.readFileSync(CONF, 'utf8'));
  } catch (e) {
    fail(`could not read ${CONF}: ${e.message}`);
  }
  const entry = (conf.bundle && conf.bundle.externalBin) || [];
  if (!entry.includes(EXTERNAL_BIN_ENTRY)) {
    fail(
      `tauri.conf.json bundle.externalBin does not list "${EXTERNAL_BIN_ENTRY}" ` +
        `(it has ${JSON.stringify(entry)}) — the staged file would not be bundled`
    );
  }
  if (conf.bundle.active !== true) {
    fail('tauri.conf.json has bundle.active !== true');
  }
  return conf;
}

/* -------------------------------------------------------------- build */

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
  const args = ['build', '--release', '--bin', BIN];
  if (target !== DEFAULT_TRIPLE) args.push('--target', target);
  log(`building: ${cargo} ${args.join(' ')}  (cwd ${SRC_TAURI})`);
  const run = spawnSync(cargo, args, { cwd: SRC_TAURI, stdio: 'inherit' });
  if (run.error) {
    fail(
      `could not run cargo: ${run.error.message}. ` +
        'Source the toolchain first (. .\\scripts\\rust-env.ps1) or set AK_CARGO'
    );
  }
  if (run.status !== 0) fail(`cargo build exited ${run.status}`);
}

/** Where cargo left the executable for the triple we are staging. */
function artefactPath() {
  const profile = path.join(SRC_TAURI, 'target', target === DEFAULT_TRIPLE ? '' : target, 'release');
  return path.join(profile, `${BIN}.exe`);
}

/* -------------------------------------------------------------- stage */

function main() {
  assertConfigPointsAtTheStagedFile();
  if (noBuild) {
    log('--no-build: using the existing artefact');
  } else {
    build();
  }

  const source = artefactPath();
  if (!fs.existsSync(source)) {
    fail(
      `no artefact at ${source}. Build it first: ` +
        `cargo build --release --bin ${BIN}${target === DEFAULT_TRIPLE ? '' : ` --target ${target}`}`
    );
  }

  const destination = path.join(OUT_DIR, `${BIN}-${target}.exe`);
  fs.mkdirSync(OUT_DIR, { recursive: true });
  fs.copyFileSync(source, destination);

  const from = fs.statSync(source).size;
  const to = fs.statSync(destination).size;
  if (from !== to || from === 0) {
    fail(`staged copy is ${to} bytes but the artefact is ${from} — copy did not complete`);
  }

  log(`source:      ${source}  (${from} bytes)`);
  log(`staged:      ${destination}  (${to} bytes)`);
  log(`externalBin: ${EXTERNAL_BIN_ENTRY}  ->  ${BIN}-${target}.exe  (installed as ${BIN}.exe)`);
}

main();
