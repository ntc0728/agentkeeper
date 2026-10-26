'use strict';
/**
 * AgentKeeper renderer. Talks to the main process only through window.ak.
 * No require, no fs — the CSP in index.html enforces it.
 */

/* Engine-layer message translation (core/ + ipc errors are English; the
   zh UI must never show them raw — no-degradation policy). */
const { zhText, zhField, zhError, zhCode } = window.__akEngineZh;

const S = {
  view: 'overview',  boot: null,
  data: null,
  settings: {},
  lastIssueCount: null,
  localStats: null,
  filter: '',
  kindFilter: 'all',
  agentFilter: 'all',
  scopeFilter: 'all',
  originFilter: 'curated',
  sortKey: null,
  onlyShadowed: false,
  page: 1,
  checked: new Set(),
  selected: null,
  lastDiff: null,
  dirty: false,
  discQ: '',
  discResults: null,
  discTotal: 0,
  discLoading: false,
  discError: null,
  // --- software update ------------------------------------------------
  // Snapshot pushed by the main process (`ak:update-status` / `ak:update-event`).
  update: null,
  updateBusy: false,
  // Input drafts survive the full re-render that a progress poll triggers.
  updateMinutesDraft: '',
  updateTokenDraft: '',
  updateTimer: null,
  updatePollTimer: null,
  // Version we already showed the dialog for in this session ("Later" is not
  // remembered across restarts — "skip" is, and lives in main-process state).
  updatePrompted: '',
  // --- boot retry bookkeeping -----------------------------------------
  // boot() is also the retry target of the startup error panel, so every
  // one-time wire* inside it is guarded by one of these flags. Without them a
  // retry stacks listeners: two scans per click, two update dialogs, two
  // language listeners on one element.
  wired: false,
  langHooked: false,
  updateWired: false,
  updateEventHooked: false,
  notifyWired: false
};

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));
/**
 * Create an element.
 *
 * `style` is applied through the CSSOM (element.style.setProperty) rather than
 * a style attribute. A strict CSP with `style-src 'self'` blocks inline style
 * attributes, and we do NOT want to weaken the policy with 'unsafe-inline'.
 * CSSOM writes are not subject to style-src, so dynamic values (bar widths,
 * per-agent accent colours) work under the strict policy.
 */
const el = (tag, attrs = {}, ...kids) => {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') n.className = v;
    else if (k === 'html') n.innerHTML = v;
    else if (k === 'style') {
      // A string is `prop: value; …`; an object is the same thing without the
      // stringly-typed round trip, which also keeps data-driven values (an
      // adapter's accent, its matching ink) out of the i18n prose scan.
      if (typeof v === 'string') applyStyle(n, v);
      else for (const [prop, val] of Object.entries(v)) n.style.setProperty(prop, val);
    }
    else if (k.startsWith('on') && typeof v === 'function') n.addEventListener(k.slice(2).toLowerCase(), v);
    else if (v !== null && v !== undefined && v !== false) n.setAttribute(k, v === true ? '' : String(v));
  }
  for (const k of kids.flat()) {
    if (k === null || k === undefined || k === false || k === true) continue;
    n.appendChild(typeof k === 'string' || typeof k === 'number' ? document.createTextNode(String(k)) : k);
  }
  return n;
};

/** Apply a `a:1; b:2` style string via the CSSOM. */
function applyStyle(node, css) {
  for (const decl of String(css).split(';')) {
    const i = decl.indexOf(':');
    if (i < 0) continue;
    const prop = decl.slice(0, i).trim();
    const val = decl.slice(i + 1).trim();
    if (prop && val) node.style.setProperty(prop, val);
  }
}

const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) =>
  ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

/**
 * Token counts and other plain magnitudes, with the unit ladder spelled out.
 *
 * One formatter for every count shown in the UI. A hundreds-of-millions value
 * must render as "102.9M": dividing by 1000 and appending "k" produces
 * "102909.9k", which is not an SI form of anything.
 */
function count(n) {
  const v = Number(n) || 0;
  if (v >= 1e9) return `${(v / 1e9).toFixed(1).replace(/\.0$/, '')}B`;
  if (v >= 1e6) return `${(v / 1e6).toFixed(1).replace(/\.0$/, '')}M`;
  if (v >= 1e4) return `${(v / 1e3).toFixed(1).replace(/\.0$/, '')}k`;
  return v.toLocaleString('en-US');
}

/* ------------------------------------------------- data-driven logo ink --- */

/**
 * Resolve any CSS colour to [r, g, b, a] by letting the engine parse it.
 *
 * The logo tile's background comes from adapter data (accent), which is a hex
 * string today but is not ours to constrain. Reading it back through
 * getComputedStyle means named colours, rgb() and color(display-p3 …) all
 * resolve exactly as the engine will paint them.
 */
