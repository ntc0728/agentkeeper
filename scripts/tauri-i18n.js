#!/usr/bin/env node
'use strict';
/**
 * Rust-engine i18n audit — the Tauri half of the no-degradation guarantee.
 *
 * Policy: with the UI in Chinese, every sentence the engine puts on screen
 * must render in Chinese. Proper nouns / protocol words are the only allowed
 * exceptions.
 *
 * The engine is `tauri-shell/src-tauri/src/**\/*.rs`, and every Rust message
 * reaches the renderer as `{ ok:false, error:"…" }`, a `reason` field, a
 * validation issue or a `const &str`. The script scrapes all string literals,
 * filters out the ones that cannot reach the screen (see `looksSentence`) and
 * asserts each survivor has a dictionary entry; it also audits the dictionary
 * *back towards* the engine (ORPHAN), so dead entries and mis-targeted
 * additions surface instead of piling up silently.
 *
 * Dependencies: `src/renderer/engine-zh.js` and the Rust sources only.
 * ORPHAN is judged purely against the Rust engine, which is the only thing
 * that emits sentences in the shipped build.
 *
 * It also checks the two things a dictionary can get wrong without ever
 * showing up as a MISS:
 *   CODE-MISS  a `code` literal the Rust engine emits that CODE_ZH cannot
 *              translate (the user then sees a raw English code);
 *   CJK-KEY    Chinese sitting in an EXACT / PATTERNS *key*, i.e. a slot that
 *              must hold the engine's English.
 *              in Rust.
 *
 *   node scripts/tauri-i18n.js             gate: exit 1 on MISS/AMBIGUOUS
 *   node scripts/tauri-i18n.js --report    print every bucket, always exit 0
 *
 * Exit codes: 0 clean, 1 MISS / AMBIGUOUS / CODE-MISS / CJK-KEY, 2 could not run.
 */

const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
/**
 * Every Rust source tree that can put a sentence on screen.
 *
 * The engine lives in the `agentkeeper-core` crate; the Tauri shell has its
 * own tree. Both are scanned — a gate that only looked at one of them would
 * let half the English sentences through unnoticed, and the crate split moved
 * most of them into the other one.
 */
const RUST_SRC_DIRS = [
  path.join(ROOT, 'tauri-shell', 'core', 'src'),
  path.join(ROOT, 'tauri-shell', 'src-tauri', 'src'),
];
const DICT = path.join(ROOT, 'src', 'renderer', 'engine-zh.js');

for (const dir of RUST_SRC_DIRS) {
  if (!fs.existsSync(dir)) {
    console.error(`[tauri-i18n] missing Rust source tree: ${dir}`);
    process.exit(2);
  }
}
if (!fs.existsSync(DICT)) {
  console.error(`[tauri-i18n] missing dictionary: ${DICT}`);
  process.exit(2);
}
const zh = require(DICT);
const DICT_SRC = fs.readFileSync(DICT, 'utf8');

const reportOnly = process.argv.includes('--report');

/* ------------------------------------------------------------------ */
/* 1. Judgement rules                                                  */

/**
 * Does this string look like a sentence an English speaker would read?
 * Same regex (incl. the 140-char cap) and same two extra conditions as the
 * so a string that passed the same rule in the previous implementation passes
 * here too.
 */
