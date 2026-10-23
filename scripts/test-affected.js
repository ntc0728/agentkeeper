#!/usr/bin/env node
'use strict';
/**
 * Run only the tests a change can affect.
 *
 * `cargo test --workspace` takes ~7 s and runs 317 tests. During iteration
 * that is the wrong default: changing one module of the core should not
 * re-run the update-subsystem's 39 integration tests.
 *
 * This maps the files that changed to the test targets that cover them:
 *
 *   core/src/update/**   →  core unit tests (update module) + tests/update.rs
 *                          + tests/boundary.rs (it drives the update manager)
 *   core/src/store.rs    →  core unit tests (store) + every integration suite
 *                          (the store is the substrate they all write through)
 *   src-tauri/src/lib.rs →  the shell's own unit tests + every integration suite
 *   src/renderer/**      →  no Rust test; the renderer gates (npm run check)
 *
 * With no arguments it runs everything, so it is safe to use as a default.
 * With `--changed <ref>` it diffs against a git ref instead of the working
 * tree, which is what CI wants.
 *
 * Usage:
 *   node scripts/test-affected.js                 # everything
 *   node scripts/test-affected.js                 # affected by uncommitted changes
 *   node scripts/test-affected.js --changed HEAD~1
 *   node scripts/test-affected.js --all           # ignore the mapping, run all
 *   node scripts/test-affected.js --list          # print the plan, run nothing
 *
 * Exit 0 = the selected tests passed; non-zero = a test failed.
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

/* --------------------------------------------------------------- the map */

/**
 * Which test target covers which source area.
 *
 * `core` unit tests are the crate's own `#[cfg(test)]` modules — cargo can
 * only run them as a whole (`-p agentkeeper-core --lib`), so the finest
 * granularity available for the core is "the core's unit tests".
 *
 * The integration suites are separate binaries and can be selected
 * individually with `--test <name>`.
 */
const AREAS = [
  {
    name: 'core',
    // Files whose change can break the core's own unit tests.
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
    // The store is the substrate every integration suite writes through.
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
    // No Rust test covers the renderer; the static gates do.
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

/* ------------------------------------------------------------- changed files */

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
    // "XY path" — the path starts after the 3-char status field.
    const p = line.slice(3).trim().replace(/^"|"$/g, '');
    if (p) out.push(p);
  }
  return out;
}

/** A change to the manifest, the lockfile or a build script can break anything. */
function isGlobalChange(f) {
  return (
    /Cargo\.toml$/.test(f) ||
    /Cargo\.lock$/.test(f) ||
    /build\.rs$/.test(f) ||
    /tauri\.conf\.json$/.test(f) ||
    /tauri-shell\/Cargo\.toml$/.test(f)
  );
}

/* ------------------------------------------------------------------ plan */

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
  // de-duplicate, keep order
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

/* ------------------------------------------------------------------- run */

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
    const r = spawnSync('cargo', ['test', '-p', t.pkg, ...t.args, '--offline'], {
      cwd: SHELL,
      stdio: 'inherit',
    });
    if (r.status !== 0) {
      failed++;
      console.error('FAILED: ' + key(t));
    }
  }

  if (plan.gates) {
    // The renderer changed, so `dist/` is stale until it is regenerated — and
    // the `dist` gate would fail on exactly that. Sync first, then gate.
    console.log('=== npm run tauri:sync (the renderer changed) ===');
    const s = spawnSync('npm', ['run', 'tauri:sync'], { cwd: ROOT, stdio: 'inherit' });
    if (s.status !== 0) failed++;

    console.log('=== npm run check ===');
    const r = spawnSync('npm', ['run', 'check'], { cwd: ROOT, stdio: 'inherit' });
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