function akColor(css) {
  const probe = document.createElement('span');
  probe.style.color = css;
  probe.style.display = 'none';
  document.body.appendChild(probe);
  const resolved = getComputedStyle(probe).color;
  probe.remove();
  const m = /rgba?\(\s*([\d.]+)[,\s]+([\d.]+)[,\s]+([\d.]+)(?:[,\s/]+([\d.]+))?/i.exec(resolved);
  if (!m) return null;
  const a = m[4] === undefined ? 1 : Number(m[4]);
  return [Number(m[1]), Number(m[2]), Number(m[3]), isFinite(a) ? a : 1];
}

/** WCAG relative luminance of an opaque [r, g, b]. */
function akLuminance([r, g, b]) {
  const lin = (v) => {
    const s = v / 255;
    return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

/** Source-over composite of top (possibly translucent) onto opaque base. */
function akOver(top, base) {
  const a = top[3];
  return [0, 1, 2].map((i) => Math.round(top[i] * a + base[i] * (1 - a)));
}

/**
 * Ink for the agent logo chip — chosen, not fixed.
 *
 * The tile painted color: var(--knob) (white) over whatever accent the
 * adapter declared. #c9c9c9 is in the shipped adapter table and measured
 * 1.63:1, so the two-letter label was unreadable. Picking the better of black
 * and white cannot fail: the worst case for max(C_white, C_black) is 4.53:1,
 * already above AA (4.5:1) for 11 px bold text. A translucent accent is
 * composited over the surface it sits on first, so the pair measured is the pair
 * painted.
 */
function akLogoInk(accent) {
  const surface = akColor('var(--surface)') || [255, 255, 255, 1];
  const fallback = akColor('var(--agent-fallback)') || [75, 85, 99, 1];
  const declared = accent ? akColor(accent) : null;
  const top = declared && declared[3] > 0 ? declared : fallback;
  const bg = akOver(top, surface);
  const L = akLuminance(bg);
  const white = 1.05 / (L + 0.05);
  const black = (L + 0.05) / 0.05;
  const ink = white >= black ? '#ffffff' : '#000000';
  // hex, not rgb(): the lint gate cannot tell a CSS function call from an
  // untranslated sentence, and a hex string is what an accent already is.
  const hex = '#' + bg.map(function (v) { return v.toString(16).padStart(2, '0'); }).join('');
  return {
    background: hex,
    color: ink,
    ratio: Math.round(Math.max(white, black) * 100) / 100
  };
}

function bytes(n) {
  if (!n) return '0 B';
  const u = ['B', 'KB', 'MB', 'GB'];
  let i = 0, v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v >= 10 || i === 0 ? 0 : 1)} ${u[i]}`;
}

function ago(iso) {
  if (!iso) return '—';
  const s = (Date.now() - new Date(iso).getTime()) / 1000;
  // Relative time is user-visible prose, so it goes through t() like everything
  // else: a Chinese UI with "1h ago" in a column is the exact defect the i18n
  // rule exists for. Units are not translated in bytes() — KB/MB are SI symbols.
  if (s < 60) return t('common.agoSeconds', { n: Math.floor(s) });
  if (s < 3600) return t('common.agoMinutes', { n: Math.floor(s / 60) });
  if (s < 86400) return t('common.agoHours', { n: Math.floor(s / 3600) });
  return t('common.agoDays', { n: Math.floor(s / 86400) });
}

function clock(iso) {
  if (!iso) return '—';
  const d = new Date(iso);
  return d.toLocaleString('zh-CN', { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

/* ------------------------------------------------------------------ */
/* error reporting                                                     */
/* ------------------------------------------------------------------ */

/**
 * Renderer exceptions → the main process log (`<storeRoot>/ui-errors.jsonl`).
 *
 * Why: before this, an exception in this window left a trace only in the
 * WebView2 devtools, which nobody sees once the app is packaged. The shell
 * appends one JSON line per report (it keeps the newest 200 and never panics).
 *
 * The report shape is fixed by the Rust command:
 *   { kind, message, stack, view, at }
 *
 * Three rules, all of them about not making the failure worse than the bug:
 *   1. Never assume the command exists — the bridge of an older build has no
 *      `uiError`, so the call is guarded and its rejection swallowed.
 *   2. Never block — nothing here is awaited by a caller, and every field is
 *      truncated before it crosses IPC (a 2 MB stack would stall the call).
 *   3. Never storm — one report per distinct message per session, at most 20
 *      per session, and conditions that are normal operation (a cancelled
 *      dialog, a failed update fetch) are filtered out before they count.
 */
const UI_ERROR = {
  // Keyed by the truncated message: the same exception re-thrown in a loop is
  // one line in the log, not thousands.
  seen: new Set(),
  sent: 0,
  cap: 20,
  maxMessage: 2000,
  maxStack: 4000
};

/**
 * Conditions that are part of normal operation, not bugs.
 *
 * `abort`/`cancel` covers a dialog the user dismissed; the fetch/socket words
 * cover the update check failing because the machine is offline. IPC itself is
 * in the list because a torn-down window rejects every in-flight invoke, and
 * reporting that on quit would write noise on every exit.
 */
const UI_ERROR_EXPECTED = /(?:abort|cancel|network|failed to fetch|offline|timed? ?out|ipc)/i;

function safeJson(v) {
  try { return JSON.stringify(v); } catch (_e) { return String(v); }
}

/**
 * Send one report. Synchronous and total: it returns before any IPC work
 * happens, and it cannot throw (a throw inside `error` would recurse).
 */
function reportUiError(kind, err) {
  try {
    const message = String((err && err.message) || err || '').slice(0, UI_ERROR.maxMessage);
    if (!message) return;
    // A call site may mark an expected failure explicitly; the regex is the
    // fallback for the ones that come from the platform.
    if (err && err.akExpected === true) return;
    if (UI_ERROR_EXPECTED.test(message)) return;
    if (UI_ERROR.seen.has(message)) return;
    if (UI_ERROR.sent >= UI_ERROR.cap) return;
    const ak = window.ak;
    if (!ak || typeof ak.uiError !== 'function') return;
    UI_ERROR.seen.add(message);
    UI_ERROR.sent++;
    const p = ak.uiError({
      kind,
      message,
      stack: String((err && err.stack) || '').slice(0, UI_ERROR.maxStack),
      view: S.view || '',
      at: new Date().toISOString()
    });
    // A rejected report must never surface as an unhandled rejection (which
    // would report itself, and so on).
    if (p && typeof p.catch === 'function') p.catch(() => {});
  } catch (_e) { /* reporting is best-effort by definition */ }
}

/**
 * Catch what the renderer does not.
 *
 * Registered once, at the end of the file, before the first boot(): `error`
 * covers throws from event handlers and timers, `unhandledrejection` covers
 * async paths that forgot a catch. Both handlers stay cheap and synchronous.
 */
function wireErrorReporting() {
  window.addEventListener('error', (ev) => {
    // A failed <img>/<script> load reports an Event with no Error attached;
    // the load target is the only useful information in it.
    const target = ev.target;
    if (!ev.error && target && target !== window && target.tagName) {
      // Log-only text (never rendered), so it is not a t() key.
      reportUiError('error', 'resource load failed: ' + (target.src || target.href || target.tagName));
      return;
    }
    reportUiError('error', ev.error || { message: ev.message });
  });
  window.addEventListener('unhandledrejection', (ev) => {
    const reason = ev.reason;
    if (reason instanceof Error) { reportUiError('unhandledrejection', reason); return; }
    reportUiError('unhandledrejection', { message: typeof reason === 'string' ? reason : safeJson(reason) });
  });
}

/* ------------------------------------------------------------------ */
/* retry panel                                                         */
/* ------------------------------------------------------------------ */

/**
 * An error message with a way out.
 *
 * A failed boot or a failed repaint must not leave a restart as the only
 * recovery. The panel keeps the message and adds one button that re-runs
 * exactly the step that failed (boot → bootstrap+scan, scan → scan, view →
 * its data load).
 */
function errorPanel(message, retry) {
  const btn = el('button', { class: 'btn sm', type: 'button' }, t('common.retry'));
  // A disabled button already swallows the second click of a double click;
  // runRetry re-checks too, so a synthetic .click() cannot queue a second call.
  btn.addEventListener('click', () => { void runRetry(btn, retry); });
  return el('div', { class: 'alert err', role: 'alert' },
    el('div', { class: 'alert-msg' }, message),
    el('div', { class: 'alert-hint' }, t('err.retryHint')),
    el('div', { class: 'alert-actions' }, btn));
}

/**
 * Compact "this panel could not load its own data" + retry.
 *
 * The view-level error panel is too heavy for a single row inside a card (the
 * local-stats line, the baseline body). A bare "—" or an endless "loading…"
 * leaves no way out; the retry re-renders the view, which re-runs the load.
 */
function inlineRetry(box, retry) {
  box.textContent = '';
  const btn = el('button', { class: 'btn sm', type: 'button' }, t('common.retry'));
  btn.addEventListener('click', () => { void runRetry(btn, retry); });
  box.appendChild(el('span', { class: 'chip err mr-6' }, t('err.loadFailed')));
  box.appendChild(btn);
}

/**
 * Run one retry, with the button locked for the duration.
 *
 * `disabled` + the text swap are the visible half ("retrying…"); the early
 * return is the correctness half — the step may take seconds, and the user
 * will click again.
 */
async function runRetry(btn, retry) {
  if (!btn || btn.disabled) return;
  btn.disabled = true;
  btn.classList.add('loading');
  btn.textContent = t('common.retrying');
  try {
    await retry();
  } catch (_e) {
    // The retried step reports and paints its own failure; swallowing here
    // keeps the promise from becoming an unhandled rejection.
  } finally {
    // A successful retry normally replaces #view-root, so the button may be
    // detached by now — re-enabling it then is harmless but pointless.
    if (document.contains(btn)) {
      btn.disabled = false;
      btn.classList.remove('loading');
      btn.textContent = t('common.retry');
    }
  }
}

/* ------------------------------------------------------------------ */
/* toast / modal                                                       */
/* ------------------------------------------------------------------ */

function toast(msg, kind = '') {
  // Engine errors arrive in English; the zh UI never shows them raw.
  msg = zhError(msg);
  // Named `node`, not `t` — `t` is the module-scope i18n translator.
  const node = el('div', { class: `toast ${kind}` }, msg);
  // #toasts is role=status aria-live=polite, so appending is enough to have it
  // announced; the timeout only removes the visual bubble afterwards.
  $('#toasts').appendChild(node);
  setTimeout(() => {
    node.style.transition = 'opacity .2s';
    node.style.opacity = '0';
    setTimeout(() => node.remove(), 220);
  }, kind === 'err' ? 5200 : 2600);
}

/* --- dialog focus bookkeeping ---------------------------------------
 * Both overlays are modal surfaces: while open, Tab must cycle inside them and
 * on close focus must come back to whatever opened them. Without this a
 * keyboard user tabs into the page underneath the scrim and loses the focus
 * ring entirely. */

/** Element focused before the dialog opened; focus returns there on close. */
let modalReturnFocus = null;
let drawerReturnFocus = null;

const FOCUSABLE_SEL =
  'a[href], button, input, select, textarea, [tabindex]:not([tabindex="-1"])';

/** Focusable descendants of `root` that are actually rendered. */
function focusables(root) {
  return $$(FOCUSABLE_SEL, root)
    .filter((n) => !n.disabled && n.getClientRects().length > 0);
}

/** Move focus to the first actionable element of a freshly opened dialog. */
function focusInto(root) {
  (focusables(root)[0] || root).focus();
}

/** Keep Tab inside an open dialog instead of letting it walk into the page. */
function trapTab(e, root) {
  if (e.key !== 'Tab') return;
  const list = focusables(root);
  if (!list.length) { e.preventDefault(); root.focus(); return; }
  const first = list[0];
  const last = list[list.length - 1];
  const active = document.activeElement;
  const outside = !root.contains(active);
  if (e.shiftKey && (outside || active === first)) { e.preventDefault(); last.focus(); }
  else if (!e.shiftKey && (outside || active === last)) { e.preventDefault(); first.focus(); }
}

/**
 * Put focus back on the trigger of a closed dialog.
 *
 * A drawer opened from a table row has no focusable trigger (rows are not in
 * the tab order), so the fallback is the active nav item — a stable, visible
 * place rather than `document.body`, where the next Tab would restart from the
 * top of the page.
 */
function returnFocus(node) {
  if (node && document.contains(node) && typeof node.focus === 'function') {
    node.focus();
    return;
  }
  const nav = $('.nav-item.active');
  if (nav) nav.focus();
}

/**
 * Report a failed write, and distinguish the three failures that are not the
 * same thing to the user.
 *
 * 1. The write was refused before it started (`store.snapshotTooLarge`): the
 *    asset is bigger than the undo budget, so *nothing* was written and the
 *    tree is exactly as it was. A sentence is the whole story.
 * 2. The write ran and the undo put everything back: nothing on disk changed.
 *    Again a sentence is the whole story.
 * 3. The write ran and the undo could NOT put everything back
 *    (`store.restoreIncomplete` + `restoreErrors`): the tree is now in a state
 *    nobody chose. That is a data problem the user has to see in full, with
 *    every path named, because they are the only ones who can fix it by hand.
 *    A one-line toast was the reason a half-restored directory could sit there
 *    unnoticed.
 *
 * `restoreErrors` is an array of entries carrying a path, a code and a
 * detail; `restoreError` is the joined legacy string. The codes are stable,
 * so the wording is chosen by looking at them rather than at parsing an
 * English sentence.
 */
function reportWriteFailure(r, fallbackKey) {
  if (!r || r.ok !== false) return toast((r && r.error) || t(fallbackKey), 'err');
  const code = r.errorCode || '';

  // Case 1 — refused before any write.
  if (code === STORE_SNAPSHOT_TOO_LARGE) {
    return toast(t('restore.toast.tooLarge'), 'err');
  }

  const restores = Array.isArray(r.restoreErrors)
    ? r.restoreErrors
    : (r.restoreError ? [{ path: '', code: '', detail: String(r.restoreError) }] : []);
  // Case 2 — failed, but the undo was clean.
  if (!restores.length) {
    return toast(r.error || t(fallbackKey), 'err');
  }

  // Case 3 — partially restored. A symlink is only one way this happens, so
  // the advice line is chosen by what actually failed rather than by a single
  // global guess.
  const symlink = restores.some((e) => e && e.code === STORE_SYMLINK_RESTORE_FAILED);
  const body = el('div', {},
    el('div', { class: 'alert err' }, t(symlink ? 'restore.headline.symlink' : 'restore.headline.partial')),
    el('div', { class: 'muted fs-12 mb-12' }, t('restore.intro')),
    el('ul', { class: 'restore-list' },
      ...restores.map((e) => {
        const path = e && e.path ? String(e.path) : t('restore.unknownPath');
        // The engine sends a path / code / detail triple; zhError reads
        // code + params. The path is passed as the single parameter so a
        // sentence with a `§1` placeholder resolves against it, and the
        // detail string is the fallback when the code is unknown to the
        // dictionary (then zhText maps the English sentence).
        const detail = e && e.code && typeof e === 'object'
          ? (zhCode(e.code, [e.path]) || zhError(e.detail))
          : zhError(String((e && e.detail) || e));
        return el('li', {},
          el('div', { class: 'mono fs-11' }, path),
          el('div', { class: 'muted fs-11' }, detail));
      })),
    el('div', { class: 'muted fs-11 mt-8' }, t(symlink ? 'restore.advice.symlink' : 'restore.advice')));
  openModal(t('restore.title'), body, [
    el('button', { class: 'btn', onclick: closeModal }, t('common.close'))
  ]);
  toast(t(symlink ? 'restore.toast.symlink' : 'restore.toast.partial'), 'err');
}

/* Error codes the main process emits on a failed transaction (store.rs). Kept
 * as literals here on purpose: the bridge crosses a string boundary, and a
 * typo in one file would otherwise surface as a generic fallback rather than as
 * a missing translation. `scripts/check.js bridge` does not cover values, only
 * call shapes, so this list is checked against store.rs by hand. */
const STORE_SNAPSHOT_TOO_LARGE = 'store.snapshotTooLarge';
const STORE_RESTORE_INCOMPLETE = 'store.restoreIncomplete';
const STORE_SYMLINK_RESTORE_FAILED = 'store.symlinkRestoreFailed';

function openModal(title, bodyNode, footNodes = []) {
  modalReturnFocus = document.activeElement;
  $('#modal-title').textContent = title;
  const body = $('#modal-body');
  body.innerHTML = '';
  body.appendChild(bodyNode);
  const foot = $('#modal-foot');
  foot.innerHTML = '';
  for (const f of footNodes) foot.appendChild(f);
  $('#modal-scrim').classList.add('on');
  $('#modal-scrim').setAttribute('aria-hidden', 'false');
  focusInto($('#modal'));
}

/**
 * The confirm promise that is currently waiting for an answer.
 *
 * A dialog can go away through paths its own buttons do not control: Escape,
 * the scrim, ✕. Before this, the promise stayed pending forever — which was
 * invisible for a delete (nothing happened, correctly) but is not for
 * `confirmWrite`, whose caller has to rebuild the editor it covered. One
 * registry, because there is exactly one #modal.
 */
let pendingConfirm = null;

function closeModal() {
  const scrim = $('#modal-scrim');
  if (!scrim.classList.contains('on')) return;
  scrim.classList.remove('on');
  scrim.setAttribute('aria-hidden', 'true');
  // Dismissing a confirmation is an answer: "no". Resolving after the visual
  // close lets a caller restore whatever the dialog was covering.
  if (pendingConfirm) pendingConfirm.settle(false);
  const back = modalReturnFocus;
  modalReturnFocus = null;
  returnFocus(back);
}

function confirmDialog({ title, message, confirmLabel = t('common.confirm'), danger = false, extra = null }) {
  return new Promise((resolve) => {
    let settled = false;
    const entry = {
      settle(v) {
        if (settled) return;
        settled = true;
        if (pendingConfirm === entry) pendingConfirm = null;
        resolve(v);
      }
    };
    pendingConfirm = entry;
    const body = el('div', {}, el('div', { class: 'pre-wrap' }, message));
    if (extra) body.appendChild(extra);
    // Settle BEFORE closing: closeModal() answers any still-pending confirm with
    // false, and that would beat this true.
    const ok = el('button', { class: `btn ${danger ? 'danger' : 'primary'}`, onclick: () => { entry.settle(true); closeModal(); } }, confirmLabel);
    const cancel = el('button', { class: 'btn', onclick: () => { entry.settle(false); closeModal(); } }, t('common.cancel'));
    openModal(title, body, [cancel, ok]);
  });
}

/**
 * Snapshot the dialog on screen, so a stacked confirmation can put it back.
 *
 * The shell has exactly one #modal, so opening the write check while an editor
 * is up replaces that editor's DOM. The nodes are moved (appendChild), not
 * cloned, so keeping references restores the *same* inputs — every keystroke
 * the user already typed survives cancelling the confirmation.
 */
function snapshotModal() {
  const scrim = $('#modal-scrim');
  if (!scrim || !scrim.classList.contains('on')) return null;
  return {
    title: $('#modal-title').textContent,
    body: [...$('#modal-body').childNodes],
    foot: [...$('#modal-foot').childNodes],
    back: modalReturnFocus
  };
}

/**
 * Put a snapshot back on screen.
 *
 * Called from two places for two reasons: a cancelled write check (the editor
 * must look untouched) and a write that failed after its check (the caller
 * only toasts, so the form has to come back or the user loses it).
 */
function restoreModal(snap) {
  if (!snap) return;
  $('#modal-title').textContent = snap.title;
  const body = $('#modal-body');
  body.innerHTML = '';
  for (const n of snap.body) body.appendChild(n);
  const foot = $('#modal-foot');
  foot.innerHTML = '';
  for (const n of snap.foot) foot.appendChild(n);
  modalReturnFocus = snap.back;
  $('#modal-scrim').classList.add('on');
  $('#modal-scrim').setAttribute('aria-hidden', 'false');
  focusInto($('#modal'));
}

/**
 * Destination directories for a set of distribution targets.
 *
 * A target is (agent × scope × project) and the adapter declares the relative
 * directory per kind, so the real path is only knowable here. `withLabel`
 * prefixes the agent name: two agents can declare the same relative dir under
 * different homes, and the path alone would not tell them apart in the dialog.
 */
function targetPaths(picks, key, withLabel = false) {
  const out = [];
  for (const tg of picks || []) {
    const agent = (S.boot?.agents || []).find((x) => x.id === tg.agentId);
    if (!agent) continue;
    const rels = tg.scope === 'project' ? (agent.projects?.[key] || []) : (agent.globals?.[key] || []);
    const project = tg.scope === 'project'
      ? (S.data?.projects || []).find((p) => p.id === tg.projectId)
      : null;
    const base = project ? project.dir : (agent.home || '');
    const label = [zhText(agent.label) || agent.id, project ? project.name : ''].filter(Boolean).join(' · ');
    for (const rel of rels) {
      const dir = base ? base.replace(/[\\/]+$/, '') + '\\' + rel.replace(/^[\\/]+/, '') : rel;
      out.push(withLabel ? `${label} — ${dir}` : dir);
    }
  }
  return out;
}

/**
 * Every mutation of a real agent file goes through this: one dialog, listing
 * what is about to change, before a byte is written. Settings are excluded —
 * they live in the app's own store, not in an agent directory.
 *
 * Built on confirmDialog rather than a second modal implementation: this shell
 * grants no dialog-plugin ACL, so `window.confirm` rejects instead of asking,
 * and two modal paths would drift apart on focus handling.
 *
 * Covering dialog: the shell has one #modal, so when this check is layered over
 * an editor its nodes are moved aside. They are put back when the answer is
 * "no" — and a write that then fails puts them back through reopenCovered(),
 * because the caller's failure branch only toasts and the user's typed form
 * must not disappear with the error.
 */
let coveredSnap = null;

/** Put the dialog the last check covered back on screen (after a failed write). */
function reopenCovered() {
  const snap = coveredSnap;
  coveredSnap = null;
  restoreModal(snap);
}

async function confirmWrite({ title, action, targets, detail, danger = false }) {
  const list = (Array.isArray(targets) ? targets : [targets]).filter(Boolean).map(String);
  const MAX = 10;
  // A path list, not a sentence: joined paths wrap mid-segment and stop being
  // readable exactly where the user needs to compare them.
  const extra = el('div', {},
    list.length
      ? el('ul', { class: 'write-targets mono fs-11' },
          ...list.slice(0, MAX).map((p) => el('li', {}, p)),
          list.length > MAX ? el('li', { class: 'muted' }, t('confirm.moreTargets', { n: list.length - MAX })) : null)
      : null,
    el('div', { class: 'alert info mt-12' }, t('confirm.writeBackupNote')));
  // Null when nothing was covered (a drawer or a table row triggered this), so
  // every caller gets the same behaviour without a flag of its own.
  const snap = snapshotModal();
  coveredSnap = snap;
  const ok = await confirmDialog({
    title: title || t('confirm.writeTitle'),
    message: [detail, t('confirm.writeBody')].filter(Boolean).join('\n\n'),
    confirmLabel: action || t('common.confirm'),
    danger,
    extra
  });
  // Restored eagerly on cancel: the editor has to look untouched the moment the
  // check closes. On success it stays hidden, so the form cannot blink up behind
  // the dialog during the IPC round-trip; closeModal() dismisses it as before.
  if (!ok) { reopenCovered(); return false; }
  return true;
}

/* ------------------------------------------------------------------ */
/* drawer                                                              */
/* ------------------------------------------------------------------ */

/**
 * Can the closed drawer rely on the native `inert` attribute?
 *
 * `inert` shipped in Chromium 102; an older WebView2 (or an embedder that
 * never enabled it) ignores the property silently, and the closed drawer
 * keeps its ✕ in the tab order — a keyboard user tabs into an off-screen
 * panel. Feature-detected once: the prototype does not change at runtime.
 */
const INERT_NATIVE = typeof HTMLElement !== 'undefined' && 'inert' in HTMLElement.prototype;

/**
 * Which mechanism emulates inertness *right now*.
 *
 * The ak.forceInertFallback localStorage stamp (value '1') is a FORENSICS-ONLY
 * switch: it lets a probe drive the fallback branch on a WebView2 that does
 * support `inert` (a CDP session cannot downgrade the engine). Normal users
 * never set it; it is read on every open/close so a probe can toggle it
 * without a reload.
 */
function inertMode() {
  let forced = false;
  try { forced = localStorage.getItem('ak.forceInertFallback') === '1'; } catch (_e) { /* storage disabled */ }
  return (!INERT_NATIVE || forced) ? 'fallback' : 'native';
}

/** tabindex values the fallback overwrote, restored on the next open. */
let drawerTabindexSaved = null;

/** Everything that would sit in the tab order inside the drawer. */
const DRAWER_FOCUSABLE =
  'a[href], area[href], button, input, select, textarea, iframe, object, embed, [tabindex], [contenteditable="true"]';

/**
 * Make the drawer inert (closed) or interactive, either natively or by
 * emulation. One place, because open/close and the boot-time state all have
 * to agree — a half-applied mode is exactly the "✕ is still tabbable" bug.
 *
 * `data-ak-inert-mode` is the probe hook: it states which branch ran, so an
 * external assertion (CDP) does not have to infer it from behaviour.
 */
function setDrawerInert(inert) {
  const d = $('#drawer');
  if (!d) return;
  const mode = inertMode();
  d.setAttribute('data-ak-inert-mode', mode);
  if (mode === 'native') {
    d.inert = !!inert;
    d.setAttribute('aria-hidden', inert ? 'true' : 'false');
    return;
  }
  // Fallback: `inert` may be missing or ignored, so it cannot be the mechanism.
  // Clear it anyway — the markup ships `inert` on the drawer, and an engine
  // that *does* support it would leave the fallback branch untestable.
  if ('inert' in d) d.inert = false;
  d.setAttribute('aria-hidden', inert ? 'true' : 'false');
  if (inert) {
    // Idempotent: the boot stamp, a failed-then-retried boot and closeDrawer()
    // can all ask for "closed". Re-scanning after the first pass would record
    // tabindex="-1" as the "original" value and lose the real one.
    if (drawerTabindexSaved) return;
    // aria-hidden alone is not enough: it hides the subtree from screen
    // readers but leaves the focus stops in place. Remember each element's
    // original tabindex (usually absent) so opening restores the markup.
    const saved = new Map();
    for (const n of $$(DRAWER_FOCUSABLE, d)) saved.set(n, n.getAttribute('tabindex'));
    drawerTabindexSaved = saved;
    for (const n of saved.keys()) n.setAttribute('tabindex', '-1');
    return;
  }
  if (drawerTabindexSaved) {
    for (const [n, prev] of drawerTabindexSaved) {
      if (prev === null) n.removeAttribute('tabindex');
      else n.setAttribute('tabindex', prev);
    }
    drawerTabindexSaved = null;
  }
}

function openDrawer(title, node) {
  drawerReturnFocus = document.activeElement;
  $('#drawer-title').textContent = title;
  const b = $('#drawer-body');
  b.innerHTML = '';
  b.appendChild(node);
  b.scrollTop = 0;
  const d = $('#drawer');
  // Restore tabbability first: focusInto() picks the first *focusable*
  // descendant, and the fallback left them all at tabindex="-1".
  setDrawerInert(false);
  d.classList.add('on');
  $('#scrim').classList.add('on');
  focusInto(d);
}

/**
 * The one and only exit for the drawer.
 *
 * X, the scrim and Escape all come through here, so the unsaved-edit guard
 * cannot be bypassed by one of the three. The confirm runs BEFORE the drawer
 * is hidden: refusing it leaves the editor exactly where the user left it.
 */
async function closeDrawer() {
  const d = $('#drawer');
  if (!d.classList.contains('on')) return;
  if (S.dirty) {
    // The guard must be this app's own dialog. `window.confirm()` is routed to
    // the Tauri dialog plugin, which this shell does not grant
    // (`Command plugin:dialog|message not allowed by ACL`): the call rejected
    // and the drawer closed anyway, so the user's edits were discarded with no
    // prompt at all. One async path keeps ✕ / scrim / Escape identical.
    // A dialog already on screen owns the next answer — opening a second one
    // would stack scrims and strand the first promise.
    if ($('#modal-scrim').classList.contains('on')) return;
    const ok = await confirmDialog({
      title: t('drawer.unsavedTitle'),
      message: t('drawer.discardConfirm'),
      confirmLabel: t('verb.discard'),
      danger: true
    });
    if (!ok) return;
  }
  S.dirty = false;
  S.selected = null;
  d.classList.remove('on');
  // inert (not just aria-hidden): the drawer is off-screen, not display:none,
  // so its ✕ would otherwise stay in the tab order — and aria-hidden around a
  // focusable element is itself an accessibility error. setDrawerInert() keeps
  // that promise on engines without `inert`, where it adds tabindex="-1" to
  // every focusable descendant and restores it on the next open.
  setDrawerInert(true);
  $('#scrim').classList.remove('on');
  render();
  const back = drawerReturnFocus;
  drawerReturnFocus = null;
  returnFocus(back);
}

/* ------------------------------------------------------------------ */
/* bootstrap                                                           */
/* ------------------------------------------------------------------ */

async function boot() {
  restoreUiState();
  // The sidebar collapse state is a pure view preference: restore it before
  // the first paint so the rail does not jump from 248px to 60px on boot.
  if (localStorage.getItem('ak.side') === 'collapsed') $('#app').classList.add('collapsed');
  S.boot = await window.ak.bootstrap();
  // Settings drive language and theme, so load them before the first paint
  // decision — theme-init.js already handled the attribute, we just sync it.
  try { S.settings = (await window.ak.getSettings()) || {}; } catch (e) { S.settings = {}; }
  if (S.settings.theme) applyTheme(S.settings.theme);
  if (S.settings.lang) i18n.setLang(S.settings.lang);
  if (!S.langHooked) { i18n.onChange(applyLanguage); S.langHooked = true; }

  $('#store-root').textContent = 'store: ' + S.boot.storeRoot;
  // Paint the shell (nav labels, wired buttons, localized statics) BEFORE the
  // first scan: the scan takes seconds on real machines and must not leave the
  // whole main area blank while it runs.
  //
  // boot() is also the retry target of the startup error panel, so every wire*
  // below runs at most once: a second pass would stack a second scan listener,
  // a second update dialog and a second language listener.
  if (!S.wired) { wireGlobal(); S.wired = true; }
  applyLanguage(i18n.lang);
  // The closed drawer ships off-screen but focusable; the boot pass applies
  // inert before the user could ever Tab into it (openDrawer/closeDrawer keep
  // it in sync afterwards).
  setDrawerInert(true);
  await rescan(true);

  // --- keep the settings promises -----------------------------------
  if (!S.updateWired) { wireUpdateBanner(); S.updateWired = true; }
  await initUpdates();
  // scan-complete notifications only fire when the window is hidden, so the
  // user learns about attention-needing assets without watching the app.
  if (S.settings.notifications !== false && !S.notifyWired) {
    S.notifyWired = true;
    document.addEventListener('visibilitychange', () => {
      if (!document.hidden && S.lastIssueCount === null) S.lastIssueCount = S.data?.summary?.issues ?? 0;
    });
  }
}

/* ------------------------------------------------------------------ */
/* software update                                                     */
/* ------------------------------------------------------------------ */

/**
 * The main process owns the update schedule; the renderer only renders what
 * it is given. That split is what makes an automatic check able to raise the
 * dialog while the user is on another page, and what keeps the "check on
 * start" promise from turning into one GitHub request per page reload.
 */
async function initUpdates() {
  try { S.update = await window.ak.updateStatus(); } catch (e) { S.update = null; }
  // Guarded: initUpdates() also runs on a boot retry, and a second subscription
  // would raise the update dialog twice per event.
  if (typeof window.ak.onUpdateEvent === 'function' && !S.updateEventHooked) {
    S.updateEventHooked = true;
    window.ak.onUpdateEvent((status) => {
      if (!status) return;
      S.update = status;
      syncUpdateBanner(status);
      if (S.view === 'settings') render();
      promptUpdate(status);
    });
  }
  syncUpdateBanner(S.update);
  syncUpdateBadge(S.update);
  promptUpdate(S.update);
  // If the app opened straight onto Settings, the page was painted from the
  // bootstrap placeholder (no `repo`, no `checkedAt`) and would otherwise keep
  // showing "github.com/" until the user navigated away and back.
  if (S.view === 'settings') render();
  // The badge has to survive the window being open for hours: this reads the
  // main-process snapshot only (no network, no check), so polling it is free.
  if (!S.updatePollTimer) {
    S.updatePollTimer = setInterval(() => {
      window.ak.updateStatus().then((status) => {
        S.update = status;
        syncUpdateBadge(status);
        syncUpdateBanner(status);
      }).catch(() => { /* transient IPC failure: keep the last badge */ });
    }, 60000);
  }
}

/** Sidebar dot: an update is waiting and the user has not dismissed it. */
function syncUpdateBadge(status) {
  const badge = $('#nav-update-badge');
  if (!badge) return;
  const waiting = !!(status && status.hasUpdate === true && !status.skipped);
  badge.classList.toggle('hidden', !waiting);
  if (waiting) syncUpdateBanner(status);
}

/**
 * Version whose banner the user dismissed.
 *
 * Remembering it in-memory only was not enough: the 60 s status poll calls
 * `syncUpdateBanner()` again and re-showed the banner the user had just closed.
 * A *newer* version no longer matches, so it still gets announced.
 */
let updateDismissed = (function () {
  try { return localStorage.getItem('ak.updateDismissed') || null; } catch (_e) { return null; }
})();

/** One-time wiring for the top banner (rebinding on every boot would stack). */
function wireUpdateBanner() {
  const open = $('#update-open');
  const dismiss = $('#update-dismiss');
  if (open) open.addEventListener('click', () => go('settings'));
  if (dismiss) dismiss.addEventListener('click', () => {
    const v = S.update && S.update.latest ? String(S.update.latest) : '';
    updateDismissed = v || null;
    try { if (v) localStorage.setItem('ak.updateDismissed', v); } catch (_e) { /* storage disabled */ }
    $('#update-banner').classList.add('hidden');
  });
}

function syncUpdateBanner(status) {
  const banner = $('#update-banner');
  if (!banner) return;
  if (!status || status.hasUpdate !== true || status.skipped
    || (status.latest && String(status.latest) === updateDismissed)) {
    banner.classList.add('hidden');
    return;
  }
  $('#update-text').textContent = t('update.text', { v: status.latest, cur: status.current });
  banner.classList.remove('hidden');
}

/**
 * Show the "update available" dialog at most once per version per session.
 *
 * "Later" deliberately remembers nothing — the scheduler will find the same
 * release again, so a session-level guard is what stops it from re-prompting
 * every interval. "Skip this version" is persisted in main-process state by
 * version number, so the next release prompts normally.
 */
function promptUpdate(status) {
  if (!status || status.hasUpdate !== true || status.skipped) return;
  if (S.view === 'settings') return;            // the panel is already on screen
  const latest = String(status.latest || '');
  if (!latest || latest === S.updatePrompted) return;
  S.updatePrompted = latest;
  showUpdateDialog(status);
}

function showUpdateDialog(status) {
  const body = el('div', {},
    el('div', { class: 'muted fs-12' },
      t('update.dialogBody', { v: status.latest, cur: status.current })),
    el('pre', { class: 'update-notes' }, status.notes || t('update.noNotes')));
  const skip = el('button', {
    class: 'btn',
    onclick: async () => {
      const r = await window.ak.updateSkip(status.latest).catch(() => null);
      if (r) { S.update = r; syncUpdateBanner(r); }
      closeModal();
    }
  }, t('update.skip'));
  const later = el('button', { class: 'btn', onclick: closeModal }, t('update.later'));
  const goBtn = el('button', {
    class: 'btn primary',
    onclick: () => { closeModal(); go('settings'); void startUpdateDownload(status); }
  }, t('update.go'));
  openModal(t('update.dialogTitle'), body, [skip, later, goBtn]);
}

/** Start the installer download for the cached release and poll its progress. */
async function startUpdateDownload(status) {
  const asset = status && status.asset;
  if (!asset || !asset.url) { toast(t('update.badAsset'), 'err'); return; }
  const r = await window.ak.updateDownload(asset.url, asset.name).catch((e) => ({ ok: false, error: e.message }));
  if (!r || !r.ok) {
    toast(t('update.downloadFailed', { error: (r && r.error) || 'unknown' }), 'err');
    return;
  }
  if (S.update) S.update.task = r.task;
  if (S.view === 'settings') render();
  pollUpdateTask();
}

/**
 * Progress polling. The task object lives in the main process (downloads keep
 * running when the user navigates), so this poll is the only way the UI sees
 * bytes arrive — the panel keeps the last N lines.
 */
function pollUpdateTask() {
  if (S.updateTimer) return;
  S.updateTimer = setInterval(async () => {
    let task = null;
    try { task = await window.ak.updateProgress(); } catch (e) { task = null; }
    if (S.update) S.update.task = task;
    if (S.view === 'settings') render();
    if (!task || !task.active) {
      clearInterval(S.updateTimer);
      S.updateTimer = null;
      try { S.update = await window.ak.updateStatus(); } catch (e) { /* keep last */ }
      if (S.view === 'settings') render();
      if (task && task.done && task.path) promptInstall(task);
      else if (task && task.error) toast(t('update.downloadFailed', { error: task.error }), 'err');
      else if (task && task.canceled) toast(t('update.downloadCanceled'), '');
    }
  }, 900);
}

/**
 * Installing closes the app, so it always takes an explicit confirmation —
 * the download finishing is not the same thing as the user asking to quit.
 */
function promptInstall(task) {
  const version = (S.update && S.update.latest) || '';
  const body = el('div', { class: 'fs-12' }, t('update.readyBody', { v: version }));
  const later = el('button', { class: 'btn', onclick: closeModal }, t('update.later'));
  const goBtn = el('button', {
    class: 'btn primary',
    onclick: async () => {
      goBtn.disabled = true;
      const r = await window.ak.updateInstall(task.path).catch((e) => ({ ok: false, error: e.message }));
      closeModal();
      if (!r || !r.ok) toast(t('update.installFailed', { error: (r && r.error) || 'unknown' }), 'err');
      else toast(t('update.installing'), 'ok');
    }
  }, t('update.installNow'));
  openModal(t('update.readyTitle'), body, [later, goBtn]);
}

/**
 * Desktop notification, gated by the notifications setting AND window
 * visibility — a toast inside the visible app is enough when the user is
 * already looking at it.
 */
function maybeNotify(key, vars) {
  if (S.settings.notifications === false) return;
  if (!document.hidden) return;
  try {
    if (typeof Notification === 'undefined') return;
    if (Notification.permission === 'granted') {
      new Notification(t(key, vars));
    } else if (Notification.permission !== 'denied') {
      Notification.requestPermission().then((p) => { if (p === 'granted') new Notification(t(key, vars)); });
    }
  } catch (e) { /* notifications are best-effort */ }
}

async function rescan(silent = false) {
  // Scan-in-flight feedback: a slim progress strip under the banner plus a
  // disabled scan button, so a slow scan never reads as "clicked, nothing
  // happened". Silent rescans (side effects of other actions) skip the strip.
  const scanbar = $('#scanbar');
  const scanBtn = $('#btn-scan');
  // First scan of a session is shown even when "silent": the user is staring
  // at a skeleton and needs to know why data has not landed yet.
  const firstScan = !S.data;
  if (!silent || firstScan) {
    if (scanbar) { $('#scanbar-text').textContent = t('scan.scanning'); scanbar.classList.remove('hidden'); }
    if (scanBtn) { scanBtn.classList.add('loading'); scanBtn.disabled = true; }
  }
  try {
    // Writes land in the real agent directories, so the scan is the only state
    // this view needs — there is no staged layer left to fetch.
    const d = await window.ak.scan();
    S.data = d;
    // Drop checked ids that no longer exist (deleted elsewhere / rescan changed
    // ids) — otherwise the batch bar counts ghosts.
    if (S.checked.size) {
      const alive = new Set((d.assets || []).map((a) => a.id));
      for (const id of [...S.checked]) if (!alive.has(id)) S.checked.delete(id);
    }
    $('#nav-asset-count').textContent = d.summary.total;
    // One number for all three surfaces. `summary.issues` counts error-level
    // format problems only, so a warning-level finding left the sidebar badge on
    // "0" and the dashboard card on "0" while the Health page listed
    // "1 asset needs attention". issueAssets() is the Health page's predicate.
    const issueCount = issueAssets(d).length;
    const issueEl = $('#nav-issue-count');
    issueEl.textContent = issueCount;
    issueEl.classList.toggle('hidden', issueCount === 0);
    syncNavNames();
    render();
    if (!silent) {
      const errs = issueCount;
      toast(errs ? t('toast.scanIssues', { n: errs }) : t('toast.scanDone', { n: d.summary.total }),
        errs ? 'warn' : 'ok');
    }
  } catch (e) {
    // A failed scan must not hide behind a toast: the table would keep the
    // previous data and a first scan would leave a blank main area with no way
    // to retry. Paint the retryable panel instead; its retry re-runs the scan.
    reportUiError('error', e);
    const root = $('#view-root');
    if (root) {
      root.innerHTML = '';
      root.appendChild(errorPanel(t('err.scanFailed', { msg: zhError(e) }), () => rescan()));
    }
  } finally {
    if (scanbar) scanbar.classList.add('hidden');
    if (scanBtn) { scanBtn.classList.remove('loading'); scanBtn.disabled = false; }
  }
}

function wireGlobal() {
  $('#btn-scan').addEventListener('click', () => rescan());
  $('#btn-import').addEventListener('click', doImportProject);
  $('#btn-new-skill').addEventListener('click', () => openSkillEditor(null));
  $('#drawer-close').addEventListener('click', closeDrawer);
  // Scrim click is the same exit as X: one closeDrawer, one confirm.
  $('#scrim').addEventListener('click', closeDrawer);
  $('#modal-close').addEventListener('click', closeModal);
  $('#modal-scrim').addEventListener('click', (e) => { if (e.target.id === 'modal-scrim') closeModal(); });

  const sideToggle = $('#side-toggle');
  if (sideToggle) sideToggle.addEventListener('click', () => {
    const collapsed = $('#app').classList.toggle('collapsed');
    localStorage.setItem('ak.side', collapsed ? 'collapsed' : 'expanded');
    syncSideToggle();
  });

  $$('.nav-item').forEach((b) => b.addEventListener('click', () => go(b.dataset.view)));

  // Close the context menu on any click outside it.
  document.addEventListener('click', (e) => {
    const m = $('#ctxmenu');
    if (m && m.classList.contains('on') && !m.contains(e.target)) hideCtxMenu();
  });

  let debounce;
  $('#search').addEventListener('input', (e) => {
    clearTimeout(debounce);
    const v = e.target.value;
    debounce = setTimeout(() => {
      S.filter = v.trim().toLowerCase();
      // A new query redefines the visible set exactly like a filter select, so
      // it has to reset the page and the checked set: otherwise page 3 of the
      // old result stays selected and the batch bar offers to delete rows the
      // query hides.
      S.page = 1;
      S.checked.clear();
      render();
    }, 120);
  });

  document.addEventListener('keydown', (e) => {
    // Dialog first: while one is open it owns Tab and Escape. Otherwise focus
    // stays behind the scrim and Escape drops unsaved edits.
    const modalOpen = $('#modal-scrim').classList.contains('on');
    const drawerOpen = $('#drawer').classList.contains('on');
    if (e.key === 'Tab') {
      if (modalOpen) { trapTab(e, $('#modal')); return; }
      if (drawerOpen) { trapTab(e, $('#drawer')); return; }
    }
    if (e.key === '/' && document.activeElement.tagName !== 'INPUT' && document.activeElement.tagName !== 'TEXTAREA') {
      e.preventDefault();
      $('#search').focus();
    }
    if (e.key === 'Escape') {
      hideCtxMenu();
      if (modalOpen) closeModal();
      else if (drawerOpen) closeDrawer();
    }
    if ((e.ctrlKey || e.metaKey) && e.key === 's' && S.selected) { e.preventDefault(); saveSelected(); }
  });
}

/* ------------------------------------------------------------------ */
/* render dispatch                                                     */
/* ------------------------------------------------------------------ */

const VIEWS = {
  overview: { titleKey: 'nav.overview', subKey: 'dashboard.subtitle', render: viewOverview },
  assets: { titleKey: 'nav.assets', subKey: 'assets.subtitle', render: viewAssets },
  agents: { titleKey: 'nav.agents', subKey: 'agents.subtitle', render: viewAgents },
  projects: { titleKey: 'nav.projects', subKey: 'projects.subtitle', render: viewProjects },
  discover: { titleKey: 'nav.discover', subKey: 'discover.subtitle', render: viewDiscover },
  health: { titleKey: 'nav.health', subKey: 'health.subtitle', render: viewHealth },
  activity: { titleKey: 'nav.activity', subKey: 'activity.subtitle', render: viewActivity },
  settings: { titleKey: 'nav.settings', subKey: 'settings.subtitle', render: viewSettings }
};

/** The element that should keep focus if this render replaces the DOM. */
function focusKeyNow() {
  const n = document.activeElement;
  if (!n || !n.closest) return null;
  const holder = n.closest('[data-focus-key]');
  return holder ? holder.getAttribute('data-focus-key') : null;
}

/**
 * Put focus back on the equivalent control after a re-render.
 *
 * Checkbox / filter changes re-render the whole view; without this the focus
 * fell to <body> on every click, so a keyboard user restarted from the top of
 * the page after each selection and a screen reader lost its place.
 */
function restoreFocusKey(root, key) {
  if (!key) return;
  const again = root.querySelector(`[data-focus-key="${key}"]`);
  if (again && typeof again.focus === 'function') again.focus({ preventScroll: true });
}

function render() {
  if (!S.data) return;
  const v = VIEWS[S.view];
  $('#view-title').textContent = t(v.titleKey);
  $('#view-sub').textContent = t(v.subKey);
  const root = $('#view-root');
  const keepFocus = focusKeyNow();
  root.innerHTML = '';
  try {
    const out = v.render();
    // Async views (activity) resolve after the fact; show a placeholder and
    // swap in the real node when the data lands.
    if (out && typeof out.then === 'function') {
      root.appendChild(el('div', { class: 'empty' }, t('common.loading')));
      const viewAtCall = S.view;
      out.then((node) => {
        if (S.view !== viewAtCall) return;   // user navigated away
        root.innerHTML = '';
        root.appendChild(node);
        restoreFocusKey(root, keepFocus);
      }).catch((e) => {
        if (S.view !== viewAtCall) return;
        reportUiError('error', e);
        root.innerHTML = '';
        root.appendChild(errorPanel(t('err.renderFailed', { view: S.view, msg: zhError(e) }), () => render()));
      });
      return;
    }
    root.appendChild(out);
    restoreFocusKey(root, keepFocus);
  } catch (e) {
    reportUiError('error', e);
    root.innerHTML = '';
    root.appendChild(errorPanel(t('err.renderFailed', { view: S.view, msg: zhError(e) }), () => render()));
  }
}

/**
 * Sidebar collapse state → the toggle button.
 *
 * The icon-only rail hides `.nav-text`, so each nav item needs an accessible
 * name that survives display:none; aria-label + title carry it. Called from
 * applyLanguage (label language) and from the toggle itself.
 */
function syncSideToggle() {
  const btn = $('#side-toggle');
  if (!btn) return;
  const collapsed = $('#app').classList.contains('collapsed');
  const label = t(collapsed ? 'nav.expandSidebar' : 'nav.collapseSidebar');
  btn.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
  btn.setAttribute('aria-label', label);
  btn.title = label;
  btn.textContent = collapsed ? '»' : '«';
}

/** Re-render everything that shows a translated string (nav, titles, counts). */
/**
 * Sidebar labels, including the badge counts.
 *
 * `aria-label` replaces descendant text, so the badges ("969", the issue count,
 * the update dot) were invisible to assistive tech even though they are the only
 * place those numbers live. Keep the visible text in sync for the same reason.
 */
function syncNavNames() {
  for (const n of $$('.nav-item')) {
    const view = n.dataset.view;
    const badge = n.querySelector('.nav-badge');
    const extra = badge && !badge.classList.contains('hidden') ? badge.textContent.trim() : '';
    const name = t(`nav.${view}`) + (extra ? ` · ${extra}` : '');
    n.setAttribute('aria-label', name);
    n.title = name;
    const label = n.querySelector('.nav-text');
    if (label) label.textContent = t(`nav.${view}`);
  }
}

function applyLanguage(lang) {
  document.documentElement.lang = lang === 'zh' ? 'zh-CN' : 'en';
  syncNavNames();
  // Static markup in index.html opts in with data-i18n / data-i18n-placeholder
  // / data-i18n-title / data-i18n-label (ARIA names, which have no text node).
  for (const n of $$('[data-i18n]')) n.textContent = t(n.dataset.i18n);
  for (const n of $$('[data-i18n-placeholder]')) n.placeholder = t(n.dataset.i18nPlaceholder);
  for (const n of $$('[data-i18n-title]')) n.title = t(n.dataset.i18nTitle);
  for (const n of $$('[data-i18n-label]')) n.setAttribute('aria-label', t(n.dataset.i18nLabel));
  const group = $('#nav-group-operate');
  if (group) group.textContent = t('nav.groupOperate');
  const overview = $('#nav-group-overview');
  if (overview) overview.textContent = t('nav.groupOverview');
  // #btn-scan's title comes from data-i18n-title="nav.rescanTitle" in the markup;
  // do not overwrite it here with a subtitle sentence (it reads wrong on a button).
  syncSideToggle();
  render();
}

function filteredAssets() {
  const d = S.data;
  if (!d) return [];
  let list = d.assets;
  if (S.kindFilter !== 'all') list = list.filter((a) => a.kind === S.kindFilter);
  if (S.agentFilter !== 'all') list = list.filter((a) => a.agentId === S.agentFilter);
  if (S.scopeFilter !== 'all') list = list.filter((a) => a.scope === S.scopeFilter);
  if (S.originFilter === 'curated') list = list.filter((a) => a.kind !== 'plugin' || a.origin === 'managed');
  else if (S.originFilter === 'user') list = list.filter((a) => (a.origin || 'user') === 'user');
  else if (S.originFilter === 'cached') list = list.filter((a) => (a.origin || '') === 'cached' || (a.origin || '') === 'marketplace');
  if (S.onlyShadowed) list = list.filter((a) => a.effective && a.effective.live === false);
  if (S.filter) {
    const q = S.filter;
    list = list.filter((a) =>
      (a.name || '').toLowerCase().includes(q) ||
      (a.description || '').toLowerCase().includes(q) ||
      (a.agentLabel || '').toLowerCase().includes(q) ||
      (a.projectName || '').toLowerCase().includes(q) ||
      (a.meta.relLabel || '').toLowerCase().includes(q) ||
      (a.tags || []).join(' ').toLowerCase().includes(q));
  }
  if (S.sortKey) {
    const cmp = {
      tokens: (a, b) => (b.size || 0) - (a.size || 0),
      size: (a, b) => (b.size || 0) - (a.size || 0),
      name: (a, b) => a.name.localeCompare(b.name),
      agent: (a, b) => a.agentLabel.localeCompare(b.agentLabel),
      scope: (a, b) => a.scope.localeCompare(b.scope)
    }[S.sortKey];
    // An explicit sort choice is the whole point of the control, so it must win:
    // no unconditional re-sort at the end of this function, which would discard
    // every choice but `default`.
    if (cmp) {
      return list.slice().sort((a, b) => cmp(a, b) || (a.kind + a.name).localeCompare(b.kind + b.name));
    }
  }
  return list.slice().sort((a, b) => (a.kind + a.name).localeCompare(b.kind + b.name));
}

function issuesOf(a) {
  return (a.issues || []).filter((i) => i.level === 'error').length;
}

/**
 * Assets the Health page will list: any format/validation issue or security
 * finding, excluding host caches (their defects are the vendor's, not the
 * user's). The sidebar badge, the dashboard "needs attention" card and the
 * Health page all read this one predicate.
 */
function issueAssets(d) {
  return (d.assets || []).filter((a) =>
    ((a.issues || []).length || (a.security || []).length) && (a.origin || 'user') !== 'cached');
}

/* Raw engine enums (`skill`, `mcp`, `global`) are data, not UI text: they must
   be mapped through the dictionary before they reach a chip or a table cell,
   otherwise a zh window shows "记忆 3 · global". */

const KIND_LABEL = {
  skill: 'assets.kindSkill',
  mcp: 'assets.kindMcp',
  plugin: 'assets.kindPlugin',
  rule: 'assets.kindRule',
  memory: 'assets.kindMemory'
};

/** "skill" → 技能 / Skills. Unknown kinds (new adapter) fall back to the enum. */
function kindName(kind) {
  return KIND_LABEL[kind] ? t(KIND_LABEL[kind]) : kind;
}

/** "global" / "project" → 全局 / 项目级. */
function scopeName(scope) {
  return scope === 'project' ? t('assets.scopeProject') : t('assets.scopeGlobal');
}

/* ------------------------------------------------------------------ */
/* view: overview                                                      */
/* ------------------------------------------------------------------ */

/**
 * Asset counts split by kind, for the dashboard's per-agent table.
 *
 * One shared KIND_COLS definition, so the header order and the cell order can
 * never drift apart — a per-agent row whose columns were built from a
 * different list than its header is exactly the "the numbers do not match the
 * titles" defect.
 */
const KIND_COLS = ['skill', 'mcp', 'plugin', 'rule', 'memory'];

function viewOverview() {
  const d = S.data;
  const s = d.summary;
  // Same predicate as the sidebar badge and the Health page (issueAssets), so
  // the three cannot disagree about how many assets need attention.
  const issueCount = issueAssets(d).length;
  // .view-fill: the view root is a scrolling box of unknown height, so the two
  // panels below have to claim the remaining space rather than sit as short
  // cards at the top of an empty page.
  const wrap = el('div', { class: 'view-fill' });

  const cached = s.byOrigin.cached || 0;
  const stats = el('div', { class: 'grid grid-4' },
    statCard('k-skill', t('dashboard.skills'), s.by.skill,
      t('dashboard.projectScoped', { n: d.assets.filter((a) => a.kind === 'skill' && a.scope === 'project').length }),
      () => drill('skill')),
    statCard('k-mcp', t('dashboard.mcp'), s.by.mcp,
      t('dashboard.enabledCount', { n: d.assets.filter((a) => a.kind === 'mcp' && a.enabled).length }),
      () => drill('mcp')),
    statCard('k-plugin', t('dashboard.plugins'), s.by.plugin,
      cached ? t('dashboard.cachedCount', { n: cached }) : t('dashboard.noCached'),
      () => drill('plugin', 'curated')),
    statCard('k-rule', t('dashboard.rules'), s.by.rule,
      t('dashboard.alwaysLoaded', { n: bytes(d.assets.filter((a) => a.kind === 'rule').reduce((n, a) => n + a.size, 0)) }),
      () => drill('rule')),
    statCard('k-memory', t('dashboard.memory'), s.by.memory,
      t('dashboard.stored', { n: bytes(d.assets.filter((a) => a.kind === 'memory').reduce((n, a) => n + a.size, 0)) }),
      () => drill('memory')),
    statCard('', t('dashboard.issues'), issueCount,
      issueCount ? t('dashboard.openHealth') : t('dashboard.clean'),
      () => go('health')),
    statCard('', t('dashboard.shadowed'), s.shadowed, t('dashboard.hiddenByProject'),
      () => { S.onlyShadowed = true; go('assets'); }),
    statCard('', t('dashboard.contextCost'), count(s.estTokens), t('dashboard.tokens'),
      () => { S.sortKey = 'tokens'; go('assets'); })
  );
  wrap.appendChild(stats);

  // Each panel owns a full row. Side by side they had to share ~560px of card
  // width at the common 1440px viewport, and the agent table's eight columns
  // then clipped its own Agent column to 68px — a column narrow enough to
  // ellipsize the very label it exists to identify. Stacked, each table gets the
  // full content width, and the two panels split the leftover height.
  const row = el('div', { class: 'row-fill col mt' });

  /* per agent — full width, one column per kind.
     The composition panel that used to sit here is gone: its stacked bar
     duplicated the eight stat cards directly above it (same five kinds, same
     numbers), and the half-column card it shared a row with left both cards
     too short to read. */
  const perAgent = new Map();
  for (const a of d.assets) {
    if (!perAgent.has(a.agentId)) {
      perAgent.set(a.agentId, { label: zhText(a.agentLabel), n: 0, bytes: 0, byKind: {} });
    }
    const e = perAgent.get(a.agentId);
    e.n++; e.bytes += a.size || 0;
    e.byKind[a.kind] = (e.byKind[a.kind] || 0) + 1;
  }
  const agentRows = [...perAgent.entries()]
    .sort((a, b) => b[1].n - a[1].n)
    .map(([id, v]) =>
      el('tr', {
        tabindex: '0',
        'data-focus-key': `agent-${id}`,
        onclick: () => drillAgent(id),
        onkeydown: rowKey(() => drillAgent(id))
      },
        el('td', { class: 'col-agent' },
          el('div', { class: 't-name', title: v.label }, v.label),
          el('div', { class: 't-mono', title: id }, id)),
        ...KIND_COLS.map((k) =>
          el('td', { class: `num col-num${v.byKind[k] ? '' : ' muted'}` }, v.byKind[k] ? String(v.byKind[k]) : '·')),
        el('td', { class: 'num col-num strong' }, String(v.n)),
        el('td', { class: 'num col-num muted' }, bytes(v.bytes))
      ));
  const agentCard = el('div', { class: 'card card-fill' },
    el('div', { class: 'card-head' },
      el('h2', {}, t('card.byAgent')),
      el('span', { class: 'sub spacer' }, t('card.clickToFilter'))),
    el('div', { class: 'card-scroll' },
      el('table', { class: 'tbl tbl-agents' },
        el('thead', {},
          el('tr', {},
            el('th', { class: 'col-agent' }, t('th.agent')),
            ...KIND_COLS.map((k) => el('th', { class: 'num col-num' }, kindName(k))),
            el('th', { class: 'num col-num' }, t('th.assets')),
            el('th', { class: 'num col-num' }, t('drawer.size')))),
        el('tbody', {}, agentRows.length ? agentRows
          : el('tr', {}, el('td', { colspan: '8', class: 'empty' }, t('empty.noMatch')))))));
  row.appendChild(el('div', { class: 'flex-1-min' }, agentCard));

  /* recent activity */
  const recent = d.assets
    .filter((a) => a.mtime)
    .sort((a, b) => new Date(b.mtime) - new Date(a.mtime))
    .slice(0, 8);
  const recRows = recent.map((a) =>
    el('tr', { tabindex: '0', onclick: () => openAsset(a.id), onkeydown: rowKey(() => openAsset(a.id)) },
      el('td', { class: 'col-kind' }, el('span', { class: `chip ${a.kind}` }, kindName(a.kind))),
      el('td', { class: 'col-name' }, el('div', { class: 't-name', title: a.name }, a.name)),
      el('td', { class: 'col-agent muted' }, zhText(a.agentLabel)),
      el('td', { class: 'col-scope muted' }, scopeName(a.scope)),
      el('td', { class: 'col-time muted' }, ago(a.mtime))
    ));
  row.appendChild(el('div', { class: 'flex-1-min' },
    el('div', { class: 'card card-fill' },
      el('div', { class: 'card-head' },
        el('h2', {}, t('card.recent')),
        el('span', { class: 'sub spacer' }, t('card.newestFirst'))),
      el('div', { class: 'card-scroll' },
        el('table', { class: 'tbl tbl-recent' },
          el('thead', {},
            el('tr', {},
              el('th', { class: 'col-kind' }, t('th.kind')),
              el('th', { class: 'col-name' }, t('th.name')),
              el('th', { class: 'col-agent' }, t('th.agent')),
              el('th', { class: 'col-scope' }, t('th.scope')),
              el('th', { class: 'col-time' }, t('drawer.modified')))),
          el('tbody', {}, recRows.length ? recRows
            : el('tr', {}, el('td', { colspan: '5', class: 'empty' }, t('empty.noMatch')))))))));
  wrap.appendChild(row);

  return wrap;
}

/**
 * Dashboard agent row → the asset list filtered to that agent.
 *
 * Every other dimension is reset before navigating. Leaving a dimension
 * behind is how a click lands on an empty list: the per-agent table sits under
 * eight stat cards, several of which just wrote `kindFilter` and
 * `originFilter`, and those are remembered across navigations (persistUiState)
 * and across restarts (restoreUiState). A row whose agent owns a single asset
 * is then filtered by that asset's kind *and* by an origin filter that hides
 * it, and the Assets page correctly reports "no match" for a filter the user
 * never set. Page must be reset for the same reason: landing on page 4 of a
 * 1-row result set is an empty table.
 */
function drillAgent(agentId) {
  S.kindFilter = 'all';
  S.agentFilter = agentId;
  S.scopeFilter = 'all';
  S.originFilter = 'all';
  S.onlyShadowed = false;
  S.sortKey = null;
  S.filter = '';
  S.page = 1;
  S.checked.clear();
  persistUiState();
  go('assets');
}

/**
 * Dashboard stat tile. Pass `onClick` to make it a drill-down target —
 * a dead card that only reports a number forces the user to go hunting for
 * where that number lives.
 */
/**
 * Enter/Space handler for rows that are only clickable (agent, recent, matrix,
 * project rows). The asset table is the exception: it moves a
 * cursor with ↑/↓ so 50 rows do not become 50 tab stops.
 */
const rowKey = (fn) => (e) => {
  if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); fn(); }
};

function statCard(cls, label, value, foot, onClick) {
  const node = el('div', {
    class: `stat ${cls}${onClick ? ' clickable' : ''}`,
    title: onClick ? t('dashboard.show', { x: label }) : '',
    tabindex: onClick ? '0' : null,
    role: onClick ? 'button' : null
  },
    el('div', { class: 'stat-label' }, label),
    el('div', { class: 'stat-value' }, String(value)),
    el('div', { class: 'stat-foot' }, foot || ''),
    onClick ? el('span', { class: 'stat-go' }, '→') : null
  );
  if (onClick) {
    node.addEventListener('click', onClick);
    node.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onClick(); }
    });
  }
  return node;
}

/* i18n + theme (defined in i18n.js / theme-init.js) */
const i18n = window.__akI18n;
const t = (k, vars) => i18n.t(k, vars);

/** Switch view and keep the sidebar in sync. Single place for navigation. */
function go(view) {
  if (!VIEWS[view]) view = 'overview';
  S.view = view;
  $$('.nav-item').forEach((x) => x.classList.toggle('active', x.dataset.view === view));
  syncSearchBox();
  render();
}

/**
 * `#search` shows `S.filter`, but drill-in paths (stat cards, project rows) set
 * the filter without typing. The box must follow the filter, or the UI claims
 * a query the list is not applying.
 */
function syncSearchBox() {
  const s = $('#search');
  if (s && document.activeElement !== s) s.value = S.filter || '';
}

/** Persist the asset-list UI state across restarts. Filters and sort survive;
 * the selected row does not (it may not exist after a rescan). */
function persistUiState() {
  try {
    localStorage.setItem('ak.ui', JSON.stringify({
      kindFilter: S.kindFilter,
      agentFilter: S.agentFilter,
      scopeFilter: S.scopeFilter,
      originFilter: S.originFilter,
      sortKey: S.sortKey,
      onlyShadowed: S.onlyShadowed
    }));
  } catch (e) { /* private mode — filters just don't persist */ }
}

function restoreUiState() {
  try {
    const ui = JSON.parse(localStorage.getItem('ak.ui') || '{}');
    for (const k of ['kindFilter', 'agentFilter', 'scopeFilter', 'originFilter', 'sortKey', 'onlyShadowed']) {
      if (ui[k] !== undefined) S[k] = ui[k];
    }
  } catch (e) { /* corrupt state = defaults */ }
}

/** Jump to Assets with a pre-applied filter. */
function drill(kind, origin) {
  S.kindFilter = kind;
  if (origin) S.originFilter = origin;
  else S.originFilter = kind === 'plugin' ? 'curated' : 'all';
  // The other four dimensions are sticky: they are persisted across restarts
  // and survive every navigation, so a stat card that only sets `kindFilter`
  // can land on a list another filter had already emptied — and the page
  // index can point past the end of the shorter result set, which is an empty
  // table rather than an empty list. Reset them; the card names exactly the
  // one dimension it drills into.
  S.agentFilter = 'all';
  S.scopeFilter = 'all';
  S.onlyShadowed = false;
  S.filter = '';
  S.page = 1;
  S.checked.clear();
  persistUiState();
  go('assets');
}

/* ------------------------------------------------------------------ */
/* view: assets                                                        */
/* ------------------------------------------------------------------ */

function viewAssets() {
  const list = filteredAssets();

  // Pagination: rendering 987+ rows at once wastes frames; 50 a page keeps
  // scroll and paint costs flat as libraries grow. Filter changes reset to
  // page 1 (selectGroup does that).
  const PER_PAGE = 50;
  const pageCount = Math.max(1, Math.ceil(list.length / PER_PAGE));
  if (S.page > pageCount) S.page = 1;
  const pageItems = list.slice((S.page - 1) * PER_PAGE, S.page * PER_PAGE);

  const wrap = el('div', {});

  const bar = el('div', { class: 'card mb-14' },
    el('div', { class: 'card-body flex flex-wrap gap-8' },
      selectGroup('kind', S.kindFilter, [['all', t('assets.allKinds')], ['skill', t('assets.kindSkill')], ['mcp', 'MCP'], ['plugin', t('assets.kindPlugin')], ['rule', t('assets.kindRule')], ['memory', t('assets.kindMemory')]]),
      selectGroup('agent', S.agentFilter, [['all', t('assets.allAgents')], ...agentOptions()]),
      selectGroup('scope', S.scopeFilter, [['all', t('assets.bothScopes')], ['global', t('assets.scopeGlobal')], ['project', t('assets.scopeProject')]]),
      selectGroup('origin', S.originFilter, [['curated', t('assets.curated')], ['user', t('assets.userOnly')], ['all', t('assets.everything')], ['cached', t('assets.cachesOnly')]]),
      selectGroup('sort', S.sortKey || 'default',
        [['default', t('assets.defaultOrder')], ['tokens', t('assets.largestFirst')], ['name', t('assets.nameAsc')], ['agent', t('assets.byAgent')], ['scope', t('assets.byScope')]]),
      S.onlyShadowed
        ? el('button', { class: 'chip warn clickable', title: t('assets.clearFilters'),
            onclick: () => { S.onlyShadowed = false; persistUiState(); render(); } }, t('assets.shadowedOnly') + '  ✕')
        : null,
      el('span', { class: 'muted ml-auto fs-11' }, t('assets.shown', { n: list.length })),
      el('button', { class: 'btn ghost sm', onclick: doImportBundle, title: t('bundle.importTitle') }, t('bundle.import'))
    )
  );
  wrap.appendChild(bar);

  const rows = pageItems.map((a) => {
    const errs = issuesOf(a);
    // A name has to identify the row: `{n} selected` made all 50 checkboxes
    // announce the same thing, so a screen-reader user could not tell which
    // asset a checkbox belongs to.
    const cb = el('input', {
      type: 'checkbox',
      'aria-label': t('batch.rowAria', { name: a.name, agent: zhText(a.agentLabel) }),
      'data-focus-key': `cb-${a.id}`
    });
    cb.checked = S.checked.has(a.id);
    cb.addEventListener('click', (e) => e.stopPropagation());
    cb.addEventListener('change', () => {
      if (cb.checked) S.checked.add(a.id); else S.checked.delete(a.id);
      render();
    });
    return el('tr', { class: S.selected === a.id ? 'sel' : '', onclick: () => openAsset(a.id),
      oncontextmenu: (e) => showCtxMenu(e, a) },
      el('td', { class: 'nowrap' }, cb),
      el('td', {}, el('span', { class: `chip ${a.kind}` }, kindName(a.kind))),
      el('td', {},
        el('div', { class: 't-name' }, a.name),
        el('div', { class: 't-desc', title: a.description || a.meta.relLabel || '' }, a.description || (a.meta.relLabel || ''))
      ),
      el('td', { class: 'nowrap' },
        el('div', {}, zhText(a.agentLabel)),
        el('div', { class: 't-mono' }, a.scope === 'project' ? (a.projectName || '') : scopeName(a.scope))
      ),
      el('td', { class: 'num nowrap' }, bytes(a.size)),
      el('td', { class: 'num nowrap muted' }, a.meta && a.meta.estTokens ? '~' + a.meta.estTokens.toLocaleString() : '—'),
      el('td', { class: 'num nowrap' },
        errs ? el('span', { class: 'chip err', title: t('assets.issue') }, t('assets.issueCount', { n: errs }))
          : (a.effective && a.effective.live === false ? el('span', { class: 'chip warn', title: t('dashboard.hiddenByProject') }, t('assets.shadowedBadge')) : el('span', { class: 'chip ok' }, t('assets.ok')))
      )
    );
  });

  const allChecked = pageItems.length > 0 && pageItems.every((a) => S.checked.has(a.id));
  const headCb = el('input', {
    type: 'checkbox',
    'aria-label': t('batch.allAria', { n: pageItems.length }),
    'data-focus-key': 'cb-all'
  });
  headCb.checked = allChecked;
  headCb.addEventListener('change', () => {
    for (const a of pageItems) { if (headCb.checked) S.checked.add(a.id); else S.checked.delete(a.id); }
    render();
  });

  const tbl = el('table', { class: 'tbl', tabindex: '0', 'aria-label': t('nav.assets') },
    el('thead', {}, el('tr', {}, el('th', { class: 'nowrap' }, headCb), el('th', {}, t('th.kind')), el('th', {}, t('th.name')), el('th', {}, t('th.owner')), el('th', { class: 'num' }, t('drawer.size')), el('th', { class: 'num' }, t('th.tokens')), el('th', { class: 'num' }, t('th.state')))),
    el('tbody', {}, rows.length ? rows : el('tr', {}, el('td', { colspan: '7' }, el('div', { class: 'empty' }, t('empty.noMatch')))))
  );

  // Keyboard navigation: ↑/↓ move a virtual cursor, Enter opens. The active
  // row scrolls into view so long lists stay operable without a mouse.
  let kbIndex = -1;
  tbl.addEventListener('keydown', (e) => {
    if (!rows.length) return;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      kbIndex = e.key === 'ArrowDown' ? Math.min(rows.length - 1, kbIndex + 1) : Math.max(0, kbIndex - 1);
      rows.forEach((r, i) => r.classList.toggle('sel', i === kbIndex));
      rows[kbIndex].scrollIntoView({ block: 'nearest' });
    } else if (e.key === 'Enter' && kbIndex >= 0) {
      openAsset(pageItems[kbIndex].id);
    }
  });

  wrap.appendChild(el('div', { class: 'card' }, tbl));

  // Batch bar appears only when something is checked. Actions are disabled
  // while a dialog is open is unnecessary — modals stack above it anyway.
  if (S.checked.size > 0) {
    const selected = (S.data?.assets || []).filter((a) => S.checked.has(a.id));
    wrap.appendChild(el('div', { class: 'batchbar' },
      el('span', { class: 'count' }, t('batch.selected', { n: S.checked.size })),
      el('button', { class: 'btn danger sm', onclick: () => doBatchDelete(selected) }, t('batch.delete')),
      el('button', { class: 'btn sm', onclick: () => openBatchDistribute(selected) }, t('batch.distribute')),
      el('button', { class: 'btn sm', onclick: () => doExportBundle(selected) }, t('bundle.export')),
      el('button', { class: 'btn ghost sm', onclick: () => { S.checked.clear(); render(); } }, t('batch.clear'))
    ));
  }

  // Pager — only rendered when there is something to page through.
  if (list.length > PER_PAGE || S.page > 1) {
    wrap.appendChild(el('div', { class: 'card pager' },
      el('span', { class: 'info' }, t('pager.info', {
        from: (S.page - 1) * PER_PAGE + 1,
        to: Math.min(S.page * PER_PAGE, list.length),
        total: list.length
      })),
      el('button', {
        class: 'btn sm',
        disabled: S.page <= 1 ? 'true' : null,
        'aria-label': t('pager.prev'),
        'data-focus-key': 'pager-prev',
        onclick: () => { S.page--; render(); }
      }, '←'),
      el('span', { class: 'info' }, `${S.page} / ${pageCount}`),
      el('button', {
        class: 'btn sm',
        disabled: S.page >= pageCount ? 'true' : null,
        'aria-label': t('pager.next'),
        'data-focus-key': 'pager-next',
        onclick: () => { S.page++; render(); }
      }, '→')
    ));
  }
  return wrap;
}

function agentOptions() {
  const ids = new Set((S.data?.assets || []).map((a) => a.agentId));
  return [...ids].map((id) => {
    const l = zhText((S.data.assets.find((a) => a.agentId === id) || {}).agentLabel) || id;
    return [id, l];
  });
}

/** Accessible name for each filter dimension (a select's value is not its name). */
const FILTER_LABEL = {
  kind: 'filter.kind', agent: 'filter.agent', scope: 'filter.scope',
  origin: 'filter.origin', sort: 'filter.sort'
};

function selectGroup(dim, current, options) {
  const s = el('select', {
    class: 'select-inline',
    // The current option text is the only thing a reader would hear otherwise;
    // it says "Skills" but not that this control filters *by kind*.
    'aria-label': t(FILTER_LABEL[dim] || 'filter.kind'),
    'data-focus-key': `filter-${dim}`
  });
  for (const [v, label] of options) {
    const o = el('option', { value: v }, label);
    if (v === current) o.selected = true;
    s.appendChild(o);
  }
  s.addEventListener('change', () => {
    if (dim === 'kind') S.kindFilter = s.value;
    if (dim === 'agent') S.agentFilter = s.value;
    if (dim === 'scope') S.scopeFilter = s.value;
    if (dim === 'origin') S.originFilter = s.value;
    if (dim === 'sort') S.sortKey = s.value === 'default' ? null : s.value;
    S.page = 1;
    // A filter change redefines what "the selected ones" means. Keeping stale
    // ids meant the batch bar offered to delete assets the user could not see.
    S.checked.clear();
    persistUiState();
    render();
  });
  return s;
}

/* ------------------------------------------------------------------ */
/* view: agents                                                        */
/* ------------------------------------------------------------------ */

function viewAgents() {
  const d = S.data;
  const wrap = el('div', {});
  const cards = d.agents.map((a) => {
    const mine = d.assets.filter((x) => x.agentId === a.id);
    const counts = {};
    for (const x of mine) counts[x.kind] = (counts[x.kind] || 0) + 1;
    return el('div', { class: `agent-card ${a.installed ? '' : 'off'}` },
      el('div', { class: 'agent-logo', style: (function () { var k = akLogoInk(a.accent); return { background: k.background, color: k.color }; })() }, (a.label || '?').slice(0, 2).toUpperCase()),
      el('div', { class: 'flex-1-min' },
        el('div', { class: 'spread' },
          el('div', {},
            el('div', { class: 'agent-name' }, a.label),
            el('div', { class: 'agent-path' }, a.home || '—')
          ),
          a.installed ? el('span', { class: 'chip ok' }, t('assets.installed')) : el('span', { class: 'chip ghost' }, t('assets.notInstalled'))
        ),
        el('div', { class: 'agent-counts' },
          ...['skill', 'mcp', 'plugin', 'rule', 'memory'].map((k) =>
            el('span', { class: 'mini' }, el('b', {}, String(counts[k] || 0)), ' ', kindName(k)))
        )
      )
    );
  });
  wrap.appendChild(el('div', { class: 'grid grid-3' }, cards));
  wrap.appendChild(el('div', { class: 'alert info mt' },
    t('agents.coverage', { n: d.agents.length })));
  wrap.appendChild(matrixCard());
  return wrap;
}

/**
 * Agent matrix: which assets exist on which agent. skillhub-local's core
 * view — gaps are immediately visible, and clicking a gap cell offers to
 * distribute the asset to that agent.
 */
function matrixCard() {
  const d = S.data;
  const kind = S.matrixKind || 'skill';
  const agents = d.agents.filter((a) => a.installed);
  // One row per (name, kind) across all agents; scope shown for ambiguity.
  const byName = new Map();
  for (const a of d.assets) {
    if (a.kind !== kind) continue;
    if (!byName.has(a.name)) byName.set(a.name, {});
    byName.get(a.name)[a.agentId] = a;
  }
  const names = [...byName.keys()].sort((x, y) => x.localeCompare(y));

  const cell = (asset, name) => {
    if (asset) {
      return el('td', { class: 'yes', title: asset.file, onclick: () => openAsset(asset.id) }, '●');
    }
    return el('td', { class: 'no', title: t('matrix.gap') }, '·');
  };

  const head = el('tr', {},
    el('th', {}, t('th.name')),
    ...agents.map((ag) => el('th', { class: 'rot' }, el('div', {}, ag.label))),
    el('th', {}, t('matrix.gaps')));

  const body = names.map((name) => {
    const row = byName.get(name);
    const missing = agents.filter((ag) => !row[ag.id]);
    const present = agents.filter((ag) => row[ag.id]);
    // Row click opens the asset wherever it exists; gap cells stay inert.
    const first = present.length ? row[present[0].id] : null;
    const openFirst = first ? () => openAsset(first.id) : null;
    return el('tr', {
      tabindex: openFirst ? '0' : null,
      class: first ? '' : 'inert',
      onclick: openFirst,
      onkeydown: openFirst ? rowKey(openFirst) : null
    },
      el('td', { class: 't-name' }, name),
      ...agents.map((ag) => cell(row[ag.id], name)),
      el('td', { class: 'num' }, missing.length
        ? el('button', { class: 'btn ghost sm',
            onclick: () => { if (present.length) { S.checked.clear(); openDistribute(Object.values(row)[0]); } } },
            missing.length)
        : '—'));
  });

  const kindSel = el('select', { class: 'select-inline', 'aria-label': t('filter.matrixKind') },
    ...['skill', 'rule', 'memory'].map((k) => {
      const o = el('option', { value: k }, t(`dashboard.${k === 'skill' ? 'skills' : k === 'memory' ? 'memory' : 'rules'}`));
      if (k === kind) o.selected = true;
      return o;
    }));
  kindSel.addEventListener('change', () => { S.matrixKind = kindSel.value; render(); });

  return el('div', { class: 'card mt' },
    el('div', { class: 'card-head' },
      el('h2', {}, t('matrix.title')),
      el('span', { class: 'sub spacer' }, t('matrix.note')),
      kindSel),
    el('div', { class: 'card-body scroll-x' },
      el('table', { class: 'tbl matrix' },
        el('thead', {}, head),
        el('tbody', {}, body.length ? body : el('tr', {}, el('td', { colspan: agents.length + 2, class: 'empty' }, t('empty.noMatch')))))));
}

/* ------------------------------------------------------------------ */
/* view: projects                                                      */
/* ------------------------------------------------------------------ */

function viewProjects() {
  const d = S.data;
  const wrap = el('div', {});
  const head = el('div', { class: 'spread mb-12' },
    el('div', { class: 'muted fs-12' },
      t('projects.markerHint', { dirs: ['.workbuddy', '.claude', '.codex', '.cursor'].join('、') })),
    el('button', { class: 'btn primary', onclick: doImportProject }, '+ ' + t('projects.import'))
  );
  wrap.appendChild(head);

  const rows = d.projects.map((p) =>
    // Clicking a project filters the asset list to it (text search over the
    // project name) — the fastest path from "which project owns this" to the assets.
    el('tr', { onclick: () => { S.filter = p.name.toLowerCase(); go('assets'); },
      tabindex: '0', onkeydown: rowKey(() => { S.filter = p.name.toLowerCase(); go('assets'); }) },
      el('td', {},
        el('div', { class: 't-name' }, p.name),
        el('div', { class: 't-mono' }, p.dir)
      ),
      el('td', { class: 'nowrap' }, el('span', { class: 'chip' }, t('projects.agentsCount', { n: p.agents.length }))),
      ...['skill', 'mcp', 'plugin', 'rule', 'memory'].map((k) => el('td', { class: 'num' }, String(p.counts[k] || 0))),
      el('td', { class: 'num muted nowrap' }, ago(p.addedAt)),
      el('td', {}, el('button', {
        class: 'btn sm danger',
        onclick: async () => {
          // Removing a project only rewrites the registry, so the target is the
          // project directory the user is giving up — not a file that changes.
          const ok = await confirmWrite({
            title: t('activity.removeProject'),
            action: t('common.remove'),
            targets: [p.dir],
            detail: t('activity.removeProjectMsg', { x: p.name }),
            danger: true
          });
          if (!ok) return;
          await window.ak.removeProject(p.id);
          toast(t('projects.removed', { x: p.name }), 'ok');
          rescan(true);
        }
      }, t('common.remove')))
    ));

  wrap.appendChild(el('div', { class: 'card' },
    el('table', { class: 'tbl' },
      el('thead', {}, el('tr', {}, el('th', {}, t('th.project')), el('th', {}, t('th.detected')), el('th', { class: 'num' }, t('dashboard.skills')), el('th', { class: 'num' }, t('dashboard.mcp')), el('th', { class: 'num' }, t('dashboard.plugins')), el('th', { class: 'num' }, t('dashboard.rules')), el('th', { class: 'num' }, t('dashboard.memory')), el('th', { class: 'num' }, t('th.added')), el('th', {}, ''))),
      el('tbody', {}, rows.length ? rows : el('tr', {}, el('td', { colspan: '9' },
        el('div', { class: 'empty' },
          t('projects.noneLong'),
          el('div', { class: 'mt-12' },
            el('button', { class: 'btn primary', onclick: doImportProject }, '+ ' + t('projects.import')))))))
    )
  ));
  return wrap;
}

async function doImportProject() {
  const r = await window.ak.pickProject();
  if (r.canceled) return;
  if (r.added.length) toast(t('toast.importedProjects', { n: r.added.length, names: r.added.map((p) => p.name).join(', ') }), 'ok');
  for (const s of r.skipped) toast(`${s.dir}: ${zhText(s.reason)}`, 'warn');
  await rescan(true);
  if (r.added.length) {
    go('projects');
  }
}

/* ------------------------------------------------------------------ */
/* view: health                                                        */
/* ------------------------------------------------------------------ */

function viewHealth() {
  const d = S.data;
  const wrap = el('div', {});

  /* --- security findings first: they outrank format problems --- */
  const risky = d.assets.filter((a) => (a.security || []).length)
    .sort((a, b) => b.security.length - a.security.length);
  if (risky.length) {
    const sevChip = (s) => el('span', { class: `chip ${s === 'error' ? 'err' : 'warn'} mr-6` },
      s === 'error' ? t('health.error') : t('health.warn'));
    const secItems = risky.flatMap((a) => (a.security || []).map((f) =>
      el('div', { class: `issue ${f.severity === 'error' ? 'error' : 'warn'}` },
        el('div', { class: 'issue-body' },
          el('div', { class: 'issue-title' }, a.name, ' ',
            el('span', { class: `chip ${a.kind}` }, kindName(a.kind)), ' ',
            el('span', { class: 'muted fw-400' }, `· ${zhText(a.agentLabel)}`)),
          el('div', { class: 'issue-meta' }, sevChip(f.severity), el('b', {}, f.rule), ` — ${zhText(f.reason)}`),
          el('div', { class: 'issue-meta' }, el('span', { class: 'chip mr-6' }, f.field), f.match),
          el('div', { class: 'issue-meta' }, a.file)
        ),
        el('button', { class: 'btn sm', onclick: () => openAsset(a.id) }, t('common.inspect'))
      )));
    wrap.appendChild(el('div', { class: 'card' },
      el('div', { class: 'card-head' },
        el('h2', {}, t('health.securityTitle')),
        el('span', { class: 'sub spacer' }, t('health.securityNote'))),
      el('div', { class: 'card-body flex col gap-8' }, secItems)
    ));
  }

  /* --- format/validation issues --- */
  const bad = d.assets.filter((a) => (a.issues || []).length && (a.origin || 'user') !== 'cached');
  if (!bad.length && !risky.length) {
    // No `return` here: the baseline card below is also the only place a user
    // can *anchor* a baseline ("建立基线"). Returning early meant the feature was
    // unreachable on a healthy machine — exactly when you would want to record
    // a clean starting point. Reported by the renderer audit as a UI dead end.
    wrap.appendChild(el('div', { class: 'card' }, el('div', { class: 'empty' },
      el('div', { class: 'fs-26 mb-8' }, '✓'),
      t('health.fine'))));
  }
  if (bad.length) {
    const items = bad
      .sort((a, b) => issuesOf(b) - issuesOf(a))
      .map((a) => el('div', { class: 'issue ' + (issuesOf(a) ? 'error' : 'warn') },
        el('div', { class: 'issue-body' },
          el('div', { class: 'issue-title' }, `${a.name} `, el('span', { class: `chip ${a.kind}` }, kindName(a.kind)),
            ' ', el('span', { class: 'muted fw-400' }, `· ${zhText(a.agentLabel)} · ${scopeName(a.scope)}`)),
          ...(a.issues || []).map((i) => el('div', { class: 'issue-meta' },
            el('span', { class: `chip ${i.level === 'error' ? 'err' : 'warn'} mr-6` }, i.field), i.message)),
          el('div', { class: 'issue-meta' }, a.file)
        ),
        el('button', { class: 'btn sm', onclick: () => openAsset(a.id) }, t('common.inspect'))
      ));
    wrap.appendChild(el('div', { class: 'card' },
      el('div', { class: 'card-head' }, el('h2', {}, t(bad.length === 1 ? 'health.attentionOne' : 'health.attention', { n: bad.length })),
        el('span', { class: 'sub spacer' }, t('health.attentionNote'))),
      el('div', { class: 'card-body flex col gap-8' }, items)
    ));
  }

  wrap.appendChild(baselineCard());
  return wrap;
}

/**
 * Fingerprint baseline: what changed since the user last looked. Shows counts
 * of changed/added/gone assets and lets the user re-anchor. Purely local —
 * the fingerprints are SHA256 of file content, already computed by the scan.
 */
function baselineCard() {
  const box = el('div', { class: 'card' },
    el('div', { class: 'card-head' }, el('h2', {}, t('baseline.title')),
      el('span', { class: 'sub spacer' }, t('baseline.note'))),
    el('div', { class: 'card-body' }, el('div', { class: 'muted' }, t('common.loading'))));
  window.ak.baselineDiff().then((r) => {
    if (!r.ok) return;
    box.querySelector('.card-body').innerHTML = '';
    const body = box.querySelector('.card-body');
    if (!r.hasBaseline) {
      body.appendChild(el('div', { class: 'alert info mb-12' }, t('baseline.none')));
    } else if (!r.changed.length && !r.added.length && !r.gone.length) {
      body.appendChild(el('div', { class: 'alert ok mb-12' }, t('baseline.clean')));
    } else {
      body.appendChild(el('div', { class: 'flex flex-wrap gap-8 mb-12' },
        r.changed.length ? el('span', { class: 'chip warn' }, t('baseline.changed', { n: r.changed.length })) : null,
        r.added.length ? el('span', { class: 'chip ok' }, t('baseline.added', { n: r.added.length })) : null,
        r.gone.length ? el('span', { class: 'chip err' }, t('baseline.gone', { n: r.gone.length })) : null));
    }
    body.appendChild(el('button', { class: 'btn',
      onclick: async () => {
        const res = await window.ak.setBaseline();
        if (res.ok) toast(t('baseline.set', { n: res.count }), 'ok');
        render();
      } }, t('baseline.anchor')));
  }).catch((e) => {
    // The card sat on "loading…" forever before this: a rejected invoke left no
    // trace in the UI and only an unhandled rejection in the console.
    reportUiError('error', e);
    const body = box.querySelector('.card-body');
    if (body) inlineRetry(body, () => render());
  });
  return box;
}

/* ------------------------------------------------------------------ */
/* view: activity                                                      */
/* ------------------------------------------------------------------ */

async function viewActivity() {
  const wrap = el('div', {});
  const audit = await window.ak.audit({ limit: 200 });

  // No transaction list and no undo button: the store no longer keeps
  // backups, so an "undo" here could not restore anything — the journal is
  // now purely a record of what happened, not a menu of things to reverse.
  const items = audit.filter((r) => r.txId || r.action === 'scan').slice(0, 160).map((r) =>
    el('div', { class: `tl-item ${r.status === 'ok' ? 'ok' : r.status === 'failed' ? 'failed' : ''}` },
      el('div', { class: 'tl-head' },
        el('span', { class: 'tl-title' }, labelFor(r)),
        r.kind ? el('span', { class: `chip ${r.kind}` }, r.kind) : null,
        el('span', { class: 'tl-time' }, clock(r.ts))
      ),
      el('div', { class: 'tl-meta' },
        [zhText(r.agentLabel) || r.agentId, r.projectName, r.assetName, (r.targets || []).join('  '), zhError(r)]
          .filter(Boolean).join('  ·  '))
    )
  );

  wrap.appendChild(el('div', { class: 'card' },
    el('div', { class: 'card-head' },
      el('h2', {}, t('card.audit')),
      el('span', { class: 'sub spacer' }, t('activity.journalNote', { n: audit.length }))),
    el('div', { class: 'card-body' },
      items.length ? el('div', { class: 'tl' }, items) : el('div', { class: 'empty' }, t('empty.noActivity')))
  ));
  return wrap;
}

function labelFor(r) {
  if (r.action === 'scan') return t('activity.scanLabel', { n: r.count ?? '' });
  const verb = { create: t('verb.create'), update: t('verb.update'), 'delete': t('verb.delete'), distribute: t('verb.distribute') }[r.action] || r.action || t('verb.change');
  return `${verb} ${r.assetName || ''}`.trim() + (r.status === 'failed' ? ` ${t('activity.failedSuffix')}` : '');
}

/* ------------------------------------------------------------------ */
/* view: settings                                                      */
/* ------------------------------------------------------------------ */

/* ------------------------------------------------------------------ */
/* discover: search the public registry, install skills via git        */
/* ------------------------------------------------------------------ */

function viewDiscover() {
  const root = el('div', {});
  const repoInput = el('input', {
    class: 'input-inline wide', type: 'text', id: 'disc-repo',
    'aria-label': t('discover.repoLabel'),
    placeholder: 'https://github.com/owner/repo  ·  owner/repo',
    onkeydown: (e) => { if (e.key === 'Enter') void discInstallFromInput(); }
  });
  root.appendChild(el('div', { class: 'card' },
    el('div', { class: 'card-body' },
      el('div', { class: 'flex' },
        el('div', { class: 'search-wrap grow' },
          el('input', {
            id: 'disc-q', type: 'search', value: S.discQ || '',
            'aria-label': t('discover.searchLabel'),
            placeholder: t('discover.searchPlaceholder'), autocomplete: 'off',
            onkeydown: (e) => { if (e.key === 'Enter') discSearch(); }
          })
        ),
        el('button', { class: 'btn primary', id: 'disc-go', onclick: () => discSearch() }, t('discover.search'))
      ),
      el('div', { class: 'muted disc-hint' }, t('discover.hint'))
    )
  ));

  // Direct install by repository. The engine has always accepted any
  // owner/repo or GitHub URL (ak:install-skill); without this box the only
  // reachable path was "already listed in the registry", which excludes every
  // private or brand-new repository.
  root.appendChild(el('div', { class: 'card mt-14' },
    el('div', { class: 'card-head' }, el('h2', {}, t('discover.byRepo'))),
    el('div', { class: 'card-body' },
      el('div', { class: 'flex gap-sm' },
        repoInput,
        el('button', { class: 'btn', onclick: () => void discInstallFromInput() }, t('discover.installFromRepo'))),
      el('div', { class: 'muted disc-hint' }, t('discover.byRepoHint')))
  ));

  const results = el('div', { id: 'disc-results', class: 'mt' });
  root.appendChild(results);
  renderDiscResults(results);
  return root;
}

/**
 * Install straight from a pasted repository reference. Reuses the same
 * dialog as a registry result so the destination picker, the security scan and
 * the shadow-layer staging behave identically.
 */
async function discInstallFromInput() {
  const input = $('#disc-repo');
  const raw = input ? String(input.value || '').trim() : '';
  if (!raw) return;
  const agents = ((S.data && S.data.agents) || []).filter((a) => a.installed);
  if (!agents.length) { toast(t('discover.noAgent'), 'err'); return; }
  const sel = el('select', { class: 'input-inline' },
    agents.map((a) => el('option', { value: a.id }, a.label)));
  const body = el('div', {},
    el('div', { class: 'set-row' },
      el('div', { class: 'set-label' }, t('discover.targetAgent')),
      el('div', { class: 'set-control' }, sel)),
    el('div', { class: 'muted disc-hint' }, t('discover.directNote'))
  );
  const cancel = el('button', { class: 'btn', onclick: closeModal }, t('common.cancel'));
  const okBtn = el('button', {
    class: 'btn primary', onclick: async () => {
      // Installing writes new files into the agent's global skills directory —
      // a mutation, so it asks like every other one before starting the clone.
      const ok = await confirmWrite({
        action: t('verb.install'),
        targets: targetPaths([{ agentId: sel.value, scope: 'global', projectId: null }], 'skills'),
        detail: t('confirm.installDetail', { repo: raw })
      });
      if (!ok) return;
      okBtn.disabled = true;
      okBtn.classList.add('loading');
      okBtn.textContent = t('discover.installing');
      const r = await window.ak.installSkill({ repo: raw, agentId: sel.value });
      closeModal();
      if (input) input.value = '';
      if (!r.ok) { toast(r.error, 'err'); return; }
      const parts = [t('discover.done', { n: (r.installed || []).length })];
      if ((r.skipped || []).length) parts.push(t('discover.skipped', { n: r.skipped.length }));
      if ((r.findings || []).length) parts.push(t('discover.securityWarn', { n: r.findings.length }));
      toast(parts.join(' · '), (r.findings || []).length ? 'warn' : 'ok');
    }
  }, t('discover.install'));
  openModal(t('discover.installTitle', { name: raw }), body, [cancel, okBtn]);
}

async function discSearch() {
  const input = $('#disc-q');
  S.discQ = input ? input.value.trim() : S.discQ;
  S.discLoading = true;
  S.discError = null;
  const goBtn = $('#disc-go');
  if (goBtn) { goBtn.classList.add('loading'); goBtn.disabled = true; }
  renderDiscResults($('#disc-results'));
  let r;
  try {
    r = await window.ak.searchRegistry({ q: S.discQ, limit: 30 });
  } catch (e) {
    // The button stayed in its loading state forever when the invoke rejected.
    reportUiError('error', e);
    r = { ok: false, error: (e && e.message) || String(e) };
  } finally {
    S.discLoading = false;
    if (goBtn) { goBtn.classList.remove('loading'); goBtn.disabled = false; }
  }
  if (!r.ok) { S.discError = r.error; S.discResults = []; }
  else { S.discResults = r.skills; S.discTotal = r.total; }
  renderDiscResults($('#disc-results'));
}

function renderDiscResults(container) {
  if (!container) return;
  container.innerHTML = '';
  if (S.discLoading) {
    // Skeleton rows shimmer while the registry round-trip is in flight.
    for (const w of ['s1', 's2', 's3', 's2']) {
      container.appendChild(el('div', { class: 'disc-row' },
        el('div', { class: 'skeleton ' + w })));
    }
    return;
  }
  if (S.discError) {
    container.appendChild(el('div', { class: 'alert err' }, t('discover.registryError', { msg: zhError(S.discError) })));
    return;
  }
  if (S.discResults === null) {
    container.appendChild(el('div', { class: 'empty' }, t('discover.emptyIdle')));
    return;
  }
  if (!S.discResults.length) {
    container.appendChild(el('div', { class: 'empty' }, t('discover.emptyNoResults', { q: S.discQ })));
    return;
  }
  const card = el('div', { class: 'card' },
    el('div', { class: 'card-head' },
      el('h2', {}, t('discover.resultsTitle')),
      el('span', { class: 'sub' }, t('discover.resultsCount', { shown: S.discResults.length, total: S.discTotal }))
    ),
    el('div', { class: 'card-body' },
      S.discResults.map((item) => el('div', { class: 'disc-row' },
        el('div', { class: 'disc-main' },
          el('div', { class: 'flex' },
            el('span', { class: 'disc-name' }, item.name),
            item.author ? el('span', { class: 'chip ghost' }, item.author) : null,
            item.category ? el('span', { class: 'chip' }, item.category) : null,
            item.installs ? el('span', { class: 'mini' }, t('discover.installs', { n: item.installs })) : null
          ),
          el('div', { class: 'disc-desc', title: item.description }, item.description || ''),
          el('div', { class: 'mini muted' }, item.repo ? `${item.repo}` : t('discover.noRepo'))
        ),
        el('button', {
          class: 'btn sm primary',
          disabled: !item.repo,
          title: item.repo ? t('discover.install') : t('discover.noRepo'),
          onclick: () => discInstall(item)
        }, t('discover.install'))
      ))
    )
  );
  container.appendChild(card);
}

async function discInstall(item) {
  const agents = ((S.data && S.data.agents) || []).filter((a) => a.installed);
  if (!agents.length) { toast(t('discover.noAgent'), 'err'); return; }
  const select = el('select', { class: 'input-inline', id: 'disc-agent' },
    agents.map((a) => el('option', { value: a.id }, a.label)));
  const body = el('div', {},
    el('div', { class: 'set-row' },
      el('div', { class: 'set-label' }, t('discover.targetAgent')),
      el('div', { class: 'set-control' }, select)
    ),
    el('div', { class: 'muted disc-hint' }, t('discover.directNote'))
  );
  const cancel = el('button', { class: 'btn', onclick: closeModal }, t('common.cancel'));
  const okBtn = el('button', {
    class: 'btn primary', onclick: async () => {
      const agentId = select.value;
      const ok = await confirmWrite({
        action: t('verb.install'),
        targets: targetPaths([{ agentId, scope: 'global', projectId: null }], 'skills'),
        detail: t('confirm.installDetail', { repo: item.repo })
      });
      if (!ok) return;
      okBtn.disabled = true;
      okBtn.classList.add('loading');
      okBtn.textContent = t('discover.installing');
      const r = await window.ak.installSkill({ repo: item.repo, agentId });
      closeModal();
      if (!r.ok) { toast(r.error, 'err'); return; }
      const parts = [t('discover.done', { n: (r.installed || []).length })];
      if ((r.skipped || []).length) parts.push(t('discover.skipped', { n: r.skipped.length }));
      if ((r.findings || []).length) parts.push(t('discover.securityWarn', { n: r.findings.length }));
      toast(parts.join(' · '), (r.findings || []).length ? 'warn' : 'ok');
    }
  }, t('discover.install'));
  openModal(t('discover.installTitle', { name: item.name }), body, [cancel, okBtn]);
}

function viewSettings() {
  const wrap = el('div', {});
  const cfg = S.settings || {};

  /* --- one row per setting ------------------------------------------- */
  /**
   * A settings row is a real <label for> plus an id on the control, never a
   * styled div: a screen reader announces "Notifications, switch" from the
   * label, and clicking the text operates the control. Rows whose control is
   * not labelable (a read-only path, a button group) keep the plain div and
   * the control carries its own name.
   */
  let setRowSeq = 0;
  const LABELABLE = 'input, select, textarea, button, meter, output, progress';
  const row = (label, control, hint) => {
    const target = control.matches && control.matches(LABELABLE)
      ? control
      : (control.querySelector ? control.querySelector(LABELABLE) : null);
    if (!target) {
      return el('div', { class: 'set-row' },
        el('div', { class: 'set-label' }, label, hint ? el('div', { class: 'set-hint' }, hint) : null),
        el('div', { class: 'set-control' }, control));
    }
    if (!target.id) target.id = `set-row-${++setRowSeq}`;
    const hintId = hint ? `${target.id}-hint` : null;
    if (hintId) target.setAttribute('aria-describedby', hintId);
    // role="switch" exposes no text of its own: the label is its only name.
    if (target.getAttribute('role') === 'switch' && !target.getAttribute('aria-label')) {
      target.setAttribute('aria-label', label);
    }
    return el('div', { class: 'set-row' },
      el('div', { class: 'set-label' },
        el('label', { for: target.id }, label),
        hint ? el('div', { class: 'set-hint', id: hintId }, hint) : null),
      el('div', { class: 'set-control' }, control));
  };

  const select = (value, options, onChange) => {
    const s = el('select', { class: 'select-inline' });
    for (const [v, label] of options) {
      const o = el('option', { value: v }, label);
      if (v === value) o.selected = true;
      s.appendChild(o);
    }
    s.addEventListener('change', () => onChange(s.value));
    return s;
  };

  /** `name` becomes the switch's accessible name (role=switch has no text). */
  const toggle = (on, onChange, name) => {
    const b = el('button', {
      class: `switch${on ? ' on' : ''}`,
      role: 'switch',
      'aria-checked': on ? 'true' : 'false',
      'aria-label': name || null
    }, el('span', { class: 'knob' }));
    b.addEventListener('click', () => onChange(!on));
    return b;
  };

  const save = async (patch) => {
    const r = await window.ak.saveSettings(patch);
    if (r.ok) {
      S.settings = r.settings;
      toast(t('common.saved'), 'ok');
      render();
    } else {
      toast(r.error || t('toast.saveFailed'), 'err');
      // The control still showed the rejected value, so the UI claimed a change
      // that never reached settings.json. Re-read what was actually stored.
      render();
    }
  };

  /* --- appearance --------------------------------------------------- */
  const appearance = el('div', { class: 'card' },
    el('div', { class: 'card-head' }, el('h2', {}, t('settings.appearance'))),
    el('div', { class: 'card-body' },
      row(t('settings.language'),
        select(cfg.lang || 'auto', [
          ['auto', t('settings.langAuto')],
          ['zh', t('settings.langZh')],
          ['en', t('settings.langEn')]
        ], (v) => { i18n.setLang(v); save({ lang: v }); })),
      row(t('settings.theme'),
        select(cfg.theme || 'system', [
          ['system', t('settings.themeSystem')],
          ['light', t('settings.themeLight')],
          ['dark', t('settings.themeDark')]
        ], (v) => { localStorage.setItem('ak.theme', v); applyTheme(v); save({ theme: v }); })),
      el('div', { class: 'muted fs-11 mt-6' }, t('settings.restartNote'))
    ));

  /* --- behaviour ---------------------------------------------------- */
  const behaviour = el('div', { class: 'card' },
    el('div', { class: 'card-head' }, el('h2', {}, t('settings.behaviour'))),
    el('div', { class: 'card-body' },
      row(t('settings.notifications'), toggle(cfg.notifications !== false, (v) => save({ notifications: v }), t('settings.notifications')), t('settings.notificationsHint')),
      row(t('settings.localStats'),
        el('div', { class: 'flex gap-sm' },
          toggle(cfg.showLocalStats !== false, (v) => save({ showLocalStats: v }), t('settings.localStats')),
          localStatsRow()),
        t('settings.localStatsHint'))
    ));

  /**
   * The GitHub mark, inline (Octicons `mark-github`, 16×16).
   *
   * Inline SVG rather than an <img>: the CSP allows `img-src 'self' data:`, so
   * an icon would have to become a packaged asset that `copy-frontend.js` ships
   * and the dist gate keeps in sync — for one glyph. `fill: currentColor` lets
   * the surrounding button own the colour, including hover.
   */
  const GITHUB_MARK = 'M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27s1.36.09 2 .27c1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8Z';

  const MARK_SIZE = 16;

  function githubMark() {
    const NS = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(NS, 'svg');
    svg.setAttribute('viewBox', `0 0 ${MARK_SIZE} ${MARK_SIZE}`);
    svg.setAttribute('width', String(MARK_SIZE));
    svg.setAttribute('height', String(MARK_SIZE));
    svg.setAttribute('class', 'upd-source-mark');
    svg.setAttribute('aria-hidden', 'true');
    svg.setAttribute('focusable', 'false');
    const path = document.createElementNS(NS, 'path');
    path.setAttribute('fill', 'currentColor');
    path.setAttribute('d', GITHUB_MARK);
    svg.appendChild(path);
    return svg;
  }

  /**
   * Software update card.
   *
   * Reads `S.update` — the snapshot the main process pushes. Every control
   * saves through `ak:update-settings` and re-renders from the returned
   * snapshot, so the card can never show a value the updater is not using.
   */
  function updateCard() {
    const u = S.update || {};
    const ucfg = u.settings || { auto: cfg.updateCheck === true, minutes: cfg.updateCheckMinutes || 20, beta: cfg.updateBeta === true, proxy: cfg.updateProxy || 'auto' };
    const body = el('div', { class: 'card-body' });

    const saveUpdate = async (patch) => {
      if (S.updateBusy) return;
      S.updateBusy = true;
      const r = await window.ak.updateSaveSettings(patch).catch((e) => ({ ok: false, error: e.message }));
      S.updateBusy = false;
      if (!r || r.ok === false) { toast((r && r.error) || t('toast.saveFailed'), 'err'); return; }
      S.update = r;
      try { S.settings = await window.ak.getSettings(); } catch (e) { /* keep old */ }
      toast(t('common.saved'), 'ok');
      render();
    };

    const checkNow = async (btn) => {
      if (S.updateBusy) return;
      S.updateBusy = true;
      btn.disabled = true;
      const label = btn.textContent;
      btn.textContent = t('settings.updateChecking');
      const r = await window.ak.updateCheck().catch((e) => ({ ok: false, error: e.message }));
      S.updateBusy = false;
      btn.disabled = false;
      btn.textContent = label;
      if (r) {
        S.update = r;
        // "throttled" is a successful reuse of the previous answer — anything
        // else that failed (including a GitHub cooldown) must say why.
        if (r.ok === false && r.reason !== 'throttled') {
          toast(t('settings.updateError', { error: r.lastError || r.error || 'unknown' }), 'err');
        }
      }
      syncUpdateBanner(S.update);
      render();
      if (r && r.ok !== false) promptUpdate(r);
    };

    const versionLine = () => {
      if (u.hasUpdate === true) return t('settings.updateAvailable');
      if (u.hasUpdate === false) return t('settings.updateUpToDate');
      if (u.comparison === 'older') return t('settings.updateOlder');
      // Checked but no release came back. GitHub answers 404 both for "no
      // release published yet" and for "this repository is private and you are
      // anonymous" — it cannot tell the two apart on purpose. With no token
      // saved, the second cause is the one worth naming: the panel used to say
      // "no release yet" on a repository that had one, which reads as a bug.
      if (u.checked && !u.latest) {
        return u.token && u.token.filled === false
          ? t('settings.updateNoReleaseToken')
          : t('settings.updateNoRelease');
      }
      return u.checked ? '' : t('settings.updateNotChecked');
    };

    const checkBtn = el('button', { class: 'btn sm', onclick: (e) => void checkNow(e.currentTarget) }, t('settings.updateCheckNow'));

    /**
     * The auto-update settings behind one button.
     *
     * Source, interval, beta channel, proxy, silent install and the token are
     * six controls a user opens once and then never touches again. Inlined in
     * the card they pushed the version comparison — the one thing that needs a
     * decision — below the fold, so they now live in a dialog and the card
     * carries only the state and the two actions.
     */
    const openUpdateSettings = () => openUpdateSettingsModal(ucfg, saveUpdate);

    // Version comparison — the two numbers plus a one-line verdict. Both
    // actions sit at the right end: 检查更新 is the right-most element, so the
    // eye lands on the primary action rather than having to search for it.
    const versionRow = el('div', { class: 'upd-version' },
      el('div', { class: 'upd-version-cell' },
        el('div', { class: 'set-hint' }, t('settings.updateCurrent')),
        el('div', { class: 'upd-version-num mono' }, u.current || (S.boot && S.boot.version) || '—')),
      el('div', { class: 'upd-arrow' }, '→'),
      el('div', { class: 'upd-version-cell' },
        el('div', { class: 'set-hint' }, t('settings.updateLatest')),
        // A dash reads as "this field is empty"; the honest state is "the last
        // check could not name a version", so say that instead.
        el('div', { class: `upd-version-num mono${u.hasUpdate === true ? ' is-new' : ''}` }, u.latest || t('settings.updateUnknown'))),
      el('div', { class: 'spacer' }),
      el('button', { class: 'btn sm', onclick: openUpdateSettings }, t('settings.updateSettingsBtn')),
      checkBtn);

    body.appendChild(versionRow);
    const verdict = versionLine();
    if (verdict) body.appendChild(el('div', { class: 'muted fs-11 mt-6' }, verdict));

    // Timestamps: "checked at" is the moment of the real check (main-process
    // clock), never "a few minutes ago because the page re-rendered".
    const stamps = [];
    if (u.checkedAt) stamps.push(t('settings.updateCheckedAt', { time: new Date(u.checkedAt).toLocaleString() }));
    if (u.settings && u.settings.auto && u.nextCheckAt) {
      stamps.push(t('settings.updateNextAt', { time: new Date(u.nextCheckAt).toLocaleString() }));
    }
    if (stamps.length) body.appendChild(el('div', { class: 'muted fs-11 mt-6' }, stamps.join(' · ')));
    if (u.lastError) body.appendChild(el('div', { class: 'alert warn mt-8' }, t('settings.updateError', { error: u.lastError })));

    // Download state: progress while active, install button once ready.
    const task = u.task;
    if (task && task.active) {
      const line = el('div', { class: 'upd-progress-line' });
      const bar = el('div', { class: 'upd-progress-track' });
      const fill = el('div', { class: 'upd-progress-fill' });
      fill.style.setProperty('width', `${Math.max(0, Math.min(100, Number(task.percent) || 0))}%`);
      bar.appendChild(fill);
      line.appendChild(el('div', { class: 'muted fs-11' },
        t('settings.updateDownloading', { name: task.filename || '', percent: String(Math.round(Number(task.percent) || 0)) })));
      line.appendChild(bar);
      line.appendChild(el('button', {
        class: 'btn sm mt-6',
        onclick: () => void window.ak.updateCancel().catch(() => {})
      }, t('settings.updateCancelDownload')));
      body.appendChild(line);
    } else if (task && task.done && task.path) {
      const installBtn = el('button', {
        class: 'btn primary sm',
        onclick: async () => {
          installBtn.disabled = true;
          const r = await window.ak.updateInstall(task.path).catch((e) => ({ ok: false, error: e.message }));
          if (!r || !r.ok) { installBtn.disabled = false; toast(t('update.installFailed', { error: (r && r.error) || 'unknown' }), 'err'); }
          else toast(t('update.installing'), 'ok');
        }
      }, t('settings.updateInstall'));
      body.appendChild(el('div', { class: 'upd-ready mt-8' },
        el('span', { class: 'chip ok' }, t('settings.updateReady')),
        el('span', { class: 'muted fs-11' }, task.filename || ''),
        installBtn));
    }

    // The release notes are NOT repeated here. They used to be a <details> on
    // this card, and the changelog panel below lists every released version
    // with the same body — so the newest release appeared twice, word for word,
    // one card apart. One place per version: the panel owns the notes, this
    // card owns the decision and keeps the single link to the release page.
    if (u.pageUrl) {
      body.appendChild(el('div', { class: 'mt-6' },
        el('button', {
          class: 'btn ghost sm',
          onclick: () => void window.ak.openReleasePage(u.pageUrl).catch(() => {})
        }, t('settings.updateOpenPage'))));
    }

    return el('div', { class: 'card' },
      el('div', { class: 'card-head' },
        el('h2', {}, t('settings.updateTitle')),
        el('span', { class: 'sub spacer' }, u.latest ? `v${u.latest}` : '')),
      body);
  }

  /**
   * Auto-update settings, behind the card's 更新设置 button.
   *
   * Every control writes through `saveUpdate` and re-renders from the snapshot
   * the main process returns, so a value on screen is always the value the
   * updater is using. The interval field keeps its draft in S so a re-render
   * (a poll lands while the user is typing) does not discard half-typed input.
   *
   * The release repository is NOT editable: it is a property of this build
   * (package.json → repository), and asking the user to type their own
   * project's address is the single most confusing thing this dialog could do.
   */
  function openUpdateSettingsModal(ucfg, saveUpdate) {
    const u = S.update || {};
    const body = el('div', {});

    const row = (label, control, hint) => {
      const target = control.matches && control.matches(LABELABLE)
        ? control
        : (control.querySelector ? control.querySelector(LABELABLE) : null);
      const labelNode = target
        ? (target.id ? null : (target.id = `upd-set-${++setRowSeq}`), el('label', { for: target.id }, label))
        : null;
      if (target) {
        const hintId = hint ? `${target.id}-hint` : null;
        if (hintId) target.setAttribute('aria-describedby', hintId);
        if (target.getAttribute('role') === 'switch' && !target.getAttribute('aria-label')) {
          target.setAttribute('aria-label', label);
        }
      }
      return el('div', { class: 'set-row' },
        el('div', { class: 'set-label' },
          labelNode || label,
          hint ? el('div', { class: 'set-hint' }, hint) : null),
        el('div', { class: 'set-control' }, control));
    };

    const switchFor = (on, onChange, name) => {
      const b = el('button', {
        class: `switch${on ? ' on' : ''}`,
        role: 'switch',
        'aria-checked': on ? 'true' : 'false',
        'aria-label': name || null
      }, el('span', { class: 'knob' }));
      b.addEventListener('click', () => onChange(!on));
      return b;
    };

    // The source is a link, not a sentence: the repository is the one thing in
    // this dialog a user may need to open — to read a release, or to check
    // whether the repository is private before hunting for a token. Rendered as
    // the GitHub mark + `owner/repo`, opening through the same command the
    // release-page links use (so the URL allow-list stays in Rust).
    const repoUrl = u.repo ? `https://github.com/${u.repo}` : '';
    const repoLabel = u.repo || t('settings.updateUnknown');
    body.appendChild(row(t('settings.updateSource'),
      el('button', {
        class: 'btn ghost sm upd-source',
        type: 'button',
        title: repoUrl || null,
        'aria-label': repoUrl ? t('settings.updateOpenRepo', { repo: repoLabel }) : null,
        onclick: () => { if (repoUrl) void window.ak.openReleasePage(repoUrl).catch(() => {}); }
      }, githubMark(), el('span', { class: 'mono fs-11' }, repoLabel)),
      t('settings.updateSourceHint')));

    body.appendChild(row(t('settings.updateAuto'),
      switchFor(ucfg.auto, (v) => void saveUpdate({ auto: v }), t('settings.updateAuto')),
      t('settings.updateAutoHint')));

    const minutesValue = S.updateMinutesDraft !== '' ? S.updateMinutesDraft : String(ucfg.minutes || 20);
    /**
     * One validator for both ways out of this field.
     *
     * The input's `onchange` checked 1–1440 but the Save button wrote whatever
     * was typed (`0`, `99999`) straight to disk, and the failure text was the
     * hint sentence rather than a statement that nothing was saved.
     */
    const commitMinutes = () => {
      const draft = S.updateMinutesDraft;
      const value = Math.round(Number(draft === '' ? ucfg.minutes : draft));
      S.updateMinutesDraft = '';
      if (!Number.isFinite(value) || value < 1 || value > 1440) {
        toast(t('settings.updateIntervalInvalid'), 'err');
        return;
      }
      void saveUpdate({ minutes: value });
    };
    const minutesInput = el('input', {
      class: 'input-inline',
      type: 'number',
      min: '1',
      max: '1440',
      step: '1',
      value: minutesValue,
      oninput: (e) => { S.updateMinutesDraft = e.target.value; },
      onchange: commitMinutes
    });
    body.appendChild(row(t('settings.updateInterval'),
      el('div', { class: 'flex gap-sm' }, minutesInput,
        el('button', { class: 'btn sm', onclick: commitMinutes }, t('settings.updateSave'))),
      t('settings.updateIntervalHint')));

    body.appendChild(row(t('settings.updateBeta'),
      switchFor(ucfg.beta, (v) => void saveUpdate({ beta: v }), t('settings.updateBeta')),
      t('settings.updateBetaHint')));

    // Proxy: auto / direct / the Clash mixed port. A hand-edited settings file
    // may hold another URL — keep it visible instead of silently rewriting it.
    const proxyOptions = [
      ['auto', t('settings.updateProxyAuto')],
      ['direct', t('settings.updateProxyDirect')],
      ['http://127.0.0.1:7897', t('settings.updateProxyClash')]
    ];
    if (ucfg.proxy && !proxyOptions.some(([v]) => v === ucfg.proxy)) {
      proxyOptions.push([ucfg.proxy, ucfg.proxy]);
    }
    const proxySel = el('select', { class: 'select-inline' });
    for (const [v, label] of proxyOptions) {
      const o = el('option', { value: v }, label);
      if (v === (ucfg.proxy || 'auto')) o.selected = true;
      proxySel.appendChild(o);
    }
    proxySel.addEventListener('change', () => void saveUpdate({ proxy: proxySel.value }));
    body.appendChild(row(t('settings.updateProxy'), proxySel, t('settings.updateProxyHint')));

    // Silent install: NSIS /S /R. Windows-only wording, but the switch is
    // always visible so the setting is discoverable before it matters.
    if (window.ak.platform === 'win32') {
      body.appendChild(row(t('settings.updateSilent'),
        switchFor(ucfg.silent !== false, (v) => void saveUpdate({ silent: v }), t('settings.updateSilent')),
        t('settings.updateSilentHint')));
    }

    // Token: status badge + write-only input. The value is never echoed —
    // the main process only reports filled/origin.
    const token = u.token || { filled: false, origin: null, error: null };
    let badge = t('settings.updateTokenEmpty');
    if (token.error) badge = token.error;
    else if (token.filled && token.origin === 'stored') badge = t('settings.updateTokenFilled');
    else if (token.filled) badge = t('settings.updateTokenEnv');
    const tokenInput = el('input', {
      class: 'input-inline wide',
      id: 'set-update-token',
      'aria-describedby': 'set-update-token-hint',
      type: 'password',
      autocomplete: 'new-password',
      spellcheck: 'false',
      placeholder: token.filled && token.origin !== 'env'
        ? t('settings.updateTokenEncrypted')
        : 'ghp_… / github_pat_…',
      value: S.updateTokenDraft,
      oninput: (e) => { S.updateTokenDraft = e.target.value; }
    });
    if (u.encryption === false) tokenInput.disabled = true;

    const tokenRow = el('div', { class: 'flex gap-sm' },
      el('span', { class: 'chip' }, badge),
      tokenInput,
      el('button', {
        class: 'btn sm',
        disabled: u.encryption === false,
        onclick: async () => {
          const value = String(S.updateTokenDraft || '').trim();
          if (!value) { toast(t('settings.updateTokenHint'), 'err'); return; }
          const r = await window.ak.updateSetToken(value).catch((e) => ({ ok: false, error: e.message }));
          if (!r || r.ok === false) { toast((r && r.error) || t('toast.saveFailed'), 'err'); return; }
          S.updateTokenDraft = '';
          S.update = r;
          toast(t('common.saved'), 'ok');
          render();
        }
      }, t('settings.updateTokenSave')),
      token.filled && token.origin === 'stored'
        ? el('button', {
          class: 'btn ghost sm',
          onclick: async () => {
            const r = await window.ak.updateSetToken(null).catch((e) => ({ ok: false, error: e.message }));
            if (r && r.ok !== false) { S.update = r; toast(t('common.saved'), 'ok'); render(); }
          }
        }, t('settings.updateTokenClear'))
        : null);

    body.appendChild(el('div', { class: 'set-row' },
      el('div', { class: 'set-label' },
        el('label', { for: 'set-update-token' }, t('settings.updateToken')),
        el('div', { class: 'set-hint', id: 'set-update-token-hint' }, u.encryption === false ? t('settings.updateTokenNoCrypto') : t('settings.updateTokenHint'))),
      el('div', { class: 'set-control' }, tokenRow)));

    openModal(t('settings.updateSettingsTitle'), body,
      [el('button', { class: 'btn primary', onclick: closeModal }, t('common.done'))]);
  }

  /**
   * Changelog panel: every published release, newest first.
   *
   * Fetched on its own, on first render, and cached for the session — the
   * release history is a separate fact from the update *check* (which the
   * updater throttles), so it must not go through `updateCheck`: clicking
   * 检查更新 would then mean "also refetch 30 release bodies". A failure here
   * says so in place and can be retried, rather than leaving an empty panel
   * that looks like "this project has no releases".
   */
  function changelogCard() {
    const list = el('div', { class: 'upd-changelog' }, el('div', { class: 'muted' }, t('common.loading')));

    const card = el('div', { class: 'card mt-14' },
      el('div', { class: 'card-head' },
        el('h2', {}, t('settings.changelogTitle')),
        el('span', { class: 'sub spacer' }, t('settings.changelogHint')),
        el('button', { class: 'btn sm', onclick: () => load() }, '↻')),
      el('div', { class: 'card-body' }, list));

    const load = async () => {
      list.innerHTML = '';
      list.appendChild(el('div', { class: 'muted' }, t('common.loading')));
      try {
        const r = await window.ak.updateReleases(20);
        if (!Array.isArray(r) || !r.length) {
          list.innerHTML = '';
          list.appendChild(el('div', { class: 'empty upd-log-empty' }, t('settings.changelogEmpty')));
          return;
        }
        const current = (S.update && S.update.current) || (S.boot && S.boot.version) || '';
        list.innerHTML = '';
        for (const rel of r) {
          // No per-row button: twenty "open the releases page" buttons are one
          // action repeated twenty times, and each one only navigated away from
          // the notes the user was reading. The version label itself is the
          // link — one target per release, and the row still expands to show
          // what that version changed.
          const verLabel = el('span', { class: 'upd-log-ver mono' }, `v${rel.version}`);
          if (rel.url) {
            verLabel.classList.add('clickable');
            verLabel.setAttribute('role', 'link');
            verLabel.setAttribute('tabindex', '0');
            verLabel.setAttribute('title', t('settings.updateOpenPage'));
            const open = (e) => { e.preventDefault(); e.stopPropagation(); void window.ak.openReleasePage(rel.url).catch(() => {}); };
            verLabel.addEventListener('click', open);
            verLabel.addEventListener('keydown', (e) => {
              if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); open(e); }
            });
          }
          list.appendChild(el('details', { class: 'upd-log-item' },
            el('summary', {},
              verLabel,
              el('span', { class: 'upd-log-date' }, (rel.date || '').slice(0, 10)),
              String(rel.version) === String(current)
                ? el('span', { class: 'chip ok' }, t('settings.changelogCurrent'))
                : null,
              el('span', { class: 'spacer' }),
              rel.prerelease ? el('span', { class: 'chip warn' }, t('settings.changelogPre')) : null),
            el('pre', { class: 'upd-log-notes' }, rel.notes || t('update.noNotes'))));
        }
      } catch (e) {
        reportUiError('error', e);
        list.innerHTML = '';
        inlineRetry(list, () => load());
      }
    };

    load();
    return card;
  }

  /**
   * Local usage counters, read from the audit journal — nothing leaves the
   * machine. The row is the visible half of `showLocalStats`: a number with no
   * switch is a promise the user cannot keep.
   */
  function localStatsRow() {
    const box = el('div', { class: 'mono fs-11 stats-value-right' });
    if (cfg.showLocalStats === false) {
      box.textContent = '—';
      return box;
    }
    box.appendChild(el('div', { class: 'muted' }, t('common.loading')));
    window.ak.localStats().then((r) => {
      if (!r.ok) { box.textContent = '—'; return; }
      S.localStats = r.counts;
      // One <span class="nowrap"> per counter inside a wrapping flex row: the
      // card is narrow, and inline text broke "0 次失败" in half between "0"
      // and "次". Wrapping is allowed between counters, never inside one.
      box.textContent = '';   // drop the "loading…" placeholder
      String(t('settings.localStatsValue', r.counts)).split(' · ')
        .forEach((part) => box.appendChild(el('span', { class: 'nowrap' }, part)));
    }).catch((e) => {
      // A plain "—" would hide the failure; the row says it failed and can be
      // retried in place (retry re-renders the settings view).
      reportUiError('error', e);
      inlineRetry(box, () => render());
    });
    return box;
  }

  /* --- storage ------------------------------------------------------ */

  // No backup row: the store keeps no file copies, and a `backups\` directory
  // left by an older build is deleted on the next launch (settings.rs
  // `remove_legacy_backups`). Pointing the user at a folder that no longer
  // exists would be worse than saying nothing.
  const storage = el('div', { class: 'card' },
    el('div', { class: 'card-head' }, el('h2', {}, t('settings.storage'))),
    el('div', { class: 'card-body' },
      el('div', { class: 'alert info' }, t('settings.writePolicy')),
      el('dl', { class: 'kv mt-12' },
        el('dt', {}, t('settings.storeRoot')), el('dd', { class: 'mono' }, S.boot.storeRoot),
        el('dt', {}, t('settings.auditLog')), el('dd', { class: 'mono' }, S.boot.storeRoot + '\\audit.jsonl')
      ),
      el('div', { class: 'mt-12' },
        el('button', { class: 'btn', onclick: () => window.ak.openStore() }, t('settings.openStore')))
    ));

  /* --- adapters ----------------------------------------------------- */
  const adapters = el('div', { class: 'card' },
    el('div', { class: 'card-head' },
      el('h2', {}, t('settings.adapters')),
      el('span', { class: 'sub spacer' }, t('settings.declared', { n: S.boot.agents.length }))),
    el('div', { class: 'card-body' },
      el('div', { class: 'muted fs-11 mb-8' }, t('settings.adaptersHint')),
      el('div', { class: 'flex flex-wrap gap-sm' },
        ...S.boot.agents.map((a) => el('span', { class: `chip ${a.installed ? 'ok' : 'ghost'}` }, a.label)))
    ));

  wrap.appendChild(el('div', { class: 'grid grid-2' }, appearance, behaviour));
  // Full width: the update card holds the version comparison and the latest
  // release notes; half a column squeezes them. The channel/proxy/token
  // controls moved behind its 更新设置 button.
  wrap.appendChild(el('div', { class: 'mt-14' }, updateCard()));
  wrap.appendChild(changelogCard());
  wrap.appendChild(el('div', { class: 'grid grid-2 mt-14' }, storage, adapters));
  return wrap;
}

