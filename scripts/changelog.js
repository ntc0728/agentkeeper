#!/usr/bin/env node
'use strict';
/**
 * The release-note record: ONE file, ONE release cycle, replaced — never accumulated.
 *
 *   changelog/next.json     what changed since the last release
 *
 *   node scripts/changelog.js show                    # record + the rendered body
 *   node scripts/changelog.js add fix "…"             # append one entry
 *   node scripts/changelog.js summary "…"             # the one-line title of the release
 *   node scripts/changelog.js render --version 0.2.1  # the markdown body a release carries
 *   node scripts/changelog.js check                   # validate, exit 1 on any finding
 *   node scripts/changelog.js reset                   # clear it after the release went out
 *
 * Why a file instead of `generate_release_notes: true`: this repository merges
 * without pull requests, so GitHub's generated notes are exactly one line —
 * "Full Changelog: compare/v0.2.0...v0.2.1" — which is what an update check
 * showed to the user. The notes are read inside the app (the update panel shows
 * the release body), so they have to say what changed, in the language the rest
 * of the product speaks.
 *
 * Why one file and not a growing CHANGELOG.md: the body of a release describes
 * that release. A file that accumulates history invites editing old releases by
 * accident, and the archive already exists — every published body stays on the
 * Releases page. After a release goes out, `reset` empties the record; the next
 * release writes over it. No history is kept here on purpose.
 *
 * What goes in: changes a user of the build can notice — behaviour, fixes,
 * performance, UI copy that moved. What does not: docs-only edits, CI/tooling
 * churn, formatting, refactors with an identical observable behaviour. A record
 * that lists everything is a record nobody reads.
 *
 * Kinds are fixed to three, in the order the reference project uses: 修复 /
 * 新增 / 优化. They render as the bullet prefix, and the number is added by the
 * renderer, so the record never stores "1." and cannot drift out of order.
 */

const fs = require('fs');
const path = require('path');

const ROOT = path.join(__dirname, '..');
const RECORD = path.join(ROOT, 'changelog', 'next.json');

/** Kind → bullet prefix. The key is stored, the value is rendered. */
const KINDS = { fix: '修复', feat: '新增', opt: '优化' };
const KIND_HINT = Object.keys(KINDS).join(' / ');

class RecordError extends Error {}

function rel() {
  return path.relative(ROOT, RECORD).replace(/\\/g, '/');
}

function read() {
  let raw;
  try {
    raw = fs.readFileSync(RECORD, 'utf8');
  } catch (e) {
    throw new RecordError(`cannot read ${rel()}: ${e.code || e.message}`);
  }
  let data;
  try {
    data = JSON.parse(raw);
  } catch (e) {
    throw new RecordError(`${rel()} is not valid JSON: ${e.message}`);
  }
  return data;
}

function write(data) {
  fs.mkdirSync(path.dirname(RECORD), { recursive: true });
  fs.writeFileSync(RECORD, JSON.stringify(data, null, 2) + '\n');
}

/** Every structural problem, so one run reports all of them. */
function findings(data) {
  const out = [];
  if (data === null || typeof data !== 'object' || Array.isArray(data)) {
    return [`${rel()} must be a JSON object with "summary" and "entries"`];
  }
  for (const key of Object.keys(data)) {
    if (key !== 'summary' && key !== 'entries') out.push(`unknown key "${key}"`);
  }
  if (typeof data.summary !== 'string') out.push('"summary" must be a string');
  if (!Array.isArray(data.entries)) {
    out.push('"entries" must be an array');
    return out;
  }
  const seen = new Map();
  data.entries.forEach((entry, i) => {
    const at = `entries[${i}]`;
    if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) {
      out.push(`${at} must be an object with "kind" and "text"`);
      return;
    }
    for (const key of Object.keys(entry)) {
      if (key !== 'kind' && key !== 'text') out.push(`${at} has unknown key "${key}"`);
    }
    if (!Object.prototype.hasOwnProperty.call(KINDS, entry.kind)) {
      out.push(`${at}.kind must be one of ${KIND_HINT} (got ${JSON.stringify(entry.kind)})`);
    }
    if (typeof entry.text !== 'string' || !entry.text.trim()) {
      out.push(`${at}.text must be a non-empty string`);
      return;
    }
    const text = entry.text.trim();
    // One bullet is one sentence a reader can scan; the renderer owns the "1."
    // and a second line would break the numbered list.
    if (/[\r\n]/.test(text)) out.push(`${at}.text must be a single line`);
    if (/^\d+\./.test(text)) out.push(`${at}.text must not start with its own number — the renderer numbers the list`);
    if (seen.has(text)) out.push(`${at}.text duplicates entries[${seen.get(text)}]`);
    else seen.set(text, i);
  });
  // The title line is `0.2.1: <summary>`; an empty one renders a bare version.
  if (data.entries.length && typeof data.summary === 'string' && !data.summary.trim()) {
    out.push('"summary" must be filled in when "entries" is not empty — it is the line the release is titled with');
  }
  return out;
}

