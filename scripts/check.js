#!/usr/bin/env node
'use strict';
/**
 * One gate to run them all — the whole static verification suite in one file.
 *
 *   node scripts/check.js          # everything
 *   node scripts/check.js residue  # one gate, by name
 *
 * Why a single entry point: one script, one口径 — every gate below either
 * passes or fails for the same reason a reviewer would reject the change.
 * The whole suite runs in well under a second, so it can run on every save.
 *
 * Gates (in order, cheapest first):
 *   1. residue   — mutation markers and backup files left behind by a harness
 *   2. bridge    — every `window.ak` call shape matches the Rust command it
 *                  invokes (see below — this is the gate that pays for itself)
 *   3. tree      — `target/` stays under a size ceiling (pdb and caches are KEPT);
 *                  orphaned build dirs are reported, not deleted (7.9 GB once grew here)
 *   4. renderer  — `scripts/lint-renderer.js` (i18n parity, CSP, shadowing)
 *   5. ui        — `scripts/ui-audit.js` (dead controls, theme violations)
 *   6. i18n      — `scripts/tauri-i18n.js` (Rust sentence → zh dictionary)
 *   7. dist      — `tauri-shell/copy-frontend.js --check` (bundle is current)
 *
 * ## Why the bridge gate exists
 *
 * Tauri resolves a command argument by *name* from the invoke payload
 * (`tauri-2.12.4/src/ipc/command.rs`: `deserialize_json` → `missing required
 * key`). A bridge that passes the payload object itself instead of
 * `{ payload }` therefore fails at runtime, silently: the Rust command rejects
 * the call, the renderer has no `catch`, and the button looks dead. Three
 * commands were broken exactly that way (`save_skill`, `upsert_mcp`,
 * `install_skill`) while 371 Rust tests stayed green, because the commands are
 * private functions an integration test cannot call. The one place the bug is
 * visible is the *shape* of the call, so that is what this gate checks.
 *
 * Exit 0 = all gates pass, 1 = at least one failed.
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

/* ------------------------------------------------------------------ gate 1 */

/** Trees a mutation harness can leave a mutant in. */
const TREES = ['src', 'scripts', 'tauri-shell/src-tauri/src', 'tauri-shell/src-tauri/tests'];
const MARKERS = [
  { re: /MUTANT[-_]?\w*/g, why: 'mutation-test marker' },
  { re: /MUTATION\s+M\d+/g, why: 'mutation-test marker' }
];
/** A failed revert leaves one of these behind, and `cargo test` would compile it. */
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

/* ------------------------------------------------------------------ gate 2 */

/** Args Tauri resolves from the command message, not from the payload body. */
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

/** Top-level keys of an object literal like `{ args: { assetId: x } }`. */
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

  /* registered commands: the generate_handler! list */
  const handler = /generate_handler!\s*\[([\s\S]*?)\]/.exec(lib);
  const registered = new Set(
    handler ? handler[1].split(',').map((s) => s.trim()).filter(Boolean) : []
  );
  if (!registered.size) return fail('bridge', `no generate_handler! list found in ${path.relative(ROOT, LIB_RS)}`);

  /* Rust command signatures */
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

  /* bridge call sites */
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
    // A command whose parameters all come from the command message (Request,
    // AppHandle, …) takes the payload as an opaque body: no key to check.
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
    // An optional argument still has to be spelled right: `{ opt }` for
    // `opts: Option<…>` deserializes to None and the feature quietly no-ops.
    const unknown = keys.filter((k) => !spec.all.includes(k));
    if (unknown.length) {
      fail(
        'bridge',
        `${where}  call('${name}') sends unknown key(s) ${unknown.join(', ')}; ` +
          `Rust accepts { ${spec.all.join(', ')} }`
      );
    }
  });

  /* commands nobody calls — informational, not a failure */
  const calledNames = new Set(
    [...bridge.matchAll(/call\('([A-Za-z0-9_]+)'/g)].map((x) => x[1])
  );
  const dead = [...registered].filter((c) => !calledNames.has(c));
  ok(`bridge: ${calls} call sites ↔ ${registered.size} registered commands, 0 shape mismatches`);
  if (dead.length) console.log(`        unused commands (${dead.length}): ${dead.join(', ')}`);
}