/**
 * Apply a theme preference to <html>.
 *
 * localStorage wins over the persisted settings: it is what the user just
 * clicked, and settings.json only refreshes after the save round-trip. Without
 * this precedence a fresh reload re-applies the stale stored value.
 */
function applyTheme(pref) {
  const stored = localStorage.getItem('ak.theme');
  const eff = stored || pref || 'system';
  const dark = eff === 'dark'
    || (eff === 'system' && window.matchMedia
      && window.matchMedia('(prefers-color-scheme: dark)').matches);
  document.documentElement.setAttribute('data-theme', dark ? 'dark' : 'light');
}

/* ------------------------------------------------------------------ */
/* asset drawer                                                        */
/* ------------------------------------------------------------------ */

async function openAsset(id) {
  S.selected = id;
  let r;
  try {
    r = await window.ak.readAsset(id);
  } catch (e) {
    // No catch here meant a rejected invoke turned the row click into a dead
    // click: no panel, no toast, nothing.
    reportUiError('error', e);
    S.selected = null;
    return toast(t('toast.readFailed'), 'err');
  }
  if (!r.ok) return toast(r.error, 'err');
  const a = r.asset;
  const editable = a.writable && (a.kind === 'skill' || a.kind === 'rule' || a.kind === 'memory');
  const wrap = el('div', {});

  wrap.appendChild(el('div', { class: 'flex gap-sm mb-12' },
    el('span', { class: `chip ${a.kind}` }, kindName(a.kind)),
    el('span', { class: 'chip' }, zhText(a.agentLabel)),
    el('span', { class: 'chip' }, scopeName(a.scope)),
    a.enabled === false ? el('span', { class: 'chip warn' }, t('common.disabled')) : null,
    a.effective && a.effective.live === false ? el('span', { class: 'chip warn' }, t('assets.shadowedByProject')) : null,
    !a.writable ? el('span', { class: 'chip ghost' }, t('assets.readOnly')) : null
  ));

  if ((a.issues || []).length) {
    wrap.appendChild(el('div', { class: 'alert ' + (issuesOf(a) ? 'err' : 'warn') },
      ...a.issues.map((i) => el('div', {}, el('b', {}, zhField(i.field)), ' — ', zhText(i.message)))));
  }

  if (a.effective && a.effective.shadowedBy && a.effective.shadowedBy.length) {
    wrap.appendChild(el('div', { class: 'alert warn' },
      t('drawer.shadowedNotice')));
  }

  wrap.appendChild(el('dl', { class: 'kv' },
    el('dt', {}, t('drawer.path')), el('dd', { class: 'mono fs-11' }, a.file),
    el('dt', {}, t('drawer.size')), el('dd', {}, bytes(a.size) + (a.meta.estTokens ? ` ~${a.meta.estTokens} ${t('common.tokens')}` : '')),
    el('dt', {}, t('drawer.modified')), el('dd', {}, `${clock(a.mtime)} (${ago(a.mtime)})`),
    el('dt', {}, t('drawer.fingerprint')), el('dd', { class: 'mono' }, a.fingerprint),
    a.meta.relLabel ? el('dt', {}, t('drawer.declaredAs')) : null, a.meta.relLabel ? el('dd', { class: 'mono' }, a.meta.relLabel) : null,
    a.meta.version ? el('dt', {}, t('drawer.version')) : null, a.meta.version ? el('dd', {}, a.meta.version) : null,
    a.meta.transport ? el('dt', {}, t('drawer.transport')) : null, a.meta.transport ? el('dd', {}, a.meta.transport) : null,
    a.meta.health ? el('dt', {}, t('drawer.health')) : null,
    a.meta.health ? el('dd', {},
      el('span', { class: `chip ${a.meta.health.ok ? 'ok' : 'err'}` },
        a.meta.health.ok ? t('health.ok') : t('health.missing')), ' ',
      el('span', { class: 'mono fs-11' }, zhText(a.meta.health.detail))) : null,
    a.meta.envKeys && a.meta.envKeys.length ? el('dt', {}, t('drawer.envKeys')) : null,
    a.meta.envKeys && a.meta.envKeys.length ? el('dd', { class: 'mono' }, a.meta.envKeys.join(', ')) : null,
    a.meta.skillCount ? el('dt', {}, t('drawer.bundled')) : null,
    a.meta.skillCount ? el('dd', {}, t('drawer.bundledCount', { skills: a.meta.skillCount, agents: a.meta.agentCount, list: (a.meta.components || []).join(', ') })) : null
  ));

  /* actions */
  const actions = el('div', { class: 'flex flex-wrap gap-sm my-14' });
  if (a.kind === 'skill' && a.writable) {
    actions.appendChild(el('button', { class: 'btn primary', onclick: () => openSkillEditor(a) }, t('editor.edit')));
  }
  if (editable) {
    actions.appendChild(el('button', { class: 'btn', onclick: () => doDiff(a, r.content) }, t('assets.previewChanges')));
    actions.appendChild(el('button', { class: 'btn primary', onclick: () => { saveSelected(); } }, t('common.save')));
  }
  if (a.kind === 'mcp') {
    actions.appendChild(el('button', { class: 'btn primary', onclick: () => openMcpEditor(a) }, t('mcp.editServer')));
    actions.appendChild(el('button', { class: 'btn danger', onclick: () => doDelete(a) }, t('common.remove')));
  }
  // Same gate as the context menu (`showCtxMenu`): a plugin cache or a
  // host-owned file has no writable destination, and offering the button there
  // opened an empty picker.
  if (a.writable && DISTRIBUTE_KEY[a.kind]) {
    actions.appendChild(el('button', { class: 'btn', onclick: () => openDistribute(a) }, t('distribute.go') + '…'));
  }
  if (a.kind !== 'mcp' && a.writable) {
    actions.appendChild(el('button', { class: 'btn danger', onclick: () => doDelete(a) }, t('verb.delete')));
  }
  actions.appendChild(el('button', { class: 'btn ghost', onclick: () => window.ak.openPath(a.file) }, t('assets.reveal')));
  wrap.appendChild(actions);

  /* editor */
  if (editable) {
    wrap.appendChild(el('div', { class: 'sec-title' }, t('editor.content')));
    const ta = el('textarea', { class: 'editor', id: 'asset-editor' });
    ta.value = r.content;
    ta.addEventListener('input', () => { S.dirty = true; S.editorText = ta.value; });
    wrap.appendChild(ta);
    wrap.appendChild(el('div', { class: 'muted fs-11 mt-6' }, t('drawer.saveHint')));
  } else if (a.kind === 'plugin') {
    wrap.appendChild(el('div', { class: 'sec-title' }, t('drawer.nested', { n: (a.meta.nested || []).length })));
    // .tbl.static: read-only inventory, rows deliberately not clickable.
    wrap.appendChild(el('table', { class: 'tbl static' },
      el('thead', {}, el('tr', {}, el('th', {}, t('th.type')), el('th', {}, t('th.name')))),
      el('tbody', {}, (a.meta.nested || []).map((n) =>
        el('tr', {}, el('td', {}, el('span', { class: 'chip' }, n.type)), el('td', {}, n.name))))
    ));
  } else {
    wrap.appendChild(el('div', { class: 'sec-title' }, t('drawer.preview')));
    wrap.appendChild(el('div', { class: 'mono log-box' },
      (r.content || '').slice(0, 6000)));
  }

  openDrawer(a.name, wrap);
}