function checked() {
  const data = read();
  const problems = findings(data);
  if (problems.length) {
    throw new RecordError(`${rel()}:\n  - ${problems.join('\n  - ')}`);
  }
  return data;
}

function normaliseVersion(value) {
  const version = String(value || '').trim().replace(/^v/, '');
  if (!/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(version)) {
    throw new RecordError(`--version must be a version or a v-prefixed tag (got ${JSON.stringify(value)})`);
  }
  return version;
}

/** `0.2.1: 标题` + one numbered bullet per entry. */
function render(data, version, allowEmpty) {
  if (!data.entries.length && !allowEmpty) {
    throw new RecordError(
      `${rel()} has no entries — write the changes before releasing:\n` +
        '  node scripts/changelog.js add fix "…"\n' +
        '  node scripts/changelog.js summary "…"\n' +
        'Pass --allow-empty only for a release that genuinely ships nothing.'
    );
  }
  const summary = String(data.summary || '').trim();
  const lines = [summary ? `${version}: ${summary}` : version];
  if (data.entries.length) {
    lines.push('');
    data.entries.forEach((entry, i) => {
      lines.push(`${i + 1}. ${KINDS[entry.kind]}: ${entry.text.trim()}`);
    });
  }
  return lines.join('\n') + '\n';
}

const USAGE = `usage: node scripts/changelog.js <command>

  show                                  print the record and the body it renders
  add <${KIND_HINT}> <text>              append one entry
  summary <text>                        set the one-line release title
  render --version <x.y.z|vX.Y.Z>       print the markdown body (--allow-empty to accept none)
  check                                 validate the record
  reset                                 clear it after the release went out
`;

function takeOption(argv, name) {
  const at = argv.indexOf(name);
  if (at < 0) return undefined;
  const value = argv[at + 1];
  argv.splice(at, 2);
  return value;
}

function main(argv) {
  const command = argv.shift();

  if (command === 'show') {
    const data = checked();
    console.log(`${rel()}  (${data.entries.length} entr${data.entries.length === 1 ? 'y' : 'ies'})`);
    console.log(JSON.stringify(data, null, 2));
    console.log('\n--- rendered ---');
    process.stdout.write(render(data, '0.0.0', true));
    return 0;
  }

  if (command === 'add') {
    const kind = argv.shift();
    const text = argv.join(' ').trim();
    if (!kind || !text) {
      console.error(`add needs a kind (${KIND_HINT}) and a text\n\n${USAGE}`);
      return 2;
    }
    const data = checked();
    data.entries.push({ kind, text });
    const problems = findings(data);
    if (problems.length) {
      console.error(`not written — ${rel()} would be invalid:\n  - ${problems.join('\n  - ')}`);
      return 1;
    }
    write(data);
    console.log(`added ${KINDS[kind]} entry — ${data.entries.length} in ${rel()}`);
    return 0;
  }

  if (command === 'summary') {
    const text = argv.join(' ').trim();
    if (!text) {
      console.error(`summary needs a text\n\n${USAGE}`);
      return 2;
    }
    const data = read();
    data.summary = text;
    const problems = findings(data);
    if (problems.length) {
      console.error(`not written — ${rel()} would be invalid:\n  - ${problems.join('\n  - ')}`);
      return 1;
    }
    write(data);
    console.log(`summary set in ${rel()}`);
    return 0;
  }

  if (command === 'render') {
    const allowEmpty = argv.includes('--allow-empty');
    const version = normaliseVersion(takeOption(argv, '--version') || process.env.GITHUB_REF_NAME);
    const data = checked();
    process.stdout.write(render(data, version, allowEmpty));
    return 0;
  }

  if (command === 'check') {
    const data = checked();
    const flush = data.entries.filter((e) => /^v?\d+\.\d+\.\d+/.test(e.text)).length;
    console.log(
      `  ok    ${rel()}: ${data.entries.length} entr${data.entries.length === 1 ? 'y' : 'ies'}` +
        (flush ? ` (${flush} mention a version — the "版本: …" line is written by the renderer)` : '')
    );
    return 0;
  }

  if (command === 'reset') {
    write({ summary: '', entries: [] });
    console.log(`cleared ${rel()} — the next release writes over it`);
    return 0;
  }

  console.error(USAGE);
  return command === undefined ? 2 : 2;
}

try {
  process.exitCode = main(process.argv.slice(2));
} catch (e) {
  console.error(e instanceof RecordError ? e.message : e);
  process.exitCode = 1;
}