/* ------------------------------------------------------------- gates 3–7 */

/**
 * gate: tree — `target/` must stay BOUNDED, not sterile.
 *
 * Policy: build artefacts are KEPT. `*.pdb`, `*.rlib`, `*.rmeta`
 * and `incremental/` are what the linker emits or what the next build reads;
 * deleting them buys disk by making every rebuild slower. This gate does not
 * ask for their removal.
 *
 * What it protects is boundedness. The tree once reached 7.9 GB (2.9 debug +
 * 5.0 release) because each Cargo.toml change left an orphaned
 * `build/<crate>-<hash>/` behind and an unexpected incremental cache could
 * refill invisible to everyone, and nothing failed while it grew.
 *
 *   HARD  total > CEILING_GIB       → fail, with a breakdown naming the
 *                                     biggest consumers so the fix is targeted
 *   INFO  pdb / orphaned build dirs / incremental cache — reported with their
 *         size, never deleted by this gate, never a failure on their own
 *
 * Baseline measured for this project (both profiles, release
 * incremental cache warm): 4.5 GiB without `.pdb`, ~4.9 GiB with them. The
 * 6.5 GiB ceiling leaves room for one extra profile or toolchain while still
 * failing well below the 7.9 GiB incident — and a single stray 2.5 GiB
 * incremental cache (the worst case seen) now trips it on its own.
 *
 * The two config pins below stay hard failures: a missing/loosened
 * `[build] incremental = false` is what silently refilled that cache.
 */
function gateTree() {
  // The workspace's shared target directory: `tauri-shell/target`, beside
  // `core/` and `src-tauri/` — cargo builds every workspace member into it.
  const TARGET = path.join(ROOT, 'tauri-shell', 'target');
  const REL = 'tauri-shell/target';
  const GIB = 1024 ** 3;
  /** Total tree ceiling. See the gate doc: baseline 4.5–4.9 GiB. */
  const CEILING_GIB = 6.5;
  if (!fs.existsSync(TARGET)) {
    ok('tree: no target/ yet — nothing built, nothing to measure');
    return;
  }

  // Collect every finding, then report once. A gate that fails and *also*
  // prints its ok line is worse than no gate: the summary reads "ok" and the
  // failure scrolls away. (This happened on the first run of this gate.)
  const problems = [];

  /** Walk without following links; returns {path,size,inode} for files. */
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
          /* raced a concurrent build */
        }
      }
    }
  }

  /**
   * Sum bytes, counting each inode once.
   *
   * cargo hard-links `target/<profile>/<name>.exe|.pdb` against the hashed
   * copy under `deps/`, so a naive sum double-counts the final artefacts.
   * Reporting and the ceiling must agree with `du`, not with the number of
   * directory entries.
   */
  const seenInode = new Set();
  const unique = files.filter((f) => {
    if (seenInode.has(f.inode)) return false;
    seenInode.add(f.inode);
    return true;
  });
  const sumOf = (pred) => unique.filter(pred).reduce((a, f) => a + f.size, 0);
  const totalBytes = unique.reduce((a, f) => a + f.size, 0);

  /* ---- informational: what the tree is made of (never a failure) -------- */
  const pdbBytes = sumOf((f) => f.path.toLowerCase().endsWith('.pdb'));
  const pdbCount = unique.filter((f) => f.path.toLowerCase().endsWith('.pdb')).length;
  const rlibBytes = sumOf((f) => f.path.endsWith('.rlib'));
  const rmetaBytes = sumOf((f) => f.path.endsWith('.rmeta'));
  const incBytes = sumOf((f) => f.path.split(path.sep).includes('incremental'));

  /* orphaned build-script output dirs (exact hash match, same rule as
     tauri-shell/clean-build.js — matching on the crate name alone would call
     every historical hash live and hide the growth) */
  const buildRoot = path.join(TARGET, 'release', 'build');
  const fpRoot = path.join(TARGET, 'release', '.fingerprint');
  let fps = [];
  try {
    fps = fs.readdirSync(fpRoot);
  } catch {
    /* not built yet */
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
    /* no build dir */
  }
  const orphanBytes = orphanDirs.reduce((a, name) => {
    const prefix = path.join(buildRoot, name) + path.sep;
    return a + unique.filter((f) => f.path.startsWith(prefix)).reduce((x, f) => x + f.size, 0);
  }, 0);

  /* ---- HARD: the ceiling ------------------------------------------------- */
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

  /* ---- informational lines (printed only when nothing failed) ----------- */
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

  /* the pins that keep it clean must themselves stay in place */
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

  // One ok line per note, then the summary. A gate that fails must never print
  // its ok line (that ordering bug was fixed once already); every note here is
  // printed only on the success path above.
  for (const n of notes) ok(n);
  const cacheRlib = unique.filter((f) => f.path.endsWith('.rlib')).length;
  ok(
    `tree: ${gib(totalBytes)} GiB of ${CEILING_GIB} GiB ceiling ` +
      `(kept ${cacheRlib} .rlib + matching .rmeta as compile cache)`
  );
}