async function saveSelected() {
  const ta = $('#asset-editor');
  if (!ta || !S.selected) return;
  const content = ta.value;
  const d = await window.ak.diff(S.selected, content);
  if (d.ok && d.lines.length === 0) return toast(t('editor.noChanges'), 'warn');
  const asset = (S.data?.assets || []).find((a) => a.id === S.selected);
  // The diff already ran, so the dialog can name the file that changes; the
  // write itself waits for the answer.
  const ok = await confirmWrite({
    action: t('verb.update'),
    targets: [asset ? asset.file : S.selected],
    detail: t('confirm.saveAssetDetail', { name: asset ? asset.name : S.selected })
  });
  if (!ok) return;
  const r = await window.ak.saveMarkdown(S.selected, content);
  if (!r.ok) {
    if (r.blocked) {
      openModal(t('editor.blocked'),
        el('div', {}, ...r.issues.map((i) => el('div', { class: `alert ${i.level === 'error' ? 'err' : 'warn'}` }, `${zhField(i.field)}: ${zhText(i.message)}`))),
        [el('button', { class: 'btn', onclick: closeModal }, t('common.close'))]);
      return;
    }
    return reportWriteFailure(r, 'toast.writeFailed');
  }  S.dirty = false;
  toast(t('toast.savedBackup'), 'ok');
  await rescan(true);
  openAsset(S.selected);
}

