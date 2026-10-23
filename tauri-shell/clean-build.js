#!/usr/bin/env node
'use strict';
/**
 * Reclaim space from the Rust build tree — WITHOUT touching artefacts a
 * rebuild would have to regenerate.
 *
 *   node tauri-shell/clean-build.js                 # report only
 *   node tauri-shell/clean-build.js --apply         # remove orphaned build dirs
 *   node tauri-shell/clean-build.js --apply --pdbs  # ...and linker .pdb files
 *   node tauri-shell/clean-build.js --apply --all   # ...and incremental caches too
 *
 * Policy: build artefacts are KEPT. `*.rlib`, `*.rmeta`, `*.pdb`
 * and `incremental/` are outputs, not residue — deleting them buys disk by
 * making the next build slower. What this script removes by default is only
 * the debris no build can use again: build-script output dirs whose crate hash
 * no `.fingerprint` references any more (each Cargo.toml change orphans one,
 * ~100 MB a piece).
 *
 * The optional flags exist for a machine that genuinely needs the disk back:
 *
 *   --pdbs   `*.pdb` under build/ and deps/. A linker output, not read by any
 *            build; this project attaches no native debugger. ~0.2-0.4 GiB.
 *   --all    also `incremental/` under each profile. The RELEASE cache costs
 *            ~0.8 GiB but is what `package-release.js` relies on for 18 s hot
 *            pack builds — deleting it sends the next pack back to ~80 s.
 *
 * Never removed by any flag: `*.rlib` + `*.rmeta` (the compile cache: one
 * matched pair per dependency; removing them means recompiling all ~300
 * crates), and the `out/` dirs of third-party build scripts (webview2-com-sys
 * ships WebView2LoaderStatic.lib with a 2006 mtime — a timestamp from the
 * published crate, not staleness).
 *
 * The tree-size ceiling lives in `scripts/check.js` (`gateTree`): that gate
 * fails when `target/` passes 6.5 GiB, which is the guard against unbounded
 * growth — not this script's default behaviour.
 *
 * Deletion uses PowerShell `Remove-Item`, not fs.rmSync: the latter stalls on
 * this tree (Node's fs routes deletes through the Recycle Bin here; thousands
 * of files take minutes instead of seconds).
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

// The workspace root is `tauri-shell/`, and cargo's shared target directory
// sits beside `core/` and `src-tauri/` — not inside either of them.
const WORKSPACE = __dirname;
const TARGET = path.join(WORKSPACE, 'target');
const APPLY = process.argv.includes('--apply');
const WITH_PDBS = process.argv.includes('--pdbs') || process.argv.includes('--all');
const WITH_INCREMENTAL = process.argv.includes('--all');

/** Every file under `dir` matching `pred`, as {path, size}. Missing dir = []. */
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
          /* raced with a concurrent build; skip */
        }
      }
    }
  }
  return out;
}

/**
 * Collapse hard links before summing.
 *
 * cargo hard-links `target/release/<name>.pdb` against
 * `target/release/deps/<name>-<hash>.pdb` (link count 2). Counting both
 * entries double-reports the bytes: a dry run listed 128 pdb
 * entries / 321 MB where `du` and inode accounting agreed on 74 files /
 * 199 MB. Deleting by path is still correct (the last link releases the
 * blocks), but the reported *reclaimable* figure must not be inflated --
 * a cleanup script that overstates its own value is worse than none.
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

/** Group a list of paths into the top-level build/ or deps/ bucket. */
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

// -- 1. pdb files (opt-in: --pdbs / --all) ---------------------------------
// A linker output, not residue — kept unless the machine needs the disk back.
const pdbsAll = WITH_PDBS ? walk(TARGET, (p) => p.toLowerCase().endsWith('.pdb')) : [];
const pdbs = dedupeByInode(pdbsAll);
/** Measured size even when not deleting, so the report can say what it costs. */
const pdbAllBytes = dedupeByInode(walk(TARGET, (p) => p.toLowerCase().endsWith('.pdb')))
  .reduce((a, f) => a + f.size, 0);

// -- 2. orphaned build-script output dirs ----------------------------------
// A `build/<crate>-<hash>/` is live iff `.fingerprint/<crate>-<hash>/` exists.
// Cargo reuses the fingerprint dir for every build unit of that crate; an
// orphaned build dir means no unit references that hash any more.
const buildRoot = path.join(TARGET, 'release', 'build');
const fpRoot = path.join(TARGET, 'release', '.fingerprint');
const orphans = [];
let fpNames = new Set();
try {
  fpNames = new Set(fs.readdirSync(fpRoot));
} catch {
  /* no fingerprints yet */
}
let buildDirs = [];
try {
  buildDirs = fs.readdirSync(buildRoot, { withFileTypes: true })
    .filter((e) => e.isDirectory())
    .map((e) => e.name);
} catch {
  /* never built */
}
for (const name of buildDirs) {
  // Compare the FULL name (crate + hash). Matching on the crate name alone
  // would call every historical hash "live": the tree held
  // agentkeeper-shell-{0dab…, ab74…, d8bc…} build dirs, and a crate-name match
  // reported zero orphans because at least one always matched.
  //
  // A build dir is still needed when *any* fingerprint dir carries its hash --
  // the same crate is compiled as several units (lib / bin / test), and they
  // share one build-script output dir.
  const hash = (name.match(/-([0-9a-f]{16})$/) || [])[1];
  const live = hash && [...fpNames].some((f) => f.endsWith('-' + hash));
  if (!live) {
    const dir = path.join(buildRoot, name);
    const files = walk(dir, () => true);
    orphans.push({ path: dir, size: files.reduce((a, f) => a + f.size, 0) });
  }
}

// -- 3. incremental cache (opt-in: --all) ----------------------------------
// The RELEASE cache is what package-release.js relies on for 18 s hot pack
// builds (CARGO_INCREMENTAL=1 on the pack path); the debug cache buys nothing
// (dev rebuilds in ~4 s either way). Both are caches, so neither is removed
// by default — `--all` is for a machine that needs the ~1 GiB back.
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
/** Measured either way, so the report can show what the caches cost. */
const incAllBytes = ['release', 'debug']
  .map((p) => path.join(TARGET, p, 'incremental'))
  .reduce((a, p) => {
    try {
      return a + walk(p, () => true).reduce((x, f) => x + f.size, 0);
    } catch {
      return a;
    }
  }, 0);

// -- report ----------------------------------------------------------------
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

// -- delete ----------------------------------------------------------------
// One Remove-Item per path: PowerShell handles this tree in seconds where
// fs.rmSync stalls (see the header note).
//
// Delete `pdbsAll`, not the de-duplicated `pdbs`: the pdb at target/release/ and
// its twin under deps/ are two directory entries onto one inode, so removing
// only one path frees nothing and the file survives. Reporting uses the
// de-duplicated list; deletion has to touch every link.
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

// -LiteralPath so a path with [ or ] (not present here, but cheaper than a bug)
// is not treated as a wildcard. Batched to stay under Windows' command length.
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

// Verify rather than trust the exit code.
const leftPdb = walk(TARGET, (p) => p.toLowerCase().endsWith('.pdb')).length;
console.log('');
console.log(`deleted ${victims.length - failed} path(s); ${leftPdb} pdb file(s) remain`);
process.exit(failed ? 1 : 0);