/* ------------------------------------------------------------- gate: untracked */

/**
 * gate: untracked — build artefacts must not be tracked by git.
 *
 * `tauri-shell/dist/` is generated from `src/renderer` by
 * `tauri-shell/copy-frontend.js`, and `cargo tauri build` compiles whatever is
 * in `dist/` without checking whether it matches the sources. A tracked copy is
 * therefore a trap: it can be older than the renderer it claims to mirror, and
 * the executable ships the old UI. `tauri.conf.json` runs the generator as
 * `beforeBuildCommand`, so nothing is lost by not tracking it — and the
 * `release/` artefacts are rebuilt from source the same way.
 *
 * The check is on the INDEX (`git ls-files`), not on `.gitignore`: a file can
 * be ignored and still be tracked, and that is exactly the case worth
 * catching.
 */
const MUST_BE_UNTRACKED = ['tauri-shell/dist/', 'release/', 'tauri-shell/target/'];

function gateUntracked() {
  const r = spawnSync('git', ['ls-files'], { cwd: ROOT, encoding: 'utf8' });
  if (r.status !== 0) {
    // No git (a tarball export, a fresh container): nothing to check.
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

/** Run a repo script in-process tree; returns true when it exits 0. */
function runScript(gate, script, args, label) {
  const r = spawnSync(process.execPath, [script, ...(args || [])], { cwd: ROOT, stdio: 'inherit' });
  if (r.status === 0) {
    ok(label);
    return;
  }
  fail(gate, `${path.relative(ROOT, script)} exited ${r.status}`);
}

/* -------------------------------------------------------------------- main */

const GATES = {
  residue: gateResidue,
  bridge: gateBridge,
  tree: gateTree,
  renderer: () => runScript('renderer', path.join(ROOT, 'scripts', 'lint-renderer.js'), [], 'renderer: lint + i18n parity clean'),
  ui: () => runScript('ui', path.join(ROOT, 'scripts', 'ui-audit.js'), [], 'ui: no dead controls / theme violations'),
  i18n: () => runScript('i18n', path.join(ROOT, 'scripts', 'tauri-i18n.js'), [], 'i18n: every Rust sentence has a zh mapping'),
  dist: () => runScript('dist', DIST_CHECK, ['--check'], 'dist: tauri-shell/dist matches src/renderer'),
  // `tauri-shell/dist/` is a build artefact. If it is tracked again, a stale
  // copy can be committed and then compiled into the executable — the build
  // does not check freshness. The `beforeBuildCommand` in tauri.conf.json
  // regenerates it on every build, so tracking it buys nothing and risks
  // shipping an old UI.
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