async function doDelete(a) {
  const ok = await confirmDialog({
    title: t('confirm.deleteTitle', { kind: kindName(a.kind) }),
    message: t('confirm.deleteBody2', { name: a.name, agent: zhText(a.agentLabel), scope: a.scope, file: a.file }),
    confirmLabel: t('verb.delete'), danger: true
  });
  if (!ok) return;
  const r = a.kind === 'mcp' ? await window.ak.deleteMcp(a.id) : await window.ak.deleteAsset(a.id);
  if (!r.ok) return toast(r.error, 'err');
  S.checked.delete(a.id);   // keep the batch bar honest after a drawer delete
  toast(t('toast.deleted'), 'ok');
  // A delete is the one destructive action the user is most likely to walk
  // away from — it must reach them even if they were not looking at the window.
  maybeNotify('notify.deleted', { name: a.name });
  // The asset is gone; the unsaved edit can no longer be written, so the discard
  // guard must not ask about it on the way out.
  S.dirty = false;
  void closeDrawer();
  $('#scrim').classList.remove('on');
  await rescan(true);
}

function doDiff(a, current) {
  const ta = $('#asset-editor');
  const body = el('div', {});
  const preview = el('div', { class: 'diffbox diff-scroll' });
  body.appendChild(el('div', { class: 'alert info' }, t('diff.hint')));
  const run = async (content) => {
    const d = await window.ak.diff(a.id, content);
    preview.innerHTML = '';
    if (!d.ok) { preview.appendChild(el('div', { class: 'empty' }, zhError(d))); return; }
    if (!d.lines.length) { preview.appendChild(el('div', { class: 'empty' }, t('diff.none'))); return; }
    for (const l of d.lines) {
      const cls = l.t === 'add' ? 'diff-add' : l.t === 'del' ? 'diff-del' : 'diff-mod';
      preview.appendChild(el('div', { class: `diff-line ${cls}` },
        `${l.n}  ${l.t === 'add' ? '+ ' : l.t === 'del' ? '- ' : '~ '}${l.next ?? ''}`,
        l.t === 'mod' ? el('span', { class: 'diff-was' }, l.old ?? '') : null));
    }
  };
  body.appendChild(preview);
  const refresh = el('button', { class: 'btn', onclick: () => run($('#asset-editor')?.value ?? current) }, t('diff.recompute'));
  body.appendChild(el('div', { class: 'mt-10' }, refresh));
  openModal(t('diff.title', { name: a.name }), body, [el('button', { class: 'btn primary', onclick: () => { closeModal(); saveSelected(); } }, t('diff.applySave'))]);
  run(current);
}

