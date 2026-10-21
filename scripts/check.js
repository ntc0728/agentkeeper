#!/usr/bin/env node
'use strict';
/**
 * One gate to run them all — the whole static verification suite in one file.
 *
 *   node scripts/check.js          # everything
 *   node scripts/check.js residue  # one gate, by name
 *
 * Why a single entry point: this repo used to have four overlapping checkers
 * (`check-mutation-residue`, `i18n-report`, `lint-renderer`, `ui-audit`) plus a
 * 60 KB `selftest.js` and a 1,356-line write-parity harness, all wired into one
 * `npm run verify` that took 74 s and tested an Electron app that no longer
 * ships. A suite nobody runs on a hunch protects nothing. These gates total
 * well under a second, so they can run on every save.
 *
 * Gates (in order, cheapest first):
 *   1. residue   — mutation markers and backup files left behind by a harness
 *   2. bridge    — every `window.ak` call shape matches the Rust command it
 *                  invokes (see below — this is the gate that pays for itself)
 *   3. renderer  — `scripts/lint-renderer.js` (i18n parity, CSP, shadowing)
 *   4. ui        — `scripts/ui-audit.js` (dead controls, theme violations)
 *   5. i18n      — `scripts/tauri-i18n.js` (Rust sentence → zh dictionary)
 *   6. dist      — `tauri-shell/copy-frontend.js --check` (bundle is current)
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

/** Args Tauri resolves from the command message, not from the JS payload. */
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

/* ------------------------------------------------------------- gates 3–6 */

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
  renderer: () => runScript('renderer', path.join(ROOT, 'scripts', 'lint-renderer.js'), [], 'renderer: lint + i18n parity clean'),
  ui: () => runScript('ui', path.join(ROOT, 'scripts', 'ui-audit.js'), [], 'ui: no dead controls / theme violations'),
  i18n: () => runScript('i18n', path.join(ROOT, 'scripts', 'tauri-i18n.js'), [], 'i18n: every Rust sentence has a zh mapping'),
  dist: () => runScript('dist', DIST_CHECK, ['--check'], 'dist: tauri-shell/dist matches src/renderer')
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