function looksSentence(s) {
  return /^[A-Za-z][A-Za-z0-9 ,.:;'‘’"()%`[\]{}\-—&+!?/\\_§]{4,140}$/.test(s)
    && /[a-z]{3}/.test(s)
    && /\s[a-z]{2,}/.test(s);
}

const CJK = /[\u4e00-\u9fff]/;

/* ------------------------------------------------------------------ */
/* 2. Rust source → production (non-test) lines                        */

/**
 * Drop every line inside a `#[cfg(test)] mod … { … }` block.
 *
 * Test bodies are fixtures and assertion labels; they never reach the WebView,
 * and they hold ~300 of the crate's sentence-like literals. Depth is tracked on
 * a copy of the line with string literals blanked, so braces inside `"` do not
 * skew the count.
 */
function productionLines(src) {
  const out = [];
  let depth = 0;
  let skipUntilDepth = null;   // depth the test module opened at
  let pendingCfgTest = 0;      // lines since a bare `#[cfg(test)]` attribute

  src.split('\n').forEach((line, i) => {
    const bare = line
      .replace(/"(?:[^"\\]|\\.)*"/g, '""')           // string literals
      .replace(/\/\/.*$/, '');                       // trailing comments
    let braces = 0;
    for (const ch of bare) {
      if (ch === '{') braces += 1;
      else if (ch === '}') braces -= 1;
    }

    const before = depth;
    const t = line.trim();

    let testStart = false;
    if (/^#\[cfg\(test\)\]/.test(t)) {
      if (/mod\s+[\w]+\s*\{/.test(t)) testStart = true;   // one-liner form
      else pendingCfgTest = 1;                            // attribute, mod follows
    } else if (pendingCfgTest > 0 && /^(pub(\([^)]*\))?\s+)?mod\s+[\w]+\s*\{/.test(t)) {
      testStart = true;
    } else if (/^mod\s+(tests?|spec)\s*\{/.test(t)) {
      testStart = true;                                   // bare `mod tests {`
    }

    if (skipUntilDepth === null && testStart) skipUntilDepth = before;
    depth += braces;

    if (skipUntilDepth === null) out.push([i + 1, line]);

    if (skipUntilDepth !== null && depth <= skipUntilDepth) skipUntilDepth = null;
    if (pendingCfgTest > 0) pendingCfgTest = pendingCfgTest >= 2 ? 0 : pendingCfgTest + 1;
  });

  return out;
}

/* ------------------------------------------------------------------ */
/* 3. Literal extraction + normalisation                               */

/** Rust `{}` / `{name}` / `{:?}` / `{:#?}` / `{0}` → §1..§n, in order. */
function normalizePlaceholders(s) {
  let n = 0;
  return s.replace(/\{[^{}]*\}/g, () => `§${++n}`);
}

/** Undo the escapes the scanner also undid, so quoting matches the dictionary. */
function unescape(s) {
  return s.replace(/\\"/g, '"').replace(/\\\\/g, '\\');
}

const STRING_LITERAL = /"((?:[^"\\\n]|\\.)*)"/g;

/*
 * Call sites whose literal is NOT text the user reads. All of them are
 * structural (a call site kind), never a wording allowlist.
 *
 *   .expect(…) / panic!(…) / assert*!   abort the process; the WebView never sees it
 *   trace/log/eprintln!/…               developer diagnostics on stderr
 *   .contains/.starts_with/…            the literal is an INPUT matcher on a
 *                                       foreign error string, not text we emit
 */