/* ------------------------------------------------------------------ */
/* editors                                                             */
/* ------------------------------------------------------------------ */

function targetPicker(selected, kindRel = 'skills') {
  const box = el('div', { class: 'picker-box' });
  const chosen = new Map();
  for (const a of S.boot.agents) {
    if (!a.installed) continue;
    for (const scope of ['global', 'project']) {
      const rels = scope === 'global' ? (a.globals[kindRel] || []) : (a.projects[kindRel] || []);
      if (!rels.length) continue;
      const id = `${a.id}|${scope}`;
      const cb = el('input', { type: 'checkbox' });
      cb.checked = selected && selected.agentId === a.id && selected.scope === scope;
      if (cb.checked) chosen.set(id, { agentId: a.id, scope, projectId: null });
      cb.addEventListener('change', () => {
        if (cb.checked) chosen.set(id, { agentId: a.id, scope, projectId: null });
        else chosen.delete(id);
      });
      box.appendChild(el('label', { class: 'check-row' }, cb,
        el('span', {}, el('b', {}, a.label), el('span', { class: 'muted' }, `  ${scope}  ${rels.join(', ')}`))));
    }
  }
  if (!box.children.length) box.appendChild(el('div', { class: 'empty' }, t('distribute.none')));
  return { node: box, get: () => [...chosen.values()] };
}

function openSkillEditor(asset) {
  const fm = asset ? (asset.meta.frontmatter || {}) : {};
  const bodyVal = asset ? asset.raw ? asset.raw.replace(/^---[\s\S]*?\n---\n/, '') : '' : '';

  const f = {};
  /** A label that is actually attached to its control (click-to-focus works). */
  let fieldSeq = 0;
  const lab = (text, control) => {
    if (!control.id) control.id = `ed-field-${++fieldSeq}`;
    return el('label', { for: control.id }, text);
  };
  const mk = (key, label, hint, value, ph, locked) => {
    const i = el('input', { placeholder: ph || '', value: value ?? '' });
    // A skill's name is its directory name. Letting the field be edited on an
    // existing asset only produces a validation error the user cannot act on
    // ("name must match its directory"), so it is read-only with the reason
    // shown instead.
    if (locked) {
      i.readOnly = true;
      i.disabled = false;
      i.setAttribute('aria-readonly', 'true');
    }
    f[key] = i;
    return el('div', { class: 'form-row' }, lab(label, i), i,
      (hint || locked) ? el('div', { class: 'form-hint' }, locked ? t('editor.nameLocked') : hint) : null);
  };

  const body = el('textarea', { class: 'editor editor-tall' });
  body.value = bodyVal || `# ${asset ? asset.name : t('editor.newSkill')}\n\n## ${t('editor.templateWhen')}\n${t('editor.templateWhenBody')}\n\n## ${t('editor.templateSteps')}\n1. ...\n`;

  const picker = targetPicker(asset);

  const node = el('div', {},
    el('div', { class: 'alert info' }, t('editor.requiredHint')),
    el('div', { class: 'form-grid' },
      mk('name', t('editor.name'), t('editor.nameHint'), fm.name || '', 'my-skill', !!asset),
      mk('version', t('editor.version'), t('editor.versionHint'), fm.version || '1.0.0', '1.0.0')
    ),
    mk('description', t('editor.description'), t('editor.descHint'), fm.description || '', 'Generate … Use when …'),
    el('div', { class: 'form-grid' },
      mk('author', t('editor.author'), '', fm.author || ''),
      mk('license', t('editor.license'), '', fm.license || 'MIT')
    ),
    mk('allowed-tools', t('editor.allowedTools'), t('editor.toolsHint'), fm['allowed-tools'] || '', 'Read, Grep, Glob'),
    el('div', { class: 'sec-title' }, t('distribute.writeTo')),
    picker.node,
    el('div', { class: 'sec-title' }, t('editor.body')),
    body
  );

  const save = el('button', { class: 'btn primary' }, asset ? t('editor.saveChanges') : t('editor.createSkill'));
  save.addEventListener('click', async () => {
    const targets = picker.get();
    if (!targets.length) return toast(t('editor.selectTarget'), 'warn');
    // Directory-level list: the skill's folder name comes from the form and is
    // validated by the engine, so naming `<dir>\<name>` here could print a path
    // that never gets created.
    const ok = await confirmWrite({
      action: t(asset ? 'verb.update' : 'verb.create'),
      targets: targetPaths(targets, 'skills'),
      detail: t(asset ? 'confirm.skillUpdateDetail' : 'confirm.skillCreateDetail', { name: f.name.value.trim() })
    });
    if (!ok) return;
    const r = await window.ak.saveSkill({
      skillId: asset ? asset.id : null,
      data: {
        name: f.name.value.trim(),
        description: f.description.value.trim(),
        version: f.version.value.trim(),
        author: f.author.value.trim(),
        license: f.license.value.trim(),
        'allowed-tools': f['allowed-tools'].value.trim() || undefined
      },
      body: body.value,
      targets
    });
    if (!r.ok && r.blocked) {
      openModal(t('editor.blocked'), el('div', {}, ...r.issues.map((i) =>
        el('div', { class: `alert ${i.level === 'error' ? 'err' : 'warn'}` }, `${zhField(i.field)}: ${zhText(i.message)}`))),
        [el('button', { class: 'btn', onclick: closeModal }, t('common.close'))]);
      return;
    }
    if (!r.ok) { reopenCovered(); return reportWriteFailure(r, 'toast.writeFailed'); }
    closeModal();
    coveredSnap = null;
    toast(t('toast.written', { verb: t(asset ? 'verb.updated' : 'verb.created'), n: r.written.length }), 'ok');
    await rescan(true);
  });

  openModal(asset ? t('editor.editSkillTitle', { name: asset.name }) : t('editor.newSkill'), node, [el('button', { class: 'btn', onclick: closeModal }, t('common.cancel')), save]);
}