const PANIC_SITE = /(?:^|[^\w])(?:panic!|\.expect|assert_eq!|assert_ne!|assert!)\(\s*(?:&)?(?:format!\()?\s*$/;
const DIAGNOSTIC_SITE = /\b(?:trace|log|debug|info|warn|eprintln!|println!|print!|dbg!)\(\s*(?:&)?(?:format!\()?\s*$/;
const INPUT_MATCHER = /\.(?:contains|starts_with|ends_with|find|replace|replacen|split|trim_start_matches|trim_end_matches|strip_prefix|strip_suffix)\(\s*$/;

/*
 * Log-surface sink, exempted by OUTPUT CHANNEL — never by file, so
 * `core/update/manager.rs` stays audited for everything that is not a log
 * line.
 *
 * The update manager routes progress lines through an injected
 * `type LogFn = Arc<dyn Fn(&str) + Send + Sync>` whose production value is
 * `Arc::new(|_| {})` — a no-op. Nothing outside the crate's tests ever
 * installs a real sink, so these strings reach neither the WebView nor a
 * terminal, and translating them would only make the log harder to grep.
 *
 * Matched two ways, because rustfmt puts a long `format!` on its own line:
 *   LOG_SITE     the literal sits on the same line as the sink call
 *   LOG_SINK     the sink call opens here; the literal is on a following line
 * `logSurface` is always counted and listed in full, so growth is visible.
 */
const LOG_SITE = /(?:^|[^\w.])(?:self\s*\.\s*log|logger|log::\w+|(?:log|info|warn|error|debug|trace)!)\s*\(\s*(?:&)?(?:format!\()?\s*$/;
const LOG_SINK = /(?:^|[^\w.])(?:self\s*\.\s*log|logger|log::\w+|(?:log|info|warn|error|debug|trace)!)\s*\(\s*(?:&)?(?:format!\()?/;
/** Progress lines the update manager tags with a channel prefix. */
const LOG_PREFIXES = ['[update]'];

/**
 * The ONE place that answers "does this English belong to the log surface
 * rather than the UI?".
 *
 * It is used twice — while scraping (row.log) and while judging dictionary
 * entries (ORPHAN) — because the two checks must never return
 * opposite verdicts for the same string: "the engine emits it, but not to the
 * screen" and "nobody uses it, delete it" cannot both be true.
 *
 * Only the PREFIX half is shareable. The other half of the scrape-side test
 * (LOG_SITE / inLogCall) is a call-site property, and a dictionary entry has
 * no call site; a log line whose text carries no channel prefix is therefore
 * recognised by the scrape, not by this predicate.
 */
function isLogSurface(s) {
  return LOG_PREFIXES.some((p) => s.replace(/^\^/, '').startsWith(p));
}

function extract(file) {
  const rows = [];
  const src = fs.readFileSync(file, 'utf8');
  const rel = path.relative(ROOT, file).replace(/\\/g, '/');

  // Blank out raw strings (r#"…"#, r##"…"##, r"…") first: they hold regexes
  // and JSON fixtures, never prose, and their embedded quotes (and stray `}`
  // at column 0) otherwise wreck both the literal scan and the brace counter.
  // `\b` matters: without it a word ending in "r" directly before a quote
  // ("…frontmatter"") would be read as the start of a raw string.
  const noRaw = src.replace(/\br(#*)"([\s\S]*?)"\1/g, '""');

  // Depth of a log-sink call whose argument list is still open. rustfmt puts a
  // long `format!(…)` on its own line, so the literal is often several lines
  // below the sink that owns it.
  let logDepth = 0;

  for (const [line, text] of productionLines(noRaw)) {
    if (/^\s*(\/\/|\*|\/\*)/.test(text)) continue;      // a comment, not a sentence

    const code = text.replace(/\/\/.*$/, '');           // trailing // comment
    const opensLog = LOG_SINK.test(code);
    const inLogCall = logDepth > 0;

    let m;
    STRING_LITERAL.lastIndex = 0;
    while ((m = STRING_LITERAL.exec(code))) {
      const before = code.slice(0, m.index);
      if (PANIC_SITE.test(before)) continue;
      const raw = unescape(m[1]);
      const norm = normalizePlaceholders(raw);
      if (!norm) continue;
      /*
       * Log surface is tested BEFORE DIAGNOSTIC_SITE, and that order is the
       * whole point. `self.log("…")` leaves `before` ending in `log(`, which
       * DIAGNOSTIC_SITE also matches; with diagnostics checked first those
       * literals were dropped before the log test ever ran — excluded from
       * MISS yet absent from every bucket, invisible. The dictionary entries
       * translating them were then reported as ORPHANs: one string, two
       * opposite verdicts. Only rustfmt-wrapped calls (literal on a following
       * line, caught by inLogCall) would otherwise escape this.
       */
      if (LOG_SITE.test(before) || inLogCall || isLogSurface(norm) || isLogSurface(raw)) {
        rows.push({ file: rel, line, s: norm, raw, log: true });
        continue;
      }
      if (DIAGNOSTIC_SITE.test(before)) continue;
      if (INPUT_MATCHER.test(before)) continue;
      // `raw` is kept because `{}` is not always a format placeholder: in a
      // regex quantifier ("{0,63}") it survives verbatim all the way to the
      // screen, so the dictionary key holds the braces, not a § marker.
      rows.push({ file: rel, line, s: norm, raw });
    }

    // Paren balance of the whole line (string literals blanked), so a sink
    // call spanning several lines keeps `logDepth` open until its `)` lands.
    const bare = code.replace(/"(?:[^"\\]|\\.)*"/g, '""');
    let balance = 0;
    for (const ch of bare) {
      if (ch === '(') balance += 1;
      else if (ch === ')') balance -= 1;
    }
    if (logDepth > 0) logDepth = Math.max(0, logDepth + balance);
    else if (opensLog && balance > 0) logDepth = balance;
  }
  return rows;
}

/* ------------------------------------------------------------------ */
/* 4. Buckets — every exclusion has a structural reason                */

/*
 * Buckets the audit deliberately does not judge. These are SCRAPING RULES
 * (what counts as engine prose) and are kept strictly separate from INTERNAL
 * in the dictionary (what IS engine prose but never reaches the screen).
 * Nothing here is a back door for a real miss: every bucket is counted in the
 * report, and the discretionary ones are listed in full.
 */

/**
 * Standalone CLI binaries (`src/bin/*.rs`): prose goes to a terminal, never to
 * the WebView. File-scoped, and printed in full in the report so growth stays
 * visible — the acknowledged trade-off is that a UI message added to a bin file
 * would be hidden. There is no UI channel in a bin today.
 */
const CLI_FILE = /\/bin\/[a-z0-9-]+\.rs$/;

/*
 * MCP stdio protocol surface. `core/mcp.rs` is the MCP server: its JSON-RPC
 * errors and tool-schema descriptions go to the MCP client (an LLM), never to
 * the WebView. Excluded by FILE, not by wording — the acknowledged trade-off
 * is that a real UI message added to mcp.rs would be hidden. mcp.rs has no UI
 * channel today; if it ever gets one, split the file and narrow this rule.
 *
 * The path is matched on the FILE NAME only, not on the directory it sits in:
 * the module moved from `src-tauri/src/core/mcp.rs` to `core/src/mcp.rs` when
 * the core became its own crate, and a rule anchored on the old directory
 * would have silently stopped matching — the gate would then demand Chinese
 * translations for sentences that only ever reach an LLM.
 */
const PROTOCOL_FILE = /(^|[\\/])mcp\.rs$/;

function classify(r) {
  if (r.log) return 'logSurface';
  if (CJK.test(r.s)) return 'chinese';
  // A literal that still contains a backslash after unescaping is a multiline
  // fixture ("line one\nline two\n"), a regex or a path — not prose.
  if (r.s.includes('\\')) return 'escaped';
  if (!looksSentence(r.s)) return 'notProse';
  // `format!("{}", e)` and friends carry no words of their own.
  if (!/[A-Za-z]{3}/.test(r.s.replace(/§\d/g, ''))) return 'placeholder';
  if (CLI_FILE.test('/' + r.file)) return 'cli';
  if (PROTOCOL_FILE.test('/' + r.file)) return 'protocol';
  return null;
}

/* ------------------------------------------------------------------ */
/* 5. Coverage, with provenance                                        */

const compiledPatterns = (zh.PATTERNS || []).map(([p, z]) => ({
  src: p, re: zh.patternToRegExp(p), zh: z, i: -1
}));
compiledPatterns.forEach((c, i) => { c.i = i; });

/** Which dictionary entry renders `s`? null = uncovered. */
function covering(s) {
  if (Object.prototype.hasOwnProperty.call(zh.EXACT, s)) {
    return { kind: 'EXACT', id: s, zh: zh.EXACT[s] };
  }
  if (zh.INTERNAL && zh.INTERNAL.has(s)) return { kind: 'INTERNAL', id: s, zh: '(passthrough)' };
  for (const c of compiledPatterns) {
    if (c.re.test(s)) return { kind: 'PATTERNS', id: `#${c.i}`, src: c.src, i: c.i, zh: c.zh };
  }
  return null;
}

/** Every entry that *could* render `s` — more than one means ambiguity. */
function allCovering(s) {
  const out = [];
  if (Object.prototype.hasOwnProperty.call(zh.EXACT, s)) out.push({ kind: 'EXACT', zh: zh.EXACT[s] });
  for (const c of compiledPatterns) if (c.re.test(s)) out.push({ kind: `PATTERNS#${c.i}`, zh: c.zh });
  return out;
}

/* ------------------------------------------------------------------ */
/* 6. Dictionary → engine checks (dead entries / mis-targeted adds)     */

/**
 * English keys that the dictionary declares more than once. `require()` lets
 * a duplicate object key silently win, so this has to be read off the SOURCE,
 * not off the loaded object — that is the whole point of the check.
 */
function duplicateExactKeys() {
  const from = DICT_SRC.indexOf('const EXACT');
  const to = DICT_SRC.indexOf('const PATTERNS');
  if (from < 0 || to < from) return [];
  const counts = new Map();
  const re = /^[ \t]{2}'((?:[^'\\]|\\.)*)'[ \t]*:/gm;
  let m;
  while ((m = re.exec(DICT_SRC.slice(from, to)))) {
    const k = m[1].replace(/\\'/g, "'");
    counts.set(k, (counts.get(k) || 0) + 1);
  }
  return [...counts.entries()].filter(([, n]) => n > 1);
}

/* ------------------------------------------------------------------ */
/* 6b. Rust `code` literals → CODE_ZH                                  */

/**
 * Every code literal the Rust engine can put on a wire, extracted from the
 * SOURCE at runtime — never from a hand-copied list. Three structural rules,
 * in descending strength:
 *
 *   json      `"code": "x"`            built into a Value/map that crosses a
 *                                      boundary — always emitted
 *   table     2nd element of a ("msg", "code") pair inside a `const …CODES`
 *                                      array — the code half of a lookup table
 *   field     `code: "x"` in a struct  only emitted if the struct derives
 *                                      Serialize; otherwise the value cannot
 *                                      reach the renderer at all
 */
function structIsSerialized(text, lineIdx) {
  const lines = text.split('\n');
  for (let i = lineIdx; i >= 0; i -= 1) {
    if (!/\b(?:pub(?:\([^)]*\))?\s+)?struct\s+\w+/.test(lines[i])) continue;
    for (let j = i; j >= Math.max(0, i - 12); j -= 1) {
      const d = lines[j].match(/#\[derive\(([^)]*)\)/);
      if (d) return /Serialize/.test(d[1]);
    }
    return false;
  }
  return false;
}

function extractCodes(file) {
  const src = fs.readFileSync(file, 'utf8');
  const rel = path.relative(ROOT, file).replace(/\\/g, '/');
  const noRaw = src.replace(/\br(#*)"([\s\S]*?)"\1/g, '""');
  const out = [];

  // rule: table — scan the const block, not line by line (pairs wrap).
  const tableRe = /const\s+[A-Za-z_]*CODES?[A-Za-z_]*\s*:[^=]*=\s*\[/g;
  let t;
  while ((t = tableRe.exec(noRaw))) {
    const end = noRaw.indexOf('\n];', t.index);
    const block = noRaw.slice(t.index, end < 0 ? noRaw.length : end);
    const baseLine = noRaw.slice(0, t.index).split('\n').length;
    const pairRe = /\(\s*("(?:[^"\\]|\\.)*"|[A-Z_][A-Z0-9_]*)\s*,\s*"((?:[^"\\]|\\.)*)"\s*,?\s*\)/g;
    let p;
    while ((p = pairRe.exec(block))) {
      const line = baseLine + block.slice(0, p.index).split('\n').length - 1;
      out.push({ file: rel, line, code: unescape(p[2]), rule: 'table' });
    }
  }

  for (const [line, text] of productionLines(noRaw)) {
    if (/^\s*(\/\/|\*|\/\*)/.test(text)) continue;
    const code = text.replace(/\/\/.*$/, '');
    for (const m of code.matchAll(/"code"\s*:\s*"((?:[^"\\]|\\.)*)"/g)) {
      out.push({ file: rel, line, code: unescape(m[1]), rule: 'json' });
    }
    for (const m of code.matchAll(/\bcode\s*:\s*"((?:[^"\\]|\\.)*)"/g)) {
      out.push({
        file: rel, line, code: unescape(m[1]), rule: 'field',
        serialized: structIsSerialized(noRaw, line - 1)
      });
    }
  }
  return out;
}

/** Chinese (or any CJK) sitting in a slot that must hold English. */
const CJK_KEY = /[\u3040-\u30ff\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\uac00-\ud7af]/;

function cjkKeys() {
  const out = [];
  for (const k of Object.keys(zh.EXACT)) {
    if (CJK_KEY.test(k)) out.push({ where: 'EXACT', key: k, zh: zh.EXACT[k] });
  }
  (zh.PATTERNS || []).forEach(([p, z], i) => {
    if (CJK_KEY.test(p)) out.push({ where: `PATTERNS#${i}`, key: p, zh: z });
  });
  return out;
}

/* ------------------------------------------------------------------ */
/* 7. Run                                                              */

function rustFiles(dir) {
  const out = [];
  (function walk(d) {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const f = path.join(d, e.name);
      if (e.isDirectory()) walk(f);
      else if (e.name.endsWith('.rs')) out.push(f);
    }
  })(dir);
  return out.sort();
}

const files = RUST_SRC_DIRS.flatMap((dir) => rustFiles(dir));
const all = [];
for (const f of files) all.push(...extract(f));

const seen = new Set();
const rows = all.filter((r) => {
  const k = `${r.file}|${r.s}`;
  if (seen.has(k)) return false;
  seen.add(k);
  return true;
});

const buckets = {
  miss: [], coveredExact: [], coveredPattern: [], coveredInternal: [],
  chinese: [], escaped: [], notProse: [], placeholder: [],
  cli: [], protocol: [], logSurface: []
};
const provenance = new Map();   // sentence -> covering entry

for (const r of rows) {
  const noise = classify(r);
  if (noise) { buckets[noise].push(r); continue; }
  const hit = covering(r.s);
  if (!hit) { buckets.miss.push(r); continue; }
  provenance.set(r.s, hit);
  if (hit.kind === 'EXACT') buckets.coveredExact.push(r);
  else if (hit.kind === 'INTERNAL') buckets.coveredInternal.push(r);
  else buckets.coveredPattern.push(r);
}

/* --- dictionary-side findings --------------------------------------- */

// AMBIGUOUS-DUP: the same English key declared more than once in EXACT.
const dupKeys = duplicateExactKeys();

/*
 * AMBIGUOUS-MULTI: one sentence, two or more PATTERNS rules that can render
 * it. Which one wins is decided by their order in the list, so deleting or
 * reordering a rule silently swaps the Chinese on screen — that is the trap
 * worth failing the build over.
 *
 * Deliberately NOT counted: an EXACT hit that also matches a PATTERNS rule.
 * `zhError()` checks EXACT first, so that is shadowing with a deterministic
 * winner, not ambiguity. Those pairs are reported separately below (EXACT
 * SHADOWING) as a note, so the relationship stays visible.
 */
const multi = [];
const shadowedByExact = [];
for (const r of rows) {
  if (classify(r)) continue;
  const pats = compiledPatterns.filter((c) => c.re.test(r.s));
  if (pats.length > 1) {
    multi.push({ r, cands: pats.map((c) => ({ kind: `PATTERNS#${c.i}`, zh: c.zh })) });
  } else if (pats.length === 1 && Object.prototype.hasOwnProperty.call(zh.EXACT, r.s)) {
    shadowedByExact.push({ r, exact: zh.EXACT[r.s], pat: pats[0] });
  }
}

// SHADOWED: a PATTERNS entry that can never win, because an earlier rule
// already matches every sentence it would match.
const shadowed = [];
compiledPatterns.forEach((c) => {
  const losers = rows.filter((r) => !classify(r) && c.re.test(r.s) && covering(r.s) !== null
    && covering(r.s).kind === 'PATTERNS' && covering(r.s).i !== c.i);
  if (losers.length) shadowed.push({ c, losers });
});

/*
 * ORPHAN: a dictionary entry the Rust engine never actually uses.
 *
 * Measured by USAGE, not by textual presence: an entry is dead when no
 * extracted literal is rendered by it. That avoids false positives such as
 * `asset not found`, which the engine only ever emits interpolated
 * (`asset not found: §1`) and which is therefore rendered by a PATTERNS entry
 * while the bare EXACT key still earns its keep elsewhere.
 */
const usage = new Map();                       // "EXACT|<key>" / "PATTERNS|<i>" -> count
for (const r of rows) {
  const hit = covering(r.s);
  if (!hit) continue;
  const k = `${hit.kind}|${hit.kind === 'PATTERNS' ? hit.i : hit.id}`;
  usage.set(k, (usage.get(k) || 0) + 1);
}

/*
 * Rescue pass — keeps a scraping limitation from faking a dead entry.
 *
 * `looksSentence` (the rule, kept verbatim) only accepts characters from a
 * fixed class, and `^` / `$` are not in it. A message such as
 * `MCP server name must match /^…$/` is therefore never scraped, and usage
 * measured off the scrape alone would report it as an ORPHAN — a false
 * positive that would have deleted a live translation. So an entry also
 * counts as used when some extracted *production literal* renders it, even
 * though that literal was not judged as prose.
 */
const literalForms = new Set();
for (const r of all) { literalForms.add(r.s); if (r.raw) literalForms.add(r.raw); }
for (const s of literalForms) {
  const hit = covering(s);
  if (!hit) continue;
  const k = `${hit.kind}|${hit.kind === 'PATTERNS' ? hit.i : hit.id}`;
  usage.set(k, (usage.get(k) || 0) + 1);
}

const orphans = [];
for (const k of Object.keys(zh.EXACT)) {
  if (usage.get(`EXACT|${k}`)) continue;
  // Same entry point the coverage check uses to route this string to the log
  // surface. Skipping it here is what keeps the two checks from disagreeing:
  // "emitted but not to the screen" and "dead, delete it" cannot both hold.
  if (isLogSurface(k)) continue;
  orphans.push({ kind: 'EXACT', id: k, zh: zh.EXACT[k] });
}
compiledPatterns.forEach((c) => {
  if (usage.get(`PATTERNS|${c.i}`)) return;
  if (isLogSurface(c.src)) return;
  orphans.push({ kind: `PATTERNS#${c.i}`, id: c.src, zh: c.zh });
});

// ORPHAN is judged against the Rust engine only: an unused entry is simply an
// ORPHAN.

/* ------------------------------------------------------------------ */
/* 7b. code integrity + key-language integrity                         */

const codeLiterals = [];
for (const f of files) codeLiterals.push(...extractCodes(f));

const codeSeen = new Set();
const codes = codeLiterals.filter((c) => {
  const k = `${c.file}|${c.line}|${c.code}`;
  if (codeSeen.has(k)) return false;
  codeSeen.add(k);
  return true;
});

/** A code on a wire the renderer can see: json surface, code table, or a
 *  Serialize'd struct field. Everything else (`code:` on a struct that is
 *  never serialized) is reported separately, not gated. */
const emittedCodes = codes.filter((c) => c.rule !== 'field' || c.serialized);
const internalCodes = codes.filter((c) => c.rule === 'field' && !c.serialized);

const codeMiss = emittedCodes.filter((c) => !Object.prototype.hasOwnProperty.call(zh.CODE_ZH || {}, c.code));
const badKeys = cjkKeys();

/* ------------------------------------------------------------------ */
/* 8. Report                                                           */

const W = (s) => process.stdout.write(s + '\n');
const fmt = (r) => `    ${r.file}:${r.line}  "${r.s}"`;
/**
 * Discretionary buckets (the ones a human judgement put here, rather than a
 * mechanical rule) are always listed in full — even in gate mode — so they
 * cannot quietly grow. Mechanical buckets are count-only unless --report.
 */
const ALWAYS_LIST = ['cli', 'protocol', 'logSurface', 'chinese'];
const show = (label, list, extra) => {
  W(`  ${label}: ${list.length}`);
  if (list.length && (reportOnly || ALWAYS_LIST.includes(label))) W(list.map(fmt).join('\n'));
  if (extra) W(extra);
};

W('');
W('Rust engine i18n audit — no-English-degradation gate');
W(`  source : ${RUST_SRC_DIRS.map((d) => path.relative(ROOT, d).replace(/\\/g, '/')).join(', ')}`);
W(`  dict   : ${path.relative(ROOT, DICT).replace(/\\/g, '/')}`);
W('');
W(`  rust files scanned        : ${files.length}`);
W(`  string literals extracted : ${all.length}`);
W('');
show('COVERED by EXACT', buckets.coveredExact, reportOnly ? undefined : '    (--report lists each)');
show('COVERED by PATTERNS', buckets.coveredPattern, reportOnly ? undefined : '    (--report lists each)');
show('COVERED by INTERNAL (never reaches the UI)', buckets.coveredInternal);
W('');
W('  -- MISS (fails the build) --');
W(`  MISS — English reaches the screen: ${buckets.miss.length}`);
if (buckets.miss.length) W(buckets.miss.map(fmt).join('\n'));
W('');
W('  -- excluded by scraping rule, not by the dictionary --');
show('cli', buckets.cli,
  '    ^ src/bin/*.rs: terminal output, never the WebView. File-scoped; watch this count.');
show('protocol', buckets.protocol);
show('logSurface', buckets.logSurface,
  '    ^ literals handed to a log sink (update manager `LogFn`, `log::*`, the\n' +
  `      ${LOG_PREFIXES.map((p) => `\`${p}\``).join(' / ')} channel prefix). Exempted by OUTPUT CHANNEL,\n` +
  '      not by file: production installs `|_| {}` as the sink, so these reach\n' +
  '      neither the WebView nor a terminal. Listed in full — watch this count.');
show('chinese', buckets.chinese,
  '    ^ hard-coded Chinese in the Rust engine. Renders correctly but bypasses\n' +
  '      the dictionary, so it cannot follow a UI-language switch. Engine-side\n' +
  '      debt, not a degradation of the guarantee.');
show('escaped', buckets.escaped);
show('notProse', buckets.notProse);
show('placeholder', buckets.placeholder);

const excluded = Object.keys(buckets)
  .filter((k) => !['miss', 'coveredExact', 'coveredPattern', 'coveredInternal'].includes(k))
  .reduce((n, k) => n + buckets[k].length, 0);
W('');
W(`  total excluded: ${excluded}   candidates judged: ${rows.length - excluded}`);
if (!reportOnly) W('  (--report lists every bucket)');

/* --- dictionary-side report ----------------------------------------- */
W('');
W('  -- dictionary → engine (report only, nothing is deleted) --');
W(`  AMBIGUOUS-DUP (same key declared twice in EXACT): ${dupKeys.length}`);
for (const [k, n] of dupKeys) W(`    "${k}" declared ${n}x`);
if (dupKeys.length) {
  W('    ^ a duplicate object key silently wins in JS, so only one Chinese is');
  W('      ever used while the audit still counts the entry as covered.');
}
W(`  AMBIGUOUS-MULTI — FAILS THE BUILD (>1 PATTERNS rule renders one sentence): ${multi.length}`);
for (const { r, cands } of multi.slice(0, 10)) {
  W(`    ${r.file}:${r.line} "${r.s}"`);
  for (const c of cands) W(`        ${c.kind}: ${c.zh}`);
}
if (multi.length) {
  W('    ^ the first rule in PATTERNS wins; delete or reorder it and the same');
  W('      English silently renders as different Chinese.');
}
W(`  EXACT SHADOWING (expected — EXACT is checked before PATTERNS): ${shadowedByExact.length}`);
for (const { r, exact, pat } of shadowedByExact.slice(0, 10)) {
  W(`    ${r.file}:${r.line} "${r.s}"`);
  W(`        wins: EXACT "${exact}"   shadowed: PATTERNS#${pat.i} ${pat.src}`);
}
W(`  SHADOWED PATTERNS (can never win; an earlier rule matches first): ${shadowed.length}`);
for (const { c, losers } of shadowed.slice(0, 10)) {
  W(`    PATTERNS#${c.i} ${c.src}`);
  for (const l of losers.slice(0, 3)) W(`        loses to it on "${l.s}"`);
}
W(`  ORPHAN (dictionary entry no Rust source produces): ${orphans.length}`);
for (const o of orphans.slice(0, 10)) W(`    ${o.kind}: ${o.id}  →  ${o.zh}`);
if (orphans.length > 10) W(`    … ${orphans.length - 10} more (--report)`);
if (reportOnly) for (const o of orphans.slice(10)) W(`    ${o.kind}: ${o.id}  →  ${o.zh}`);

/* --- code / key integrity ------------------------------------------ */
W('');
W('  -- code integrity (Rust code literal → CODE_ZH) --');
W(`  code literals extracted from Rust : ${codes.length}` +
  `   (emitted ${emittedCodes.length}, struct-internal ${internalCodes.length})`);
W(`  CODE-MISS (fails the build — a code the renderer can see, untranslated): ${codeMiss.length}`);
for (const c of codeMiss) W(`    ${c.file}:${c.line}  "${c.code}"   [${c.rule}]`);
if (codeMiss.length) {
  W('    ^ add it to CODE_ZH in src/renderer/engine-zh.js, or stop emitting it.');
}
W('  CODE-INTERNAL (struct field `code`, struct does not derive Serialize:');
W('    cannot reach the renderer, reported not gated): ' + internalCodes.length);
for (const c of internalCodes) W(`    ${c.file}:${c.line}  "${c.code}"`);
if (internalCodes.length) {
  W('    ^ if the struct ever derives Serialize this flips to CODE-MISS.');
}
W(`  CJK-KEY (fails the build — Chinese in an English key slot): ${badKeys.length}`);
for (const b of badKeys) W(`    ${b.where}  "${b.key}"  →  ${b.zh}`);

if (buckets.miss.length) {
  W('');
  W('  Suggested dictionary additions (paste into src/renderer/engine-zh.js):');
  for (const r of buckets.miss) {
    if (/§\d/.test(r.s)) {
      const pat = `^${r.s}$`;
      const ok = zh.patternToRegExp(pat).test(r.s);
      W(`    // ${r.file}:${r.line}`);
      W(`    ['${pat.replace(/'/g, "\\'")}', '<中文>'],   // ${ok ? 'regex ok' : 'REGEX DOES NOT ROUND-TRIP'}`);
    } else {
      W(`    // ${r.file}:${r.line}`);
      W(`    '${r.s.replace(/'/g, "\\'")}': '<中文>',`);
    }
  }
}

const failed = buckets.miss.length + dupKeys.length + codeMiss.length + badKeys.length + multi.length;
if (failed) {
  W('');
  console.error(`[tauri-i18n] FAILED — ${buckets.miss.length} MISS, ${multi.length} ambiguous pattern(s), `
    + `${dupKeys.length} duplicate key(s), ${codeMiss.length} CODE-MISS, ${badKeys.length} CJK key(s).`);
  if (!reportOnly) process.exit(1);
  W('  (--report: exiting 0)');
} else {
  W('');
  W('[tauri-i18n] OK — every Rust engine sentence has a zh mapping.');
}
W('');
process.exit(0);