/** `KEY<sep>value` object → textarea lines (inverse of parseKv below). */
function kvLines(obj, sep) {
  return Object.entries(obj || {}).map(([k, v]) => k + sep + v).join('\n');
}

/**
 * MCP server editor.
 * A structured form, not a JSON text box. The engine already validates
 * transports (`core/validate.rs`), and a JSON textarea made the user the
 * integration's last line of defence against a typo that the validator would
 * have caught two fields earlier. The raw JSON box stays available under
 * "advanced" for configs this form cannot express (headers, custom types),
 * and whichever of the two the user touched last is what gets saved.
 */
function openMcpEditor(asset) {
  const cfg = asset ? { ...(asset.meta.config || {}) } : {};
  const nameI = el('input', { value: asset ? asset.name : '', placeholder: 'my-server' });
  const cfgI = el('textarea', { class: 'editor editor-taller' });
  cfgI.value = JSON.stringify(cfg, null, 2);
  let advancedDirty = false;
  cfgI.addEventListener('input', () => { advancedDirty = true; });

  const isHttp = !!(cfg.url || cfg.type);
  const transportSel = el('select', {});
  for (const [v, label] of [['stdio', t('editor.mcpStdio')], ['http', t('editor.mcpHttp')]]) {
    transportSel.appendChild(el('option', { value: v, selected: (v === 'http') === isHttp }, label));
  }
  const cmdI = el('input', { value: cfg.command || '', placeholder: 'npx' });
  // `value` is not a valid attribute on <textarea>: el() sets attributes, so the
  // text would never appear and saving would silently drop the existing args /
  // env / headers. Assign the property instead.
  const linesBox = (cls, text) => {
    const box = el('textarea', { class: cls });
    box.value = text;
    return box;
  };
  const argsI = linesBox('editor editor-short', (cfg.args || []).join('\n'));
  const urlI = el('input', { value: cfg.url || '', placeholder: 'https://example.com/mcp' });
  const envI = linesBox('editor editor-short', kvLines(cfg.env, '='));
  const headersI = linesBox('editor editor-short', kvLines(cfg.headers, ': '));

  /** Attach each label to its control, so clicking the text focuses the field. */
  let mcpFieldSeq = 0;
  const mcpLab = (text, control) => {
    if (!control.id) control.id = `mcp-field-${++mcpFieldSeq}`;
    return el('label', { for: control.id }, text);
  };

  const stdioBox = el('div', {},
    el('div', { class: 'form-row' }, mcpLab(t('editor.mcpCommand'), cmdI), cmdI),
    el('div', { class: 'form-row' }, mcpLab(t('editor.mcpArgs'), argsI), argsI,
      el('div', { class: 'form-hint' }, t('editor.mcpArgsHint'))));
  const httpBox = el('div', {},
    el('div', { class: 'form-row' }, mcpLab(t('editor.mcpUrl'), urlI), urlI),
    el('div', { class: 'form-row' }, mcpLab(t('editor.mcpHeaders'), headersI), headersI,
      el('div', { class: 'form-hint' }, t('editor.mcpHeadersHint'))));

  /** Which transport section is on screen — a display toggle, not a re-render. */
  function syncTransport() {
    const showHttp = transportSel.value === 'http';
    const form = stdioBox.parentNode;
    if (showHttp) {
      if (form.contains(stdioBox)) { form.replaceChild(httpBox, stdioBox); }
    } else if (form.contains(httpBox)) {
      form.replaceChild(stdioBox, httpBox);
    }
  }
  transportSel.addEventListener('change', syncTransport);

  /** `KEY<sep>value` lines → object; blank lines and `#` comments ignored. */
  function parseKv(text, sep) {
    const out = {};
    for (const line of String(text || '').split('\n')) {
      const trimmed = line.trim();
      if (!trimmed || trimmed.startsWith('#')) continue;
      const at = trimmed.indexOf(sep);
      if (at <= 0) continue;
      out[trimmed.slice(0, at).trim()] = trimmed.slice(at + sep.length).trim();
    }
    return out;
  }

  /**
   * Build the config to persist: the raw box when the user edited it,
   * otherwise the structured fields. Keys the form does not model are carried
   * over from the original config so an edit never silently drops them.
   */
  function buildConfig() {
    if (advancedDirty) {
      try { return JSON.parse(cfgI.value); } catch (e) { return { error: e.message }; }
    }
    const out = {};
    if (transportSel.value === 'http') {
      out.url = urlI.value.trim();
    } else {
      out.command = cmdI.value.trim();
      const args = argsI.value.split('\n').map((s) => s.trim()).filter(Boolean);
      if (args.length) out.args = args;
    }
    const env = parseKv(envI.value, '=');
    if (Object.keys(env).length) out.env = env;
    const headers = parseKv(headersI.value, ':');
    if (Object.keys(headers).length) out.headers = headers;
    for (const [k, v] of Object.entries(cfg)) {
      if (k === 'command' || k === 'args' || k === 'env' || k === 'url' || k === 'headers') continue;
      if (!(k in out)) out[k] = v;
    }
    return out;
  }

  const agentSel = el('select', {});
  for (const a of S.boot.agents) {
    if (!a.installed) continue;
    if (!(a.globals.mcp || []).length) continue;
    agentSel.appendChild(el('option', { value: a.id, selected: asset && asset.agentId === a.id }, `${a.label} (${a.globals.mcp.filter((x) => /\.(json|toml|yaml)$/.test(x)).join(', ') || 'settings'})`));
  }
  const scopeSel = el('select', {});
  for (const [v, l] of [['global', t('assets.scopeGlobal')], ['project', t('assets.scopeProject')]]) scopeSel.appendChild(el('option', { value: v }, l));

  const form = el('div', {},
    stdioBox,
    httpBox);
  syncTransport();

  const node = el('div', {},
    el('div', { class: 'alert info' }, t('mcp.siblingNote')),
    el('div', { class: 'form-row' }, mcpLab(t('editor.serverName'), nameI), nameI),
    el('div', { class: 'form-grid' },
      el('div', { class: 'form-row' }, mcpLab(t('editor.mcpTransport'), transportSel), transportSel),
      el('div', { class: 'form-row' }, mcpLab(t('th.scope'), scopeSel), scopeSel)),
    el('div', { class: 'form-row' },
      mcpLab(t('th.agent'), agentSel), agentSel),
    form,
    el('div', { class: 'form-row' },
      mcpLab(t('editor.mcpEnv'), envI), envI,
      el('div', { class: 'form-hint' }, t('editor.mcpEnvHint'))),
    el('details', { class: 'mt-12' },
      el('summary', { class: 'muted fs-11' }, t('editor.mcpAdvanced')),
      el('div', { class: 'mt-6' }, cfgI,
        el('div', { class: 'form-hint' }, t('editor.mcpAdvancedHint'))))
  );

  const save = el('button', { class: 'btn primary' }, asset ? t('common.save') : t('editor.addServer'));
  save.addEventListener('click', async () => {
    const parsed = buildConfig();
    if (parsed.error) return toast(t('err.badJson', { msg: parsed.error }), 'err');
    if (!parsed.command && !parsed.url) return toast(t('editor.mcpNeedTransport'), 'err');
    // The engine merges the server into the agent's MCP config file; the
    // adapter declares which file that is, so it is resolved the same way the
    // distribution dialog resolves its directories.
    const ok = await confirmWrite({
      action: t(asset ? 'verb.update' : 'verb.create'),
      targets: asset && asset.file
        ? [asset.file]
        : targetPaths([{ agentId: agentSel.value, scope: scopeSel.value, projectId: null }], 'mcp'),
      detail: t(asset ? 'confirm.mcpUpdateDetail' : 'confirm.mcpCreateDetail', { name: nameI.value.trim() })
    });
    if (!ok) return;
    const r = await window.ak.upsertMcp({
      name: nameI.value.trim(), config: parsed,
      agentId: agentSel.value, scope: scopeSel.value, projectId: null,
      assetId: asset ? asset.id : null
    });
    if (!r.ok && r.blocked) {
      openModal(t('editor.blocked'), el('div', {}, ...r.issues.map((i) =>
        el('div', { class: `alert ${i.level === 'error' ? 'err' : 'warn'}` }, `${zhField(i.field)}: ${zhText(i.message)}`))),
        [el('button', { class: 'btn', onclick: closeModal }, t('common.close'))]);
      return;
    }
    if (!r.ok) { reopenCovered(); return reportWriteFailure(r, 'toast.writeFailed'); }
    closeModal();
    coveredSnap = null;
    toast(t('toast.mcpSaved'), 'ok');
    await rescan(true);
  });

  openModal(asset ? t('editor.editMcpTitle', { name: asset.name }) : t('editor.addMcp'), node, [el('button', { class: 'btn', onclick: closeModal }, t('common.cancel')), save]);
}

/* ---------------- batch operations ---------------- */
/**
 * Batch delete. Each asset goes through the normal delete path (its own
 * backup + transaction), because one giant transaction across heterogeneous
 * agents would be harder to reason about than N small reversible ones.
 * Read-only assets (cached plugins) are skipped with a note.
 */
/**
 * Mark the batch bar busy for the duration of a serial loop.
 *
 * The bar was the only feedback a long multi-asset write had, and it stayed
 * clickable: a second click started a second loop over the same assets. Now the
 * buttons are disabled and the count becomes progress (aria-busy tells AT).
 */
function batchBusy() {
  const bar = document.querySelector('.batchbar');
  if (!bar) return { update() {}, done() {} };
  const buttons = [...bar.querySelectorAll('button')];
  const count = bar.querySelector('.count');
  const before = count ? count.textContent : '';
  bar.setAttribute('aria-busy', 'true');
  for (const b of buttons) b.disabled = true;
  return {
    update(n, total) { if (count) count.textContent = t('batch.progress', { n, total }); },
    done() {
      bar.removeAttribute('aria-busy');
      for (const b of buttons) b.disabled = false;
      if (count) count.textContent = before;
    }
  };
}

async function doBatchDelete(selected) {
  const deletable = selected.filter((a) => a.writable);
  const skipped = selected.length - deletable.length;
  if (!deletable.length) return toast(t('batch.noWritable'), 'warn');
  const ok = await confirmDialog({
    title: t('confirm.batchDeleteTitle'),
    message: t('confirm.batchDeleteBody', { n: deletable.length }),
    confirmLabel: t('verb.delete'), danger: true
  });
  if (!ok) return;
  const busy = batchBusy();
  let done = 0, failed = 0, lastTx = null;
  try {
    for (const a of deletable) {
      busy.update(done + failed + 1, deletable.length);
      try {
        const r = a.kind === 'mcp' ? await window.ak.deleteMcp(a.id) : await window.ak.deleteAsset(a.id);
        if (r.ok) { done++; lastTx = r.txId; } else failed++;
      } catch (e) { failed++; }
    }
  } finally {
    busy.done();
  }
  S.checked.clear();
  if (skipped) toast(t('batch.noWritable'), 'warn');
  toast(failed
    ? t('toast.batchDoneFailed', { n: done, failed })
    : t('toast.batchDone', { n: done }), failed ? 'warn' : 'ok');
  if (done) maybeNotify('notify.applied', { n: done });
  await rescan(true);
  void lastTx;
}

/**
 * Batch distribute: one destination picker for the whole selection. Only
 * skills/rules/memories participate — plugins are read-only and MCP configs
 * have their own per-server editor.
 */
function openBatchDistribute(selected) {
  const movable = selected.filter((a) => ['skill', 'rule', 'memory'].includes(a.kind));
  if (!movable.length) return toast(t('batch.noWritable'), 'warn');

  const key = 'skill';  // destination shape is the same for skill/rule/memory dirs
  const box = el('div', { class: 'picker-box' });
  const chosen = [];
  for (const a of S.boot.agents) {
    if (!a.installed) continue;
    const rels = a.globals[key] || [];
    if (!rels.length) continue;
    const cb = el('input', { type: 'checkbox' });
    cb.addEventListener('change', () => { if (cb.checked) chosen.push({ agentId: a.id, scope: 'global' }); });
    box.appendChild(el('label', { class: 'check-row' }, cb,
      el('span', {}, el('b', {}, a.label), el('span', { class: 'muted' }, `  ${rels.join(', ')}`))));
  }

  const node = el('div', {},
    el('div', { class: 'alert info' }, t('batch.distributeNote')),
    el('div', { class: 'muted fs-11 mb-8' }, movable.map((a) => a.name).join(' · ')),
    box);

  const go = el('button', { class: 'btn primary' }, t('batch.distribute'));
  go.addEventListener('click', async () => {
    if (!chosen.length) return toast(t('distribute.none'), 'warn');
    const ok = await confirmWrite({
      action: t('verb.distribute'),
      targets: targetPaths(chosen, key, true),
      detail: t('confirm.batchDistributeDetail', { assets: movable.length, targets: chosen.length })
    });
    if (!ok) return;
    let written = 0, failed = 0, lastTx = null;
    const total = movable.length;
    // Same reason as the delete loop: this is N sequential IPC writes with the
    // modal still up, so the button must show progress and refuse re-entry.
    go.disabled = true;
    try {
      for (const [i, a] of movable.entries()) {
        go.textContent = t('batch.progress', { n: i + 1, total });
        try {
          const r = await window.ak.distribute(a.id, chosen);
          if (r.ok) { written += r.written.length; lastTx = r.txId; } else failed++;
        } catch (e) { failed++; }
      }
    } finally {
      go.disabled = false;
      go.textContent = t('batch.distribute');
    }
    closeModal();
    coveredSnap = null;
    S.checked.clear();
    if (written) toast(t('toast.batchDistributed', { n: written }), 'ok');
    if (failed) toast(t('toast.someFailed', { n: failed }), 'err');
    await rescan(true);
    void lastTx;
  });

  openModal(t('batch.distributeTitle', { n: movable.length }), node,
    [el('button', { class: 'btn', onclick: closeModal }, t('common.cancel')), go]);
}

/* ---------------- bundle import / export ---------------- */

async function doExportBundle(selected) {
  const ids = selected.map((a) => a.id);
  const r = await window.ak.exportBundle(ids);
  if (!r.ok) return toast(r.canceled ? t('toast.pickCancelled') : r.error, 'warn');
  toast(t('bundle.exported', { n: r.count, out: r.out }), 'ok');
}

async function doImportBundle() {
  // The picker and the write are one IPC call, so the only place to ask is
  // before it: the manifest decides which agent each asset lands in, and those
  // are always the installed agents' own directories.
  const homes = [...new Set(S.boot.agents.filter((a) => a.installed).map((a) => a.home))];
  const ok = await confirmWrite({
    action: t('verb.import'),
    targets: homes,
    detail: t('confirm.importDetail')
  });
  if (!ok) return;
  const r = await window.ak.importBundle();
  if (!r.ok) return toast(r.canceled ? t('toast.pickCancelled') : r.error, 'warn');
  toast(t('bundle.imported', { n: r.imported.length }), 'ok');
  await rescan(true);
}

/* ---------------- context menu ---------------- */

function hideCtxMenu() {
  const m = $('#ctxmenu');
  if (m) m.classList.remove('on');
}

/** Right-click menu for an asset row. Items that do not apply are omitted. */
function showCtxMenu(e, a) {
  e.preventDefault();
  const m = $('#ctxmenu');
  if (!m) return;
  m.innerHTML = '';

  const item = (label, fn, cls = '') => {
    const b = el('button', { class: cls }, label);
    b.addEventListener('click', () => { hideCtxMenu(); fn(); });
    m.appendChild(b);
  };

  item(t('ctx.view'), () => openAsset(a.id));
  if (a.kind === 'skill') item(t('ctx.edit'), () => openAsset(a.id));
  if (a.writable && ['skill', 'rule', 'memory', 'mcp'].includes(a.kind)) {
    item(t('ctx.distribute'), () => openDistribute(a));
  }
  m.appendChild(el('hr'));
  item(t('ctx.copyPath'), async () => {
    try { await navigator.clipboard.writeText(a.file); toast(t('toast.copiedPath'), 'ok'); }
    catch (err) { toast(a.file, 'info'); }
  });
  item(t('ctx.reveal'), () => window.ak.openPath(a.file).catch(() => {}));
  if (a.writable) {
    m.appendChild(el('hr'));
    item(t('ctx.delete'), () => doDelete(a), 'danger');
  }

  // Measure first (menu is display:none while we position it), then reveal —
  // showing before measuring produced a one-frame flash at 0,0.
  m.classList.add('on');
  const r = m.getBoundingClientRect();
  m.classList.remove('on');
  m.style.left = Math.min(e.clientX, window.innerWidth - r.width - 8) + 'px';
  m.style.top = Math.min(e.clientY, window.innerHeight - r.height - 8) + 'px';
  m.classList.add('on');
}

/**
 * Adapter directory list that holds each distributable kind.
 *
 * `targetPicker` above has always used the plural list names; this picker used
 * the singular kind (`a.globals['skill']`), which does not exist in the
 * bootstrap payload — every destination was filtered out and the dialog opened
 * empty. Keep the mapping in one place so the two pickers cannot drift again.
 */
const DISTRIBUTE_KEY = { skill: 'skills', rule: 'rules', memory: 'memory', mcp: 'mcp' };

function openDistribute(asset) {
  const key = DISTRIBUTE_KEY[asset.kind] || 'skills';
  const box = el('div', { class: 'picker-box' });
  const chosen = new Map();

  const addTarget = (id, target, label, rels) => {
    const cb = el('input', { type: 'checkbox', 'aria-label': label });
    cb.addEventListener('change', () => {
      if (cb.checked) chosen.set(id, target); else chosen.delete(id);
    });
    box.appendChild(el('label', { class: 'check-row' }, cb,
      el('span', {}, el('b', {}, label), el('span', { class: 'muted' }, `  ${rels.join(', ')}`))));
  };

  for (const a of S.boot.agents) {
    if (!a.installed) continue;
    const rels = a.globals[key] || [];
    if (!rels.length) continue;
    addTarget(`${a.id}|global`, { agentId: a.id, scope: 'global', projectId: null }, a.label, rels);
  }
  // A project destination is (registered project × installed agent): the engine
  // resolves it as registry[projectId] + that agent's project-relative dir, so
  // the agent half must come from the clicked row, never a fixed id.
  for (const p of (S.data?.projects || [])) {
    for (const a of S.boot.agents) {
      if (!a.installed) continue;
      const rels = a.projects[key] || [];
      if (!rels.length) continue;
      addTarget(`${a.id}|${p.id}`, { agentId: a.id, scope: 'project', projectId: p.id },
        `${a.label} · ${p.name}`, rels);
    }
  }
  if (!box.children.length) box.appendChild(el('div', { class: 'empty' }, t('distribute.none')));

  const node = el('div', {},
    el('div', { class: 'alert info' }, t('distribute.writeNote')),
    box);

  const go = el('button', { class: 'btn primary' }, t('distribute.go'));
  go.addEventListener('click', async () => {
    if (!chosen.size) return toast(t('distribute.selectDest'), 'warn');
    const picks = [...chosen.values()];
    const ok = await confirmWrite({
      action: t('verb.distribute'),
      targets: targetPaths(picks, key, true),
      detail: t('confirm.distributeDetail', { name: asset.name, n: picks.length })
    });
    if (!ok) return;
    const r = await window.ak.distribute(asset.id, picks);
    closeModal();
    coveredSnap = null;
    if (!r.ok) return reportWriteFailure(r, 'toast.distributeFailed');
    toast(t('toast.distributed', { n: r.written.length }), 'ok');
    for (const s of r.skipped || []) toast(`${s.target.agentId}: ${zhText(s.reason)}`, 'warn');
    await rescan(true);
  });

  openModal(t('distribute.title', { name: asset.name }), node, [el('button', { class: 'btn', onclick: closeModal }, t('common.cancel')), go]);
}

/* ------------------------------------------------------------------ */
/* startup                                                             */
/* ------------------------------------------------------------------ */

/**
 * Paint the startup failure into the main area.
 *
 * Kept as a plain DOM paint (not render()) because boot() failed before there
 * is any data to render. The retry button re-runs boot() — bootstrap + scan —
 * so a transient IPC failure no longer costs the user an application restart.
 */
function showStartupError(e) {
  const root = document.getElementById('view-root');
  if (!root) return;
  if (!window.__akI18n) {
    // No-degradation policy, last resort: even the dictionary failed to load,
    // so t() itself would throw. Plain Chinese, escaped, no retry (there is no
    // reliable translator to build the button label with).
    root.innerHTML = `<div class="alert err">启动失败：${esc(zhText(e && e.message))}</div>`;
    return;
  }
  root.innerHTML = '';
  root.appendChild(errorPanel(t('err.startupFailed', { msg: zhError(e) }), retryBoot));
}

/** Retry target of the startup panel; reports a repeat failure the same way. */
async function retryBoot() {
  try {
    await boot();
  } catch (e) {
    reportUiError('startup', e);
    showStartupError(e);
  }
}

function startBoot() {
  boot().catch((e) => {
    reportUiError('startup', e);
    showStartupError(e);
  });
}

/* ------------------------------------------------------------------ */

// Stamp the drawer's inert branch before the first boot can fail: a probe that
// inspects a window stuck on a startup error still has to be able to tell
// native from fallback. boot() repeats this, and setDrawerInert() is idempotent.
setDrawerInert(true);
wireErrorReporting();
startBoot();
