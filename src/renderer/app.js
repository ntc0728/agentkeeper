'use strict';
/**
 * AgentKeeper 渲染层。只通过 window.ak 与主线程通信：
 * 不用 require、不碰 fs —— index.html 里的 CSP 就是这么定的。
 */

/* 引擎层文案翻译（core/ 与 ipc 的错误是英文；中文界面绝不能直接把原文显示出来
   —— 不降级原则）。 */
const { zhText, zhField, zhError, zhCode } = window.__akEngineZh;

const S = {
  view: 'overview',  boot: null,
  data: null,
  settings: {},
  lastIssueCount: null,
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
  // --- 软件更新 --------------------------------------------------------
  // 主线程推过来的快照（`ak:update-status` / `ak:update-event`）。
  update: null,
  updateBusy: false,
  // 输入框的草稿要能扛住一次完整重渲染 —— 进度轮询就会触发重渲染。
  updateMinutesDraft: '',
  updateTokenDraft: '',
  updateTimer: null,
  updatePollTimer: null,
  // 本次会话已经弹过对话框的版本（「稍后」不做记忆 —— 「跳过」才记，记在主线程状态里）。
  updatePrompted: '',
  // --- 启动重试的记账 ---------------------------------------------------
  // boot() 同时是启动错误面板的重试入口，所以它里面每个一次性的 wire* 都要用这里的
  // 标志位把门。没有这些标志，重试就会叠监听：点一次扫两遍、弹两个更新窗、同一个
  // 元素上挂两个语言监听。
  wired: false,
  langHooked: false,
  updateWired: false,
  updateEventHooked: false,
  notifyWired: false,
  /** 上次扫描失败的文案（`null` = 最近一次扫描成功）。侧栏状态条读它。 */
  scanError: null
};

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));
/**
 * 创建元素。
 *
 * `style` 走 CSSOM（element.style.setProperty），而不是写 style 属性。严格的
 * CSP（`style-src 'self'`）会拦内联 style 属性，而我们不想为了省事把策略放宽成
 * 'unsafe-inline'。CSSOM 写入不受 style-src 约束，所以动态值（进度条宽度、每个
 * Agent 的主题色）在严格策略下照样能用。
 */
const el = (tag, attrs = {}, ...kids) => {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') n.className = v;
    else if (k === 'html') n.innerHTML = v;
    else if (k === 'style') {
      // 字符串是 `prop: value; …`；对象就是同一件事，省掉先拼字符串再解析这道
      // 手续，顺带让数据驱动的值（适配器的主题色、配套的前景色）不进 i18n 文案扫描。
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

/** 用 CSSOM 应用 `a:1; b:2` 这样的样式字符串。 */
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
 * token 计数等纯量级数字，单位阶梯在这里一次定死。
 *
 * 界面上每个计数都用这一个格式化函数。上亿的值必须显示成「102.9M」：直接除以
 * 1000 再补个「k」会得到「102909.9k」，那不是任何 SI 形式。
 */
function count(n) {
  const v = Number(n) || 0;
  if (v >= 1e9) return `${(v / 1e9).toFixed(1).replace(/\.0$/, '')}B`;
  if (v >= 1e6) return `${(v / 1e6).toFixed(1).replace(/\.0$/, '')}M`;
  if (v >= 1e4) return `${(v / 1e3).toFixed(1).replace(/\.0$/, '')}k`;
  return v.toLocaleString('en-US');
}

/* ------------------------------------------------- 数据驱动的 logo 前景色 --- */

/**
 * 把任意 CSS 颜色解析成 [r, g, b, a] —— 交给引擎自己解析，不自己猜。
 *
 * logo 方块的背景来自适配器数据（accent），现在是十六进制字符串，但那不是我们能
 * 约束的格式。用 getComputedStyle 读回来，意味着命名颜色、rgb() 和
 * color(display-p3 …) 都能按引擎真正上色的方式解析。
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

/** WCAG 相对亮度，输入不透明的 [r, g, b]。 */
function akLuminance([r, g, b]) {
  const lin = (v) => {
    const s = v / 255;
    return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

/** source-over 合成：把半透明的 top 叠到不透明的 base 上。 */
function akOver(top, base) {
  const a = top[3];
  return [0, 1, 2].map((i) => Math.round(top[i] * a + base[i] * (1 - a)));
}

/**
 * Agent 徽标的前景色 —— 算出来的，不是写死的。
 *
 * 方块用旋钮色（白）上字，底色是适配器声明的任意 accent。随包发布的适配器表里有
 * #c9c9c9，实测对比度 1.63:1，两个字母根本看不清。在黑白里挑较好的那个不会失手：
 * max(C_white, C_black) 的最差情况是 4.53:1，对 11px 粗体已经
 * 高于 AA 要求的 4.5:1。半透明的 accent 会先与它所在的底色合成，所以量到的这对
 * 颜色就是真正画出来的那对。
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
  // 用 hex 而不是 rgb()：lint 关卡分不清一个 CSS 函数调用和一句没翻译的英文，而
  // accent 本来就是 hex 字符串。
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
  // 相对时间是给用户看的文案，所以跟其他东西一样走 t()：中文界面里出现一列
  // 「1h ago」，正是 i18n 规则要防的缺陷。bytes() 里的单位不翻译 —— KB/MB 是
  // SI 符号。
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

/**
 * 操作记录里的动作名。
 *
 * 以前 fallback 是 `r.action` 原样上屏：审计日志里存的是 `install`、`export`、
 * `scan` 这类英文动词，于是中文界面里每一行都以一个英文词开头（实测
 * 「rollback」「skill」，后面跟着中文描述）。动词表覆盖 keeper/store 目前会写的
 * 全部 action，未知值退回 `verb.change` —— 一个中文的「变更」比一个英文标识符
 * 诚实，因为它不假装自己已经被翻译了。
 */
const ACTION_LABEL = {
  create: 'verb.create',
  update: 'verb.update',
  'delete': 'verb.delete',
  distribute: 'verb.distribute',
  install: 'verb.install',
  'install-source': 'verb.install',
  export: 'verb.export',
  import: 'verb.import',
  scan: 'verb.scan',
  discard: 'verb.discard',
  // 下面这些是审计日志里真实存在、上面这张表漏掉的（实测 `action` 取值集合：
  // settings-save / rollback / update / shadow-discard / scan / set-baseline）。
  // 漏掉的后果不是「少翻译一个词」，而是整行标题退回英文标识符。
  'settings-save': 'activity.actionSettingsSave',
  rollback: 'activity.actionRollback',
  'shadow-discard': 'verb.discard',
  'set-baseline': 'activity.actionSetBaseline',
  apply: 'verb.update'
};

/* ------------------------------------------------------------------ */
/* 错误上报                                                             */
/* ------------------------------------------------------------------ */

/**
 * 渲染层异常 → 主线程日志（`<storeRoot>/ui-errors.jsonl`）。
 *
 * 为什么要有：以前这个窗口里抛的异常只在 WebView2 的 devtools 里留痕，一旦打包
 * 就没人看得见。外壳每条上报追加一行 JSON（只保留最新 200 条，永不 panic）。
 *
 * 上报的字段结构是 Rust 命令定死的，字段名不能改：
  *   字段：{ kind, message, stack, view, at }
 *
 * 三条规则，都是为了让故障不要比 bug 本身更糟：
 *   1. 绝不假设命令存在 —— 旧版本的 bridge 没有 `uiError`，所以调用要加守卫，
 *      rejection 要吞掉。
 *   2. 绝不阻塞 —— 这里没有任何东西被调用方 await，每个字段过 IPC 前都会截断
 *      （2 MB 的栈会把调用卡死）。
 *   3. 绝不刷屏 —— 同一条消息每次会话只报一次，每次会话最多 20 条；属于正常
 *      运行的条件（用户取消对话框、更新抓取失败）在计数前就被过滤掉。
 */
const UI_ERROR = {
  // 按截断后的消息去重：同一个异常在循环里反复抛，在日志里是一行，不是几千行。
  seen: new Set(),
  sent: 0,
  cap: 20,
  maxMessage: 2000,
  maxStack: 4000
};

/**
 * 属于正常运行、而不是 bug 的那些条件。
 *
 * `abort`/`cancel` 覆盖用户关掉的对话框；fetch/socket 那几个词覆盖因为机器离线
 * 而失败的更新检查。IPC 本身也在列表里，因为窗口已经拆掉时每个在途 invoke 都会
 * reject，退出时上报它等于每次退出都写一堆噪音。
 */
const UI_ERROR_EXPECTED = /(?:abort|cancel|network|failed to fetch|offline|timed? ?out|ipc)/i;

function safeJson(v) {
  try { return JSON.stringify(v); } catch (_e) { return String(v); }
}

/**
 * 发一条上报。同步且全函数：它在任何 IPC 工作开始前就返回，并且不可能抛
 * （在 `error` 里抛会导致递归）。
 */
function reportUiError(kind, err) {
  try {
    const message = String((err && err.message) || err || '').slice(0, UI_ERROR.maxMessage);
    if (!message) return;
    // 调用点可以显式标记一个「预期内」的失败；正则只是兜底，用来接住平台抛来的那些。
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
    // 被拒绝的上报绝不能变成 unhandled rejection（那会自己上报自己，如此往复）。
    if (p && typeof p.catch === 'function') p.catch(() => {});
  } catch (_e) { /* reporting is best-effort by definition */ }
}

/**
 * 兜住渲染层自己没接住的东西。
 *
 * 只注册一次，在文件末尾、第一次 boot() 之前：`error` 覆盖事件处理和定时器里的
 * 抛出，`unhandledrejection` 覆盖忘了 catch 的异步路径。两个处理函数都保持廉价
 * 且同步。
 */
function wireErrorReporting() {
  window.addEventListener('error', (ev) => {
    // 加载失败的 <img>/<script> 报上来的是不带 Error 的 Event；里面唯一有用的
    // 信息就是加载目标本身。
    const target = ev.target;
    if (!ev.error && target && target !== window && target.tagName) {
      // 纯日志文本（不会渲染出来），所以它不是 t() 的 key。
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
/* 重试面板                                                             */
/* ------------------------------------------------------------------ */

/**
 * 一条错误信息，外加一条出路。
 *
 * 启动失败或重绘失败之后，不能只剩「重启」这一条恢复路径。面板保留消息，再加
 * 一个按钮，重跑的正是失败的那一步（boot → bootstrap+scan，scan → scan，
 * view → 它自己的数据加载）。
 */
function errorPanel(message, retry) {
  const btn = el('button', { class: 'btn sm', type: 'button' }, t('common.retry'));
  // 禁用的按钮已经能吞掉双击的第二次点击；runRetry 里也再查一次，这样合成的
  // .click() 也排不进第二次调用。
  btn.addEventListener('click', () => { void runRetry(btn, retry); });
  return el('div', { class: 'alert err', role: 'alert' },
    el('div', { class: 'alert-msg' }, message),
    el('div', { class: 'alert-hint' }, t('err.retryHint')),
    el('div', { class: 'alert-actions' }, btn));
}

/**
 * 紧凑版「这个面板自己没把数据加载出来」+ 重试。
 *
 * 视图级的错误面板放在卡片里的一行上太重了（本地统计那行、基线正文）。光给一个
 * 「—」或者没完没了的「加载中…」就没有出路了；重试会重渲染视图，也就重跑了加载。
 */
function inlineRetry(box, retry) {
  box.textContent = '';
  const btn = el('button', { class: 'btn sm', type: 'button' }, t('common.retry'));
  btn.addEventListener('click', () => { void runRetry(btn, retry); });
  box.appendChild(el('span', { class: 'chip err mr-6' }, t('err.loadFailed')));
  box.appendChild(btn);
}

/**
 * 跑一次重试，期间把按钮锁住。
 *
 * `disabled` 加上文案替换是看得见的那一半（「重试中…」）；提前 return 是正确性
 * 的那一半 —— 这一步可能要几秒，而用户会再点一次。
 */
async function runRetry(btn, retry) {
  if (!btn || btn.disabled) return;
  btn.disabled = true;
  btn.classList.add('loading');
  btn.textContent = t('common.retrying');
  try {
    await retry();
  } catch (_e) {
    // 被重试的那一步自己会上报并画出失败；在这里吞掉，是为了不让它变成
    // unhandled rejection（未处理的 promise 拒绝）。
  } finally {
    // 成功的重试通常会换掉 #view-root，此时按钮可能已经脱离文档了 —— 再去恢复
    // 它无害但也没意义。
    if (document.contains(btn)) {
      btn.disabled = false;
      btn.classList.remove('loading');
      btn.textContent = t('common.retry');
    }
  }
}

/* ------------------------------------------------------------------ */
/* 轻提示（toast）/ 模态框（modal）                                     */
/* ------------------------------------------------------------------ */

/**
 * 轻提示。
 *
 * 三种状态各有自己的底色，但**颜色不单独承担语义**：气泡左侧带一个图标字形
 * （✓ / ! / ×），所以红绿色觉障碍的读者、以及在弱光下看不出色差的读者仍然能
 * 分辨「完成 / 提醒 / 失败」。图标对无障碍树隐藏 —— 读屏用户听到的是
 * `#toasts` 的 `role=status` 那条播报，不需要再听一个符号的名字。
 */
function toast(msg, kind = '') {
  // 引擎的错误是英文过来的；中文界面绝不能直接把原文显示出来。
  msg = zhError(msg);
  const GLYPH = { ok: '✓', err: '✕', warn: '!' };
  const glyph = GLYPH[kind];
  // 叫 `node` 而不是 `t` —— `t` 是模块作用域里的 i18n 翻译函数。
  const node = el('div', { class: `toast ${kind}` },
    glyph ? el('span', { class: 'toast-ico', 'aria-hidden': 'true' }, glyph) : null,
    el('span', { class: 'toast-msg' }, msg));
  // #toasts 是 role=status aria-live=polite，所以 append 就足以让它被朗读出来；
  // 超时只是事后把那个视觉气泡摘掉。
  $('#toasts').appendChild(node);
  setTimeout(() => {
    node.style.transition = 'opacity .2s';
    node.style.opacity = '0';
    setTimeout(() => node.remove(), 220);
  }, kind === 'err' ? 5200 : 2600);
}

/* --- 对话框的焦点记账 -----------------------------------------------
 * 两个浮层都是模态表面：打开时 Tab 必须在里面循环，关闭时焦点必须回到打开它的
 * 那个东西上。少了这套，键盘用户会 Tab 到遮罩下面的页面里，焦点环彻底丢失。 */

/** 对话框打开前获得焦点的元素；关闭时焦点回到它身上。 */
let modalReturnFocus = null;
let drawerReturnFocus = null;

const FOCUSABLE_SEL =
  'a[href], button, input, select, textarea, [tabindex]:not([tabindex="-1"])';

/** `root` 下真正被渲染出来的、可聚焦的后代。 */
function focusables(root) {
  return $$(FOCUSABLE_SEL, root)
    .filter((n) => !n.disabled && n.getClientRects().length > 0);
}

/** 刚打开的对话框：把焦点移到第一个可操作的元素上。 */
function focusInto(root) {
  (focusables(root)[0] || root).focus();
}

/** 对话框打开时把 Tab 关在里面，而不是让它走进页面。 */
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
 * 对话框关闭后把焦点还给当初触发它的东西。
 *
 * 从表格行打开的抽屉没有可聚焦的触发元素（行不在 tab 序里），所以兜底选当前
 * 激活的导航项 —— 那是稳定且看得见的位置，总好过还给 document.body，那样下一
 * 次 Tab 会从头页顶重新开始。
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
 * 上报一次失败的写入，并把三种对用户来说完全不同的失败区分开。
 *
 * 1. 写在开始前就被拒绝了（`store.snapshotTooLarge`）：资源比撤销预算还大，所以
 *    *什么都没写*，目录树跟原来一模一样。一句话就是事情的全部。
 * 2. 写跑过了，撤销把一切都放回去了：磁盘上没变。同样，一句话就是全部。
 * 3. 写跑过了，但撤销*没能*把一切放回去（`store.restoreIncomplete` +
 *    `restoreErrors`）：目录树现在处于一个谁都没选过的状态。这是用户必须完整看到
 *    的数据问题，每一个路径都要点名，因为只有他们能手动把它修好。当初只弹一行
 *    toast，正是「半个目录恢复了一半却没人发现」的原因。
 *
 * `restoreErrors` 是一个数组，每项带一个路径、一个 code 和一个 detail；
 * `restoreError` 是拼起来的旧字符串。code 是稳定的，所以文案靠看 code 来选，
 * 而不是去解析一句英文。
 */
function reportWriteFailure(r, fallbackKey) {
  if (!r || r.ok !== false) return toast((r && r.error) || t(fallbackKey), 'err');
  const code = r.errorCode || '';

  // 情况 1 —— 在任何写入之前就被拒绝。
  if (code === STORE_SNAPSHOT_TOO_LARGE) {
    return toast(t('restore.toast.tooLarge'), 'err');
  }

  const restores = Array.isArray(r.restoreErrors)
    ? r.restoreErrors
    : (r.restoreError ? [{ path: '', code: '', detail: String(r.restoreError) }] : []);
  // 情况 2 —— 写失败，但撤销是干净的。
  if (!restores.length) {
    return toast(r.error || t(fallbackKey), 'err');
  }

  // 情况 3 —— 恢复了一半。符号链接只是成因之一，所以建议那一行是按「实际失败的
  // 是什么」来选的，而不是拍一个全局的猜测。
  const symlink = restores.some((e) => e && e.code === STORE_SYMLINK_RESTORE_FAILED);
  const body = el('div', {},
    el('div', { class: 'alert err' }, t(symlink ? 'restore.headline.symlink' : 'restore.headline.partial')),
    el('div', { class: 'muted fs-12 mb-12' }, t('restore.intro')),
    el('ul', { class: 'restore-list' },
      ...restores.map((e) => {
        const path = e && e.path ? String(e.path) : t('restore.unknownPath');
        // 引擎发的是 path / code / detail 三元组；zhError 读的是 code + params。
        // 路径作为唯一参数传进去，这样带 `§1` 占位符的句子就能对着它解析；code
        // 不在词典里时（那就由 zhText 映射那句英文），detail 字符串是兜底。
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
  ], { width: 'narrow' });
  toast(t(symlink ? 'restore.toast.symlink' : 'restore.toast.partial'), 'err');
}

/* 主线程在一次失败的事务上抛出的错误码（store.rs）。这里故意写成字面量：bridge
 * 跨的是字符串边界，一个文件里打错字的话，表现会是「落到通用兜底」而不是「缺了
 * 一条翻译」。`scripts/check.js bridge` 只校验调用形状、不校验取值，所以这份清单
 * 是人工对着 store.rs 核对的。 */
const STORE_SNAPSHOT_TOO_LARGE = 'store.snapshotTooLarge';
const STORE_RESTORE_INCOMPLETE = 'store.restoreIncomplete';
const STORE_SYMLINK_RESTORE_FAILED = 'store.symlinkRestoreFailed';

/**
 * 打开对话框。
 *
 * `opts.width` 选宽度档：
 *   `'form'`（默认）—— 880px 上限，给真正的表单（新建/编辑技能、MCP、分发）。
 *   `'narrow'`     —— 620px，给「确认删除 / 更新设置 / 安装确认」这类两三列、
 *                    甚至只有一段说明的对话框。
 *
 * 为什么必须分档：`.modal` 原来写死 620px，对新建技能那种六组字段的表单太窄；
 * 但把它整体拉到 880px 之后，**更新设置**这类短表单也跟着变宽，一行的标签与
 * 输入框之间隔着一大片空白，看起来像没排版。宽度是「这张表单有多少内容」的
 * 函数，不是全局常量，所以由调用方声明。
 */
function openModal(title, bodyNode, footNodes = [], opts = {}) {
  modalReturnFocus = document.activeElement;
  $('#modal-title').textContent = title;
  const body = $('#modal-body');
  body.innerHTML = '';
  body.appendChild(bodyNode);
  const foot = $('#modal-foot');
  foot.innerHTML = '';
  for (const f of footNodes) foot.appendChild(f);
  // 每次都显式设置，不依赖上一次的残留 —— 否则「先开表单、再开确认框」时确认框
  // 会沿用表单的宽度。
  const m = $('#modal');
  m.classList.toggle('modal-narrow', opts.width === 'narrow');
  $('#modal-scrim').classList.add('on');
  $('#modal-scrim').setAttribute('aria-hidden', 'false');
  focusInto(m);
}

/**
 * 当前正在等一个答复的 confirm 承诺。
 *
 * 对话框可能从它自己的按钮管不到的路径消失：Esc、遮罩、✕。以前这个承诺会永远
 * pending —— 对删除来说那看不出来（什么都没发生，正好），但对 `confirmWrite`
 * 不行，它的调用方得重建被它盖住的那个编辑器。只用一个注册表，因为 #modal
 * 只有一个。
 */
let pendingConfirm = null;

function closeModal() {
  const scrim = $('#modal-scrim');
  if (!scrim.classList.contains('on')) return;
  scrim.classList.remove('on');
  scrim.setAttribute('aria-hidden', 'true');
  // 关掉一个确认框本身就是一种答复：「否」。在视觉关闭之后再 resolve，调用方
  // 就能把对话框盖住的东西恢复回去。
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
    // 先 settle 再关闭：closeModal() 会把任何仍 pending 的确认答成 false，
    // 那样会盖掉这里的 true。
    const ok = el('button', { class: `btn ${danger ? 'danger' : 'primary'}`, onclick: () => { entry.settle(true); closeModal(); } }, confirmLabel);
    const cancel = el('button', { class: 'btn', onclick: () => { entry.settle(false); closeModal(); } }, t('common.cancel'));
    // 确认框（删除 / 覆盖 / 导入）只放一段说明与目标清单，用窄档。
    openModal(title, body, [cancel, ok], { width: 'narrow' });
  });
}

/**
 * 给屏幕上的对话框拍个快照，好让叠上来的确认框能把它放回去。
 *
 * 外壳只有一个 #modal，所以在编辑器开着的时候打开写入检查，会替换掉那个编辑器
 * 的 DOM。节点是*移动*（appendChild）而不是克隆的，所以留住这些引用恢复的是
 * *同一批*输入框 —— 用户已经敲进去的每次按键在取消确认之后都还在。
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
 * 把一个快照放回屏幕上。
 *
 * 两个地方会调它，原因各不同：取消了的写入检查（编辑器必须看起来没被动过），
 * 以及检查通过之后写却失败了（调用方只弹 toast，所以表单必须回来，否则用户就
 * 把它弄丢了）。
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
 * 一组分发目标各自的目的目录。
 *
 * 目标是（agent × scope × project），适配器按 kind 声明的是相对目录，所以真正
 * 的路径只有在这里才能算出来。`withLabel` 会加上 agent 名字前缀：两个 agent 可能
 * 在不同的根目录下声明同一个相对目录，光看路径在对话框里分不清是谁。
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
 * 对真实 agent 文件的每一次写操作都要过这里：一个对话框，在一个字节落盘之前
 * 列出即将变化的东西。设置被排除在外 —— 它们存在应用自己的存储里，不在 agent
 * 目录下。
 *
 * 它架在 confirmDialog 之上，而不是再写第二套模态实现：这个外壳没有授予任何
 * dialog 插件的 ACL，所以 `window.confirm` 是直接拒绝而不是提问，而两套模态路径
 * 在焦点处理上迟早会走偏。
 *
 * 关于遮罩：外壳只有一个 #modal，所以当这个检查叠在编辑器上面时，编辑器的节点
 * 会被挪到一边。答复是「否」时会把它们放回来 —— 而检查通过之后写却失败时，会
 * 通过 reopenCovered() 把它们放回来，因为调用方的失败分支只弹 toast，用户敲好的
 * 表单不能跟着错误一起消失。
 */
let coveredSnap = null;

/** 把上一次检查遮住的那个对话框放回屏幕上（写入失败之后）。 */
function reopenCovered() {
  const snap = coveredSnap;
  coveredSnap = null;
  restoreModal(snap);
}

async function confirmWrite({ title, action, targets, detail, danger = false }) {
  const list = (Array.isArray(targets) ? targets : [targets]).filter(Boolean).map(String);
  const MAX = 10;
  // 用路径列表而不是一句话：拼起来的路径会在路径段中间换行，恰好在用户需要
  // 对比它们的地方变得没法读。
  const extra = el('div', {},
    list.length
      ? el('ul', { class: 'write-targets mono fs-11' },
          ...list.slice(0, MAX).map((p) => el('li', {}, p)),
          list.length > MAX ? el('li', { class: 'muted' }, t('confirm.moreTargets', { n: list.length - MAX })) : null)
      : null,
    el('div', { class: 'alert info mt-12' }, t('confirm.writeBackupNote')));
  // 什么都没被遮住时为 null（是抽屉或表格行触发的），这样每个调用方都不用自己
  // 再带一个开关。
  const snap = snapshotModal();
  coveredSnap = snap;
  const ok = await confirmDialog({
    title: title || t('confirm.writeTitle'),
    message: [detail, t('confirm.writeBody')].filter(Boolean).join('\n\n'),
    confirmLabel: action || t('common.confirm'),
    danger,
    extra
  });
  // 取消时立刻恢复：检查一关，编辑器就必须看起来没被动过。成功时它保持隐藏，
  // 这样表单不会在 IPC 往返期间闪现在对话框背后；之后照旧由 closeModal() 关掉。
  if (!ok) { reopenCovered(); return false; }
  return true;
}

/* ------------------------------------------------------------------ */
/* 抽屉                                                                */
/* ------------------------------------------------------------------ */

/**
 * 关着的抽屉能不能依赖原生的 `inert` 属性？
 *
 * `inert` 是 Chromium 102 才有的；更老的 WebView2（或者压根没启用它的宿主）会
 * 静默忽略这个属性，于是关着的抽屉里那个 ✕ 还留在 tab 序里 —— 键盘用户会 Tab
 * 进一个屏幕外的面板。只做一次特性检测：原型在运行时不会变。
 */
const INERT_NATIVE = typeof HTMLElement !== 'undefined' && 'inert' in HTMLElement.prototype;

/**
 * 此刻是*哪一种*机制在模拟 inert。
 *
 * localStorage 里的 ak.forceInertFallback 戳（值 '1'）是一个**仅供取证**的开关：
 * 它让探针能在一个本来支持 `inert` 的 WebView2 上驱动 fallback 分支（CDP 会话
 * 没法把引擎降级）。普通用户永远不会设它；每次开关都读一次，所以探针不用重载
 * 就能切换。
 */
function inertMode() {
  let forced = false;
  try { forced = localStorage.getItem('ak.forceInertFallback') === '1'; } catch (_e) { /* storage disabled */ }
  return (!INERT_NATIVE || forced) ? 'fallback' : 'native';
}

/** fallback 覆盖掉的那些 tabindex 值，下次打开时恢复。 */
let drawerTabindexSaved = null;

/** 抽屉里面所有会进入 tab 序的东西。 */
const DRAWER_FOCUSABLE =
  'a[href], area[href], button, input, select, textarea, iframe, object, embed, [tabindex], [contenteditable="true"]';

/**
 * 让抽屉变惰性（关闭）或可交互，要么原生要么模拟。只放一处，因为打开/关闭和
 * 启动时的状态必须一致 —— 模式只应用一半，正好就是「✕ 还能 Tab 到」那个 bug。
 *
 * `data-ak-inert-mode` 是探针的钩子：它写明跑的是哪个分支，这样外部断言（CDP）
 * 不用再从行为去反推。
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
  // 兜底分支：`inert` 可能不存在或被忽略，所以它不能作为机制。但还是清一下 ——
  // 抽屉的标记里本来就带着 `inert`，一个*确实*支持它的引擎会让兜底分支没法测。
  if ('inert' in d) d.inert = false;
  d.setAttribute('aria-hidden', inert ? 'true' : 'false');
  if (inert) {
    // 幂等：启动时的那一次、失败后重试的那次启动，以及 closeDrawer()，都会要求
    // 「关闭」。第一轮之后再重新扫描，会把 tabindex="-1" 记成「原始值」，真正的
    // 值就丢了。
    if (drawerTabindexSaved) return;
    // 光有 aria-hidden 不够：它把子树从屏幕阅读器里藏掉了，但焦点停靠点还在。
    // 记住每个元素原本的 tabindex（通常是没有），这样打开时能还原标记。
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
  // 标题会做省略号截断；tooltip 是回到完整名字的唯一途径。
  $('#drawer-title').setAttribute('title', title);
  const b = $('#drawer-body');
  b.innerHTML = '';
  b.appendChild(node);
  b.scrollTop = 0;
  const d = $('#drawer');
  // 先恢复可 Tab 性：focusInto() 取的是第一个*可聚焦*的后代，而兜底分支把它们
  // 全留在了 tabindex="-1" 上。
  setDrawerInert(false);
  d.classList.add('on');
  $('#scrim').classList.add('on');
  focusInto(d);
}

/**
 * 抽屉唯一的那一个出口。
 *
 * ✕、遮罩和 Esc 全都走这里，所以「有未保存改动」的守卫不会被三者之一绕过。
 * 确认框在抽屉隐藏**之前**就跑：拒绝它，编辑器就停在用户离开时的样子。
 */
async function closeDrawer() {
  const d = $('#drawer');
  if (!d.classList.contains('on')) return;
  if (S.dirty) {
    // 这个守卫必须是本应用自己的对话框。`window.confirm()` 会被路由到 Tauri 的
    // dialog 插件，而这个外壳没有授予它（`Command plugin:dialog|message not
    // allowed by ACL`）：调用被拒绝，抽屉照旧关掉，于是用户的编辑在没有任何提示
    // 的情况下被丢弃。一条异步路径，保证 ✕ / 遮罩 / Esc 三者完全一致。
    // 屏幕上已经有的对话框拥有下一个答复 —— 再开一个会叠两层遮罩并把第一个
    // 承诺晾在那里。
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
  // inert（不只是 aria-hidden）：抽屉是移出屏幕，不是 display:none，所以它的 ✕
  // 否则会留在 tab 序里 —— 而把焦点能到的元素包在 aria-hidden 里本身就是一种
  // 无障碍错误。在没有 `inert` 的引擎上，setDrawerInert() 会兑现这个承诺：给每个
  // 可聚焦的后代加上 tabindex="-1"，下次打开时恢复。
  setDrawerInert(true);
  $('#scrim').classList.remove('on');
  render();
  const back = drawerReturnFocus;
  drawerReturnFocus = null;
  returnFocus(back);
}

/* ------------------------------------------------------------------ */
/* 启动                                                               */
/* ------------------------------------------------------------------ */

async function boot() {
  restoreUiState();
  // 侧栏折叠状态是个纯视觉偏好：在首帧之前恢复，免得启动时侧栏从 248px 跳到
  // 60px。
  if (localStorage.getItem('ak.side') === 'collapsed') $('#app').classList.add('collapsed');
  S.boot = await window.ak.bootstrap();
  // 设置决定语言和主题，所以要在决定首帧之前加载 —— theme-init.js 已经处理过
  // 那个属性，这里只是同步一下。
  try { S.settings = (await window.ak.getSettings()) || {}; } catch (e) { S.settings = {}; }
  if (S.settings.theme) applyTheme(S.settings.theme);
  if (S.settings.lang) i18n.setLang(S.settings.lang);
  if (!S.langHooked) { i18n.onChange(applyLanguage); S.langHooked = true; }

  // 先把外壳画出来（导航文案、接线按钮、静态文案的本地化），**然后**才做第一次
  // 扫描：扫描在真实机器上要几秒，不能让它把整个主区域留白。
  //
  // boot() 同时也是启动错误面板的重试入口，所以下面每个 wire* 最多跑一次：跑第二
  // 遍会叠上第二个扫描监听、第二个更新对话框和第二个语言监听。
  if (!S.wired) { wireGlobal(); S.wired = true; }
  applyLanguage(i18n.lang);
  // 关着的抽屉出厂就在屏幕外但可聚焦；启动这一遍会在用户有机会 Tab 进去之前
  // 就加上 inert（之后由 openDrawer/closeDrawer 保持一致）。
  setDrawerInert(true);
  await rescan(true);

  // --- 受理那几个 settings 承诺 --------------------------------------
  if (!S.updateWired) { wireUpdateBanner(); S.updateWired = true; }
  await initUpdates();
  // 「扫描完成」的通知只在窗口隐藏时才发，这样用户不必盯着应用，也能知道有哪些
  // 资源需要处理。
  if (S.settings.notifications !== false && !S.notifyWired) {
    S.notifyWired = true;
    document.addEventListener('visibilitychange', () => {
      if (!document.hidden && S.lastIssueCount === null) S.lastIssueCount = S.data?.summary?.issues ?? 0;
    });
  }
}

/* ------------------------------------------------------------------ */
/* 软件更新                                                             */
/* ------------------------------------------------------------------ */

/**
 * 主线程拥有更新日程；渲染层只负责把拿到的东西画出来。正是这个分工让自动检查
 * 能在用户翻到别的页面时弹出对话框，也正是它让「启动时检查」这个承诺不会变成
 * 每次刷新页面就打一次 GitHub 请求。
 */
async function initUpdates() {
  try { S.update = await window.ak.updateStatus(); } catch (e) { S.update = null; }
  // 加了守卫：initUpdates() 也会在启动重试时跑一遍，订阅两次会让每个事件弹出
  // 两次更新对话框。
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
  // 如果应用一打开就直接落在设置页，那这一页是用 bootstrap 的占位数据画的（没有
  // `repo`，也没有 `checkedAt`），否则会一直显示「github.com/」，直到用户切走再
  // 切回来。
  if (S.view === 'settings') render();
  // 徽标得扛住窗口开着好几个小时：这里只读主线程的快照（不发网络、不做检查），
  // 所以轮询它是免费的。
  if (!S.updatePollTimer) {
    S.updatePollTimer = setInterval(() => {
      window.ak.updateStatus().then((status) => {
        S.update = status;
        syncUpdateBadge(status);
        syncUpdateBanner(status);
      }).catch(() => { /* 一次性的 IPC 失败：保留上一个徽标 */ });
    }, 60000);
  }
}

/** 侧栏上的小圆点：有更新在等，而且用户还没关掉它。 */
function syncUpdateBadge(status) {
  const badge = $('#nav-update-badge');
  if (!badge) return;
  const waiting = !!(status && status.hasUpdate === true && !status.skipped);
  badge.classList.toggle('hidden', !waiting);
  if (waiting) syncUpdateBanner(status);
}

/**
 * 用户关掉过它横幅的那个版本。
 *
 * 只记在内存里不够：那个 60 秒的状态轮询会再调一次 `syncUpdateBanner()`，把用户
 * 刚关掉的横幅又显示出来。*更新*的版本号就不再匹配了，所以它照样会被通知。
 */
let updateDismissed = (function () {
  try { return localStorage.getItem('ak.updateDismissed') || null; } catch (_e) { return null; }
})();

/** 顶部横幅的一次性接线（每次启动都重新绑会叠加）。 */
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
 * 每个版本、每次会话最多弹一次「有更新」对话框。
 *
 * 「稍后」故意什么都不记 —— 调度器还会再找到同一个发布版本，所以真正阻止它每
 * 个周期重复弹的，是会话级的守卫。「跳过此版本」按版本号持久化在主线程状态里，
 * 所以下一个发布版本照样会正常提示。
 */
function promptUpdate(status) {
  if (!status || status.hasUpdate !== true || status.skipped) return;
  if (S.view === 'settings') return;            // 那个面板已经在屏幕上了
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
  openModal(t('update.dialogTitle'), body, [skip, later, goBtn], { width: 'narrow' });
}

/** 开始下载缓存好的那个发布版本的安装包，并轮询它的进度。 */
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
 * 进度轮询。任务对象活在主线程里（用户切页面时下载还在跑），所以这个轮询是 UI
 * 看到字节到达的唯一途径 —— 面板保留最后 N 行。
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
      try { S.update = await window.ak.updateStatus(); } catch (e) { /* 保留上一次的 */ }
      if (S.view === 'settings') render();
      if (task && task.done && task.path) promptInstall(task);
      else if (task && task.error) toast(t('update.downloadFailed', { error: task.error }), 'err');
      else if (task && task.canceled) toast(t('update.downloadCanceled'), '');
    }
  }, 900);
}

/**
 * 安装会关掉应用，所以它总是要一次显式确认 —— 下载完成和用户请求退出不是
 * 一回事。
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
  openModal(t('update.readyTitle'), body, [later, goBtn], { width: 'narrow' });
}

/**
 * 桌面通知，同时受「通知」这项设置和窗口可见性两道闸门约束 —— 用户已经盯着应用
 * 看的时候，应用内部弹一个 toast 就够了。
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
  } catch (e) { /* 通知本身是尽力而为的 */ }
}

async function rescan(silent = false) {
  // 「扫描进行中」的反馈：横幅下面一条细进度条，加上禁用的扫描按钮，这样慢扫描
  // 不会被读成「点了，没反应」。静默重扫（其他操作的副作用）不显示这条。
  const scanbar = $('#scanbar');
  const scanBtn = $('#btn-scan');
  // 本次会话的第一次扫描即使「静默」也要显示：用户正对着骨架屏，需要知道为什么
  // 数据还没落下来。
  const firstScan = !S.data;
  if (!silent || firstScan) {
    if (scanbar) { $('#scanbar-text').textContent = t('scan.scanning'); scanbar.classList.remove('hidden'); }
    if (scanBtn) { scanBtn.classList.add('loading'); scanBtn.disabled = true; }
  }
  try {
    // 写入落在真实的 agent 目录里，所以扫描就是这个视图需要的唯一状态 —— 没有
    // 什么暂存层还需要去取。
    const d = await window.ak.scan();
    S.data = d;
    S.scanError = null;
    // 丢掉已经不存在的勾选 id（在别处被删了 / 重扫换了 id）—— 否则批量操作条会
    // 把幽灵行算进去。
    if (S.checked.size) {
      const alive = new Set((d.assets || []).map((a) => a.id));
      for (const id of [...S.checked]) if (!alive.has(id)) S.checked.delete(id);
    }
    $('#nav-asset-count').textContent = d.summary.total;
    // 三个表面共用同一个数字。`summary.issues` 只统计 error 级别的格式问题，所以
    // 一个 warning 级别的发现会让侧栏徽标停在「0」、仪表盘卡片停在「0」，而健康
    // 页却列着「1 个资源需要处理」。issueAssets() 就是健康页的那个判据。
    const issueCount = issueAssets(d).length;
    const issueEl = $('#nav-issue-count');
    issueEl.textContent = issueCount;
    issueEl.classList.toggle('hidden', issueCount === 0);
    syncNavNames();
    syncSideFoot();
    render();
    if (!silent) {
      const errs = issueCount;
      toast(errs ? t('toast.scanIssues', { n: errs }) : t('toast.scanDone', { n: d.summary.total }),
        errs ? 'warn' : 'ok');
    }
  } catch (e) {
    // 扫描失败不能只藏在 toast 后面：表格会留着上一次的数据，而首次扫描会让主
    // 区域一片空白且无从重试。改为画出可重试的面板；它的重试就是重跑扫描。
    reportUiError('error', e);
    S.scanError = zhError(e);
    syncSideFoot();
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
  // 点遮罩和点 ✕ 是同一个出口：一个 closeDrawer，一次确认。
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

  // 在它外面点任何地方都关掉右键菜单。
  document.addEventListener('click', (e) => {
    const m = $('#ctxmenu');
    if (m && m.classList.contains('on') && !m.contains(e.target)) hideCtxMenu();
  });
  // 菜单内的方向键 / Home / End / Tab 离场（ARIA APG 的 menu 键盘契约）。
  initCtxMenuKeyboard();

  let debounce;
  $('#search').addEventListener('input', (e) => {
    clearTimeout(debounce);
    const v = e.target.value;
    debounce = setTimeout(() => {
      S.filter = v.trim().toLowerCase();
      // 一个新的查询词会像换了筛选条件一样重新定义可见集合，所以它必须重置页码
      // 和勾选集合：否则旧结果的第 3 页还是选中的，批量操作条会提出删除那些被
      // 查询词藏起来的行。
      S.page = 1;
      S.checked.clear();
      // 搜索框常驻在顶栏，但只有资产页消费 `S.filter`。以前在其它页敲字是**死
      // 操作**：`S.filter` 被设上了，当前视图却按自己的数据重画，屏幕上什么也
      // 不发生（而筛选条件已经悄悄改了）。现在敲字就切到资产页 —— 顶栏那个框
      // 到处都在，就得到处都管用。
      if (S.view !== 'assets') go('assets'); else render();
    }, 120);
  });

  document.addEventListener('keydown', (e) => {
    // 对话框优先：只要有一个开着，Tab 和 Esc 就归它。否则焦点会留在遮罩后面，
    // 而 Esc 会丢掉未保存的编辑。
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

  // 滚动时关掉行菜单。
  //
  // 菜单是 `position:fixed` 贴在指针/按钮处，列表一滚它就指向了别的行 —— 而它
  // 仍然可点，于是「删除」可能落到刚滚上来的那一行。菜单只由指针与 Escape 关掉，
  // 所以这件工作得在这里显式做。用捕获阶段订阅 `scroll`：`scroll` 不冒泡，而
  // 表格滚的是自己的 `.card-scroll`（不是 window），只有捕获阶段能收到。
  document.addEventListener('scroll', () => {
    const m = $('#ctxmenu');
    if (m && m.classList.contains('on')) hideCtxMenu();
  }, true);
}

/* ------------------------------------------------------------------ */
/* 渲染分发                                                             */
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

/** 如果这次渲染会替换掉 DOM，那个该留住焦点的元素。 */
function focusKeyNow() {
  const n = document.activeElement;
  if (!n || !n.closest) return null;
  const holder = n.closest('[data-focus-key]');
  return holder ? holder.getAttribute('data-focus-key') : null;
}

/**
 * 重渲染之后把焦点放回对应的控件上。
 *
 * 勾选框 / 筛选条件的变化会重渲染整个视图；没有这套，每次点击焦点都会掉到
 * <body> 上，于是键盘用户每选一次都要从页面顶部重新开始，屏幕阅读器也丢了位置。
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
  // 顶栏那几个动作里，「新建技能」和「导入项目」只对资产/项目有意义；在设置页
  // 顶上摆着三颗写按钮，读起来是「这一页能新建技能」。按视图收起来，而不是让
  // 用户点进对话框才发现走错了路。扫描按钮全站保留 —— 它刷新的是全机状态。
  const acts = { 'btn-new-skill': ['assets', 'overview'], 'btn-import': ['projects', 'assets', 'overview'] };
  for (const [id, views] of Object.entries(acts)) {
    const b = $('#' + id);
    if (b) b.classList.toggle('hidden', !views.includes(S.view));
  }
  const root = $('#view-root');
  const keepFocus = focusKeyNow();
  root.innerHTML = '';
  try {
    const out = v.render();
    // 异步视图（活动）是事后才 resolve 的；先放个占位，数据到了再换成真正的节点。
    if (out && typeof out.then === 'function') {
      root.appendChild(el('div', { class: 'empty' }, t('common.loading')));
      const viewAtCall = S.view;
      out.then((node) => {
        if (S.view !== viewAtCall) return;   // 用户已经切走了
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
 * 侧栏的折叠状态 → 那个切换按钮。
 *
 * 只剩图标的侧栏会把 `.nav-text` 隐藏掉，所以每个导航项都需要一个能扛住
 * display:none 的可访问名字；aria-label 加 title 一起承担。由 applyLanguage
 * （文案语言）和切换动作本身调用。
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

/** 重渲染所有显示着翻译文案的东西（导航、标题、计数）。 */
/**
 * 侧栏文案，含徽标上的数字。
 *
 * `aria-label` 会覆盖后代文本，所以那些徽标（「969」、问题计数、更新小圆点）对
 * 辅助技术是不可见的 —— 尽管它们是这些数字唯一存在的地方。出于同样的理由，可见
 * 文本也要保持同步。
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

/**
 * 侧栏底部的常驻状态：上次扫描的时间 + 待处理数。
 *
 * 三个状态由圆点 + 文字共同表达（颜色不单独承担信息）：待处理 → 琥珀、扫描
 * 失败 → 红、干净 → 绿。文案走 `aria-live="polite"`（标记里就带着），所以
 * 重扫完成时读屏用户会听到一次，而不用自己去找。
 */
function syncSideFoot() {
  const dot = $('#side-dot');
  const txt = $('#side-status');
  if (!dot || !txt) return;
  const d = S.data;
  const issues = d ? issueAssets(d).length : 0;
  const scannedAt = d && d.scannedAt;
  // 失败优先于一切。判据是 `S.scanError` 本身 —— 不能写成 `!d && S.scanError`：
  // 中途一次重扫失败时 `S.data` 还是上一次的成功结果（表格里照样有行可点），
  // 那个条件于是永远不成立。实测：强制扫描失败后主区域正确弹出可重试面板，
  // 而侧栏还在说「扫描于 13 秒前 · 2 项待处理」并亮琥珀 —— 它替一次失败做了
  // 成功背书，而这正是这个状态条存在的理由。
  let state = 'ok';
  if (issues) state = 'warn';
  if (S.scanError) state = 'err';
  dot.className = 'side-dot' + (state === 'ok' ? '' : ' ' + state);
  const when = scannedAt ? t('side.scannedAgo', { ago: ago(scannedAt) }) : t('side.notScanned');
  txt.textContent = state === 'err'
    ? t('side.scanFailed')
    : (issues ? t('side.withIssues', { when, n: issues }) : t('side.clean', { when }));
  txt.title = txt.textContent;
  // 窄窗口里状态徽标全被隐藏，这里补一个短读数：待处理数 / 失败标记 / 对勾。
  const short = $('#side-short');
  if (short) {
    short.textContent = state === 'err' ? '!' : (issues ? String(issues) : '✓');
    short.title = txt.textContent;
  }
}

function applyLanguage(lang) {
  document.documentElement.lang = lang === 'zh' ? 'zh-CN' : 'en';
  syncNavNames();
  syncSideFoot();
  // index.html 里的静态标记用 data-i18n / data-i18n-placeholder /
  // data-i18n-title / data-i18n-label（ARIA 名字，它们没有文本节点）来声明参与。
  for (const n of $$('[data-i18n]')) n.textContent = t(n.dataset.i18n);
  for (const n of $$('[data-i18n-placeholder]')) n.placeholder = t(n.dataset.i18nPlaceholder);
  for (const n of $$('[data-i18n-title]')) n.title = t(n.dataset.i18nTitle);
  for (const n of $$('[data-i18n-label]')) n.setAttribute('aria-label', t(n.dataset.i18nLabel));
  const group = $('#nav-group-operate');
  if (group) group.textContent = t('nav.groupOperate');
  const overview = $('#nav-group-overview');
  if (overview) overview.textContent = t('nav.groupOverview');
  // #btn-scan 的 title 来自标记里的 data-i18n-title="nav.rescanTitle"；这里不要
  // 拿一句副标题覆盖它（在按钮上读起来是错的）。
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
    // 显式的排序选择正是这个控件的全部意义，所以它必须赢：这个函数末尾不能再有
    // 一次无条件重排，那会把除 `default` 以外的每个选择都丢掉。
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
 * 健康页会列出的资源：任何格式 / 校验问题或安全发现，但排除宿主缓存（那里的缺陷
 * 是厂商的，不是用户的）。侧栏徽标、仪表盘「需要处理」卡片和健康页读的都是这
 * 一个判据。
 */
function issueAssets(d) {
  return (d.assets || []).filter((a) =>
    ((a.issues || []).length || (a.security || []).length) && (a.origin || 'user') !== 'cached');
}

/* 引擎的原始枚举（`skill`、`mcp`、`global`）是数据、不是 UI 文案：它们必须在到达
   chip 或表格单元格之前过一遍词典，否则中文窗口会显示「记忆 3 · global」。 */

const KIND_LABEL = {
  skill: 'assets.kindSkill',
  mcp: 'assets.kindMcp',
  plugin: 'assets.kindPlugin',
  rule: 'assets.kindRule',
  memory: 'assets.kindMemory',
  // 审计日志里还会出现这三个 kind（设置写入、扫描、应用事务），它们不是资产类型，
  // 但操作记录页用同一个 `chip` + `kindName()` 渲染徽标 —— 漏掉就会在中文界面上
  // 露出 `settings` / `scan` / `apply` 三个英文标识符。
  settings: 'activity.kindSettings',
  scan: 'activity.kindScan',
  apply: 'activity.kindApply'
};

/** "skill" → 技能 / Skills。未知的 kind（新适配器）退回枚举本身。 */
function kindName(kind) {
  return KIND_LABEL[kind] ? t(KIND_LABEL[kind]) : kind;
}

/** "global" / "project" → 全局 / 项目级。 */
function scopeName(scope) {
  return scope === 'project' ? t('assets.scopeProject') : t('assets.scopeGlobal');
}

/* ------------------------------------------------------------------ */
/* 视图：总览                                                          */
/* ------------------------------------------------------------------ */

/**
 * 按 kind 拆分的资源计数，给仪表盘的「每个 agent」表格用。
 *
 * 共用一份 KIND_COLS 定义，这样表头顺序和单元格顺序永远不可能走偏 —— 一个
 * 「每个 agent」行的列如果跟表头不是同一份清单生成的，正好就是「数字对不上
 * 标题」那个缺陷。
 */
const KIND_COLS = ['skill', 'mcp', 'plugin', 'rule', 'memory'];

function viewOverview() {
  const d = S.data;
  const s = d.summary;
  // 和侧栏徽标、健康页同一个判据（issueAssets），所以三者不可能对「有多少资源
  // 需要处理」给出不同的答案。
  const issueCount = issueAssets(d).length;
  // .view-fill：视图根是一个高度未知的滚动盒子，所以下面两个面板得去占掉剩余
  // 空间，而不是缩在空页面顶部当两张矮卡片。
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

  // 每个面板独占一整行。并排的话，在常见的 1440px 视口下它们要分掉约 560px 的
  // 卡片宽度，于是 agent 表的八列会把自己那列 Agent 压到 68px —— 窄到把它本该
  // 用来标识的那个标签都做了省略号。改成上下堆叠后，每张表都拿到完整的内容
  // 宽度，两个面板再瓜分剩下的高度。
  const row = el('div', { class: 'row-fill col mt' });

  /* 每个 agent —— 整宽，每个 kind 一列。
     原来放在这里的「构成」面板被去掉了：它的堆叠条重复了正上方那八张统计卡
     （同样五种 kind，同样那几个数字），而它跟另一张卡共享半列的结果，是两张卡
     都矮到读不出来。 */
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
  row.appendChild(el('div', { class: 'flex-1-min panel-agents' }, agentCard));

  /* 最近活动 */
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
  row.appendChild(el('div', { class: 'flex-1-min panel-recent' },
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
 * 仪表盘的 agent 行 → 过滤到该 agent 的资源列表。
 *
 * 跳转之前会把其他每个维度都重置。留下一个维度，正是「点一下却落到空列表」的
 * 成因：每个 agent 那张表在八张统计卡下面，其中几张刚刚写过 `kindFilter` 和
 * `originFilter`，而这两个值会跨导航（persistUiState）、跨重启（restoreUiState）
 * 被记住。于是「某个 agent 只拥有一条资源」的那一行，会同时被那条资源的 kind
 * *和*一个把它藏起来的来源过滤器过滤掉，资源页就理所当然地报「无匹配」—— 而这
 * 个筛选条件用户从来没设过。页码必须重置是同一个道理：落在一个只有 1 行结果集
 * 的第 4 页，看到的就是一张空表。
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
 * 仪表盘的统计卡。传 `onClick` 就把它变成下钻入口 —— 一张只报数字、点了没反应的
 * 死卡，会逼着用户自己去别处找这个数字在哪。
 */
/**
 * 给那些只能点的行（agent 行、最近活动行、矩阵行、项目行）用的 Enter/Space
 * 处理函数。资源表是个例外：它用 ↑/↓ 移动光标，所以 50 行不会变成 50 个
 * tab 停靠点。
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

/* i18n 与主题（定义在 i18n.js / theme-init.js 里） */
const i18n = window.__akI18n;
const t = (k, vars) => i18n.t(k, vars);

/** 切换视图并保持侧栏同步。导航只有这一个出口。 */
function go(view) {
  if (!VIEWS[view]) view = 'overview';
  S.view = view;
  $$('.nav-item').forEach((x) => x.classList.toggle('active', x.dataset.view === view));
  syncSearchBox();
  render();
}

/**
 * `#search` 显示的是 `S.filter`，但下钻路径（统计卡、项目行）是设筛选条件而不是
 * 敲字的。输入框必须跟着筛选条件走，否则界面会声称有一个列表并没有在用的查询。
 */
function syncSearchBox() {
  const s = $('#search');
  if (s && document.activeElement !== s) s.value = S.filter || '';
}

/** 跨重启保存资源列表的 UI 状态。筛选与排序会活下来；选中的那一行不会
 * （重扫之后它可能已经不存在了）。 */
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
  } catch (e) { /* 隐私模式 —— 筛选条件就是不持久化 */ }
}

function restoreUiState() {
  try {
    const ui = JSON.parse(localStorage.getItem('ak.ui') || '{}');
    for (const k of ['kindFilter', 'agentFilter', 'scopeFilter', 'originFilter', 'sortKey', 'onlyShadowed']) {
      if (ui[k] !== undefined) S[k] = ui[k];
    }
  } catch (e) { /* 状态损坏 = 用默认值 */ }
}

/** 带着预先设好的筛选条件跳到资源页。 */
function drill(kind, origin) {
  S.kindFilter = kind;
  if (origin) S.originFilter = origin;
  else S.originFilter = kind === 'plugin' ? 'curated' : 'all';
  // 另外四个维度是「黏」的：它们跨重启持久化，并且每次导航都不会丢，所以一张只
  // 设 `kindFilter` 的统计卡，可能落到一个已经被别的筛选条件清空的列表上 —— 而且
  // 页码还可能指向更短结果集的末尾之外，那是一张空表而不是一个空列表。把它们
  // 重置掉；卡片点名的就是它下钻的那一个维度。
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
/* 视图：资源                                                          */
/* ------------------------------------------------------------------ */

function viewAssets() {
  const list = filteredAssets();

  // 分页：一次渲染 987+ 行是白扔帧；每页 50 行能让滚动和绘制成本随着库变大保持
  // 平坦。筛选条件变化时回到第 1 页（selectGroup 负责）。
  const PER_PAGE = 50;
  const pageCount = Math.max(1, Math.ceil(list.length / PER_PAGE));
  if (S.page > pageCount) S.page = 1;
  const pageItems = list.slice((S.page - 1) * PER_PAGE, S.page * PER_PAGE);

  // 锁框：筛选条与分页条常驻，只有表体滚。795 行、50 行一屏时，翻到第 7 页还得
  // 能改筛选条件；顺带让表头真正粘住 —— `table.tbl th` 的 sticky 一直声明着，但
  // `.card{overflow-x:auto}` 让它相对一个从不纵向滚动的盒子定位，实测滚 700px 后
  // 表头跑到 -537px（概览页那两张表反而是 0px：同一个组件两种行为）。
  const wrap = el('div', { class: 'fill-locked' });

  // 每个维度都在自己的小标题下；右侧是命中计数与「导入包」。
  const active = activeFilterCount();
  const fields = [
    selectGroup('kind', S.kindFilter, [['all', t('assets.allKinds')], ['skill', t('assets.kindSkill')], ['mcp', 'MCP'], ['plugin', t('assets.kindPlugin')], ['rule', t('assets.kindRule')], ['memory', t('assets.kindMemory')]], { title: t('filter.kindShort') }),
    selectGroup('agent', S.agentFilter, [['all', t('assets.allAgents')], ...agentOptions()], { title: t('filter.agentShort') }),
    selectGroup('scope', S.scopeFilter, [['all', t('assets.bothScopes')], ['global', t('assets.scopeGlobal')], ['project', t('assets.scopeProject')]], { title: t('filter.scopeShort') }),
    selectGroup('origin', S.originFilter, [['curated', t('assets.curated')], ['user', t('assets.userOnly')], ['all', t('assets.everything')], ['cached', t('assets.cachesOnly')]], { title: t('filter.originShort') }),
    selectGroup('sort', S.sortKey || 'default',
      [['default', t('assets.defaultOrder')], ['tokens', t('assets.largestFirst')], ['name', t('assets.nameAsc')], ['agent', t('assets.byAgent')], ['scope', t('assets.byScope')]], { title: t('filter.sortShort') })
  ];
  // 维度之间插分隔线 —— 它们是「与」关系，而五个下拉并排时没有边界可读。
  const withSeps = [];
  fields.forEach((f, i) => {
    if (i) withSeps.push(el('span', { class: 'filter-sep', 'aria-hidden': 'true' }));
    withSeps.push(f);
  });

  const bar = el('div', { class: 'card mb-14 flex-none' },
    el('div', { class: 'card-body' },
      el('div', { class: 'filter-bar' },
        ...withSeps,
        el('div', { class: 'filter-actions' },
          el('span', { class: 'muted fs-11', 'aria-live': 'polite' }, t('assets.shown', { n: list.length })),
          el('button', { class: 'btn sm', onclick: doImportBundle, title: t('bundle.importTitle') }, t('bundle.import'))
        )
      )
    ),
    activeFilterBar(active)
  );
  wrap.appendChild(bar);

  const rows = pageItems.map((a) => {
    const errs = issuesOf(a);
    // 名字必须能标识这一行：`{n} selected` 会让 50 个勾选框念出同样的东西，
    // 于是屏幕阅读器用户分不清某个勾选框属于哪条资源。
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
      // `label` 包住勾选框，把命中区从 16px 的方块扩到整格（≥24×24，WCAG 2.2 AA
      // Target Size）。点击仍走 change 事件，与直接点勾选框完全同一条路径。
      el('td', { class: 'nowrap col-check' }, el('label', { class: 'tick' }, cb)),
      el('td', { class: 'col-kind' }, el('span', { class: `chip ${a.kind}` }, kindName(a.kind))),
      el('td', { class: 'col-name' },
        // `title`：名称列在窄宽度下会做省略号截断，而一个截断掉的技能名从这一行
        // 的其他任何地方都找不回来。
        el('div', { class: 't-name', title: a.name }, a.name),
        el('div', { class: 't-desc', title: a.description || a.meta.relLabel || '' }, a.description || (a.meta.relLabel || ''))
      ),
      el('td', { class: 'nowrap col-owner' },
        el('div', {}, zhText(a.agentLabel)),
        el('div', { class: 't-mono' }, a.scope === 'project' ? (a.projectName || '') : scopeName(a.scope))
      ),
      el('td', { class: 'num nowrap col-num' }, bytes(a.size)),
      el('td', { class: 'num nowrap muted col-num' }, a.meta && a.meta.estTokens ? '~' + a.meta.estTokens.toLocaleString() : '—'),
      el('td', { class: 'num nowrap col-state' },
        errs ? el('span', { class: 'chip err', title: t('assets.issue') }, t('assets.issueCount', { n: errs }))
          : (a.effective && a.effective.live === false ? el('span', { class: 'chip warn', title: t('dashboard.hiddenByProject') }, t('assets.shadowedBadge')) : el('span', { class: 'chip ok' }, t('assets.ok')))
      ),
      // 行内「更多」。这些动作此前**只能**通过右键菜单到达 —— 右键不是可发现
      // 的交互（触摸板、远程桌面、只用键盘的人都拿不到），而「分发 / 删除」
      // 是这一页最主要的写操作。按钮复用同一个菜单，所以两处的条目不会走偏。
      el('td', { class: 'col-act' },
        el('button', {
          class: 'icon-btn row-more',
          'aria-haspopup': 'menu',
          'aria-label': t('row.more', { name: a.name }),
          title: t('row.more', { name: a.name }),
          'data-focus-key': `more-${a.id}`,
          onclick: (e) => { e.stopPropagation(); showCtxMenu(e, a, true); }
        }, '⋯'))
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

  const tbl = el('table', { class: 'tbl tbl-assets', tabindex: '0', 'aria-label': t('nav.assets') },
    el('thead', {}, el('tr', {},
      el('th', { class: 'nowrap col-check' }, el('label', { class: 'tick' }, headCb)),
      el('th', { class: 'col-kind' }, t('th.kind')),
      el('th', { class: 'col-name' }, t('th.name')),
      el('th', { class: 'col-owner' }, t('th.owner')),
      el('th', { class: 'num col-num' }, t('drawer.size')),
      el('th', { class: 'num col-num' }, t('th.tokens')),
      el('th', { class: 'num col-state' }, t('th.state')),
      el('th', { class: 'col-act' }, ''))),
    el('tbody', {}, rows.length ? rows : el('tr', {}, el('td', { colspan: '8' },
      // 空态必须给出下一步。以前只有一句「没有符合当前筛选条件的资产。」—— 而
      // 空列表的成因几乎总是筛选条件（五个维度会跨导航、跨重启被记住），用户
      // 看不出该改哪一个。这里复用筛选条那份条件清单，逐个列出来并各自可移除。
      el('div', { class: 'empty' },
        t('empty.noMatch'),
        (() => {
          const rowsNow = activeFilterCount();
          return rowsNow.length ? activeFilterBar(rowsNow, { inline: true }) : null;
        })())))
  ));

  // 键盘导航：↑/↓ 移动一个虚拟光标，Enter 打开。当前行会滚动进视野，所以长列表
  // 不用鼠标也照样能操作。
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

  // `.card-scroll` 是唯一的滚动盒：卡片自己不能滚（表头要粘住），表格横向溢出时
  // 也在这个盒子里滚。
  // 分页条是**表格卡片的页脚**，不是第二张卡片：一起放进同一个 `.card` 里，
  // 于是它们共享一个圆角轮廓、一条分界线，读起来是一块面板。
  // `.flex-none` 让它在锁框布局里固定于底部：翻页按钮跟着列表滚出视野，等于
  // 每翻几页都得先滚回页底。
  const pager = (list.length > PER_PAGE || S.page > 1)
    ? el('div', { class: 'pager flex-none' },
        el('span', { class: 'info', 'aria-live': 'polite' }, t('pager.info', {
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
        el('span', { class: 'page-num' }, `${S.page} / ${pageCount}`),
        el('button', {
          class: 'btn sm',
          disabled: S.page >= pageCount ? 'true' : null,
          'aria-label': t('pager.next'),
          'data-focus-key': 'pager-next',
          onclick: () => { S.page++; render(); }
        }, '→'))
    : null;

  // `.card-scroll` 是唯一的滚动盒：卡片自己不能滚（表头要粘住），表格横向溢出时
  // 也在这个盒子里滚。
  wrap.appendChild(el('div', { class: 'card card-fill' },
    el('div', { class: 'card-scroll' }, tbl),
    pager));

  // 批量操作条只在有勾选时出现。至于「对话框打开时禁用操作」则没必要 —— 模态
  // 本来就叠在它上面。它排在表格卡片下面，与它同宽。
  if (S.checked.size > 0) {
    const selected = (S.data?.assets || []).filter((a) => S.checked.has(a.id));
    wrap.appendChild(el('div', { class: 'batchbar flex-none' },
      el('span', { class: 'count' }, t('batch.selected', { n: S.checked.size })),
      el('button', { class: 'btn danger sm', onclick: () => doBatchDelete(selected) }, t('batch.delete')),
      el('button', { class: 'btn sm', onclick: () => openBatchDistribute(selected) }, t('batch.distribute')),
      el('button', { class: 'btn sm', onclick: () => doExportBundle(selected) }, t('bundle.export')),
      el('button', { class: 'btn ghost sm', onclick: () => { S.checked.clear(); render(); } }, t('batch.clear'))
    ));
  }
  return wrap;
}

/**
 * 当前生效的条件清单：每项一个 dim、一条可读标签、一个清除动作。
 *
 * 五个维度会跨导航、跨重启保留（`persistUiState`），所以「这张表为什么是空的」
 * 几乎总是某个用户早就忘了的条件。这份清单同时喂给两个地方：筛选条下方的汇总
 * 条，以及空态里的「生效条件」。两边共用同一份，才不会一处说「Agent：WorkBuddy」
 * 另一处漏掉它。
 */
function activeFilterCount() {
  const rows = [];
  if (S.filter) rows.push({ dim: 'q', label: t('filter.query', { q: S.filter }), clear: () => { S.filter = ''; syncSearchBox(); } });
  if (S.kindFilter !== 'all') {
    rows.push({ dim: 'kind', label: `${t('filter.kindShort')}：${kindName(S.kindFilter)}`, clear: () => { S.kindFilter = 'all'; } });
  }
  if (S.agentFilter !== 'all') {
    const hit = agentOptions().find(([id]) => id === S.agentFilter);
    rows.push({ dim: 'agent', label: `${t('filter.agentShort')}：${hit ? hit[1] : S.agentFilter}`, clear: () => { S.agentFilter = 'all'; } });
  }
  if (S.scopeFilter !== 'all') {
    rows.push({ dim: 'scope', label: `${t('filter.scopeShort')}：${scopeName(S.scopeFilter)}`, clear: () => { S.scopeFilter = 'all'; } });
  }
  // 「来源」的默认值是 `curated`（只看用户自己的 + 托管的，排除缓存噪声），
  // 所以 `curated` **不是**一个「用户加的条件」，不该出现在这份清单里。
  // 以前这里判的是 `!== 'all'`，于是每次进资产页都挂着一枚「来源：精选 + 自管」，
  // 点它的 ✕ 却没有任何变化 —— 它本来就已经是默认值了，那是一个死按钮。
  // 判据改成「与默认值不同」：只有用户真的改过来源，它才成为一条可移除的条件。
  if (S.originFilter !== 'curated') {
    const name = {
      all: t('assets.everything'), user: t('assets.userOnly'), cached: t('assets.cachesOnly')
    }[S.originFilter] || S.originFilter;
    rows.push({ dim: 'origin', label: `${t('filter.originShort')}：${name}`, clear: () => { S.originFilter = 'curated'; } });
  }
  if (S.sortKey) {
    const opts = { tokens: t('assets.largestFirst'), name: t('assets.nameAsc'), agent: t('assets.byAgent'), scope: t('assets.byScope') };
    rows.push({ dim: 'sort', label: `${t('filter.sortShort')}：${opts[S.sortKey] || S.sortKey}`, clear: () => { S.sortKey = null; } });
  }
  if (S.onlyShadowed) rows.push({ dim: 'shadowed', label: t('assets.shadowedOnly'), clear: () => { S.onlyShadowed = false; } });
  return rows;
}

/** 把条件清单渲染成可逐项移除的汇总条；没有条件时整条不渲染。 */
function activeFilterBar(rows, { inline = false } = {}) {
  if (!rows.length) return null;
  const after = (fn) => {
    fn();
    S.page = 1;
    S.checked.clear();
    persistUiState();
    render();
  };
  return el('div', { class: inline ? 'filter-active inline' : 'filter-active' },
    el('span', { class: 'lead' }, t('filter.activeLead')),
    ...rows.map((r) => el('span', { class: 'chip' },
      r.label,
      el('button', {
        type: 'button',
        'aria-label': t('filter.removeOne', { x: r.label }),
        title: t('filter.removeOne', { x: r.label }),
        onclick: () => after(r.clear)
      }, '✕'))),
    el('span', { class: 'spacer' }),
    el('button', {
      class: 'btn ghost sm',
      onclick: () => after(() => {
        S.filter = ''; S.kindFilter = 'all'; S.agentFilter = 'all';
        S.scopeFilter = 'all'; S.originFilter = 'curated'; S.sortKey = null;
        S.onlyShadowed = false;
        syncSearchBox();
      })
    }, t('assets.clearFilters'))
  );
}

function agentOptions() {
  const ids = new Set((S.data?.assets || []).map((a) => a.agentId));
  return [...ids].map((id) => {
    const l = zhText((S.data.assets.find((a) => a.agentId === id) || {}).agentLabel) || id;
    return [id, l];
  });
}

/** 每个筛选维度的可访问名字（一个 select 的取值并不是它的名字）。 */
const FILTER_LABEL = {
  kind: 'filter.kind', agent: 'filter.agent', scope: 'filter.scope',
  origin: 'filter.origin', sort: 'filter.sort'
};

/**
 * 一个筛选维度：小标题 + 原生 `<select>`。
 *
 * 标题用 `<label for>` 真正绑定到控件上 —— 视觉用户看到「类型 / Agent / 作用域」
 * 就知道每一维在筛什么，读屏用户也从 label 拿到同一句话（`aria-label` 只用来
 * 在 label 之外补一句更完整的说明，两者并存不冲突）。
 *
 * 控件本身仍是原生 `<select>`：自绘下拉要自己实现键盘、输入法、读屏角色与
 * 虚拟滚动，而这一页的每个筛选维度都是「从固定清单里选一个」—— 原生控件在这
 * 件事上没有对手。
 */
function selectGroup(dim, current, options, opts = {}) {
  const id = `filter-${dim}`;
  const s = el('select', {
    class: 'select-inline',
    id,
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
    // 筛选条件一变，「被选中的那些」的含义就变了。留着过期的 id，意味着批量操作
    // 条会提出删除用户根本看不见的资源。
    S.checked.clear();
    persistUiState();
    render();
  });
  const label = el('label', { for: id },
    opts.title || t(FILTER_LABEL[dim] || 'filter.kind'),
    opts.count ? el('span', { class: 'filter-count' }, ` · ${opts.count}`) : null);
  const field = el('div', { class: 'filter-field' }, label, s);
  field._select = s;
  return field;
}

/* ------------------------------------------------------------------ */
/* 视图：agent                                                         */
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
 * Agent 矩阵：哪些资源在哪些 agent 上存在。skillhub-local 的核心视图 —— 缺口
 * 一眼可见，点一个缺口单元格就会提出把该资源分发到那个 agent。
 */
function matrixCard() {
  const d = S.data;
  const kind = S.matrixKind || 'skill';
  const agents = d.agents.filter((a) => a.installed);
  // 跨所有 agent 的每行是（name, kind）；scope 只在有歧义时显示。
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
    // 点一行会打开它在任意 agent 上存在那份资源；缺口单元格保持无反应。
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
/* 视图：项目                                                          */
/* ------------------------------------------------------------------ */

function viewProjects() {
  const d = S.data;
  const head = el('div', { class: 'spread mb-12' },
    el('div', { class: 'muted fs-12' },
      t('projects.markerHint', { dirs: ['.workbuddy', '.claude', '.codex', '.cursor'].join('、') })),
    el('button', { class: 'btn primary', onclick: doImportProject }, '+ ' + t('projects.import'))
  );

  const rows = d.projects.map((p) =>
    // 点一个项目会把资源列表过滤到它（按项目名做文本搜索）—— 这是从「这个项目
    // 拥有哪个」走到资源的最快路径。
    el('tr', { onclick: () => { S.filter = p.name.toLowerCase(); go('assets'); },
      tabindex: '0', onkeydown: rowKey(() => { S.filter = p.name.toLowerCase(); go('assets'); }) },
      el('td', {},
        el('div', { class: 't-name', title: p.name }, p.name),
        el('div', { class: 't-mono', title: p.dir }, p.dir)
      ),
      el('td', { class: 'nowrap' }, el('span', { class: 'chip' }, t('projects.agentsCount', { n: p.agents.length }))),
      ...['skill', 'mcp', 'plugin', 'rule', 'memory'].map((k) => el('td', { class: 'num' }, String(p.counts[k] || 0))),
      el('td', { class: 'num muted nowrap' }, ago(p.addedAt)),
      el('td', {}, el('button', {
        class: 'btn sm danger',
        onclick: async () => {
          // 移除一个项目只改写注册表，所以目标就是用户放弃的那个项目目录 ——
          // 而不是某个会被改动的文件。
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

  // 项目表：占满视图的剩余高度，而不是只按内容长高。
  //
  // 以前这张卡片的高度完全由内容决定：没有导入任何项目时它只有约 190px（空态
  // 文案 139px + 表头），浮在 830px 的视图里，下面一大片空白 —— 看起来像是页面
  // 没加载完。导入十几个项目之后它又会一直长高、把整页推成滚动。
  //
  // 改成锁框布局（与资产页同一套 `.fill-locked`）：卡片吃掉标题行以下的全部
  // 高度，表格在自己的 `.card-scroll` 里滚动。空态因此落在**整块面板的中央**，
  // 而不是挤在顶部一条。`flex-none` 给标题行，剩下的都给卡片。
  const shell = el('div', { class: 'fill-locked' });
  shell.appendChild(el('div', { class: 'flex-none' }, head));

  shell.appendChild(el('div', { class: 'card card-fill' },
    el('div', { class: 'card-scroll' },
      el('table', { class: 'tbl proj-tbl' },
        el('thead', {}, el('tr', {}, el('th', {}, t('th.project')), el('th', {}, t('th.detected')), el('th', { class: 'num' }, t('dashboard.skills')), el('th', { class: 'num' }, t('dashboard.mcp')), el('th', { class: 'num' }, t('dashboard.plugins')), el('th', { class: 'num' }, t('dashboard.rules')), el('th', { class: 'num' }, t('dashboard.memory')), el('th', { class: 'num' }, t('th.added')), el('th', {}, ''))),
        el('tbody', {}, rows.length ? rows : el('tr', {}, el('td', { colspan: '9' },
          el('div', { class: 'empty empty-fill' },
            t('projects.noneLong'),
            el('div', { class: 'mt-12' },
              el('button', { class: 'btn primary', onclick: doImportProject }, '+ ' + t('projects.import'))))))))
    )
  ));
  return shell;
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
/* 视图：健康                                                          */
/* ------------------------------------------------------------------ */

function viewHealth() {
  const d = S.data;
  // 健康页是由若干**独立卡片**堆起来的，而每张卡片自己不带上边距 —— 于是
  // 安全发现卡、需要处理卡、变更基线卡之间完全没有缝隙，读起来是一整块。
  // 这里用一个纵向 flex + gap 统一给间距：卡片顺序与数量都是可变的，逐个加
  // `mt-*` 会在「只有基线卡」和「三张卡」两种情形下给出不同的第一行间距。
  const wrap = el('div', { class: 'stack-14' });

  /* --- 安全发现排在最前面：它们比格式问题更严重 --- */
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

  /* --- 格式 / 校验问题 --- */
  const bad = d.assets.filter((a) => (a.issues || []).length && (a.origin || 'user') !== 'cached');
  if (!bad.length && !risky.length) {
    // 这里不能 `return`：下面那张基线卡同时也是用户**锚定**基线的唯一入口
    // （「建立基线」）。提前返回会让这个功能在一台健康的机器上永远够不着 ——
    // 而恰恰是这个时候你最想记下一个干净的起点。渲染层审计把它报为一个 UI 死路。
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
            // 引擎的 issue 是英文（`unrecognized key "…" — agent may ignore it`），
            // 而这里以前直接 `i.message` 上屏 —— 健康页这两行是中文界面里仅剩的
            // 未翻译文案之一（实测英文泄漏点）。zhText 走的是同一份句子词典，
            // 字段名同理走 zhField（资产抽屉一直这么做，两处不该不一致）。
            el('span', { class: `chip ${i.level === 'error' ? 'err' : 'warn'} mr-6` }, zhField(i.field)), zhText(i.message))),
          el('div', { class: 'issue-meta' }, a.file)
        ),
        el('button', { class: 'btn sm', onclick: () => openAsset(a.id) }, t('common.inspect'))
      ));
    wrap.appendChild(el('div', { class: 'card' },
      el('div', { class: 'card-head' }, el('h2', {}, t(bad.length === 1 ? 'health.attentionOne' : 'health.attention', { n: bad.length })),
        el('span', { class: 'sub spacer' }, t('health.attentionNote'))),
      el('div', { class: 'card-body flex col gap-12' }, items)
    ));
  }

  wrap.appendChild(baselineCard());
  return wrap;
}

/**
 * 指纹基线：自用户上次看过之后发生了什么变化。显示变更 / 新增 / 消失的资源数量，
 * 并允许用户重新锚定。纯本地 —— 指纹是文件内容的 SHA256，扫描时已经算好了。
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
    // 在这之前，这张卡会永远停在「加载中…」：一个被拒绝的 invoke 在界面上留不下
    // 任何痕迹，只在控制台留一条 unhandled rejection。
    reportUiError('error', e);
    const body = box.querySelector('.card-body');
    if (body) inlineRetry(body, () => render());
  });
  return box;
}

/* ------------------------------------------------------------------ */
/* 视图：活动                                                          */
/* ------------------------------------------------------------------ */

async function viewActivity() {
  const wrap = el('div', {});
  const audit = await window.ak.audit({ limit: 200 });

  // 没有事务列表，也没有撤销按钮：存储不再保留备份，所以这里的「撤销」恢复不了
  // 任何东西 —— 日志现在纯粹是一份「发生过什么」的记录，而不是一份可反向操作的
  // 菜单。
  const items = audit.filter((r) => r.txId || r.action === 'scan').slice(0, 160).map((r) =>
    el('div', { class: `tl-item ${r.status === 'ok' ? 'ok' : r.status === 'failed' ? 'failed' : ''}` },
      el('div', { class: 'tl-head' },
        el('span', { class: 'tl-title' }, labelFor(r)),
        // 徽标以前直接渲染 `r.kind`：审计日志里的 kind 是 `skill` / `plugin` 这类
        // 标识符，于是中文界面上每一行都挂着一枚英文徽标 —— 而同一页的资产表
        // 用的是中文（kindName）。同一个概念两种写法，是 G6 的缺陷。
        r.kind ? el('span', { class: `chip ${r.kind}` }, kindName(r.kind)) : null,
        el('span', { class: 'tl-time' }, clock(r.ts))
      ),
      el('div', { class: 'tl-meta', title: auditMeta(r) }, auditMeta(r))
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

/** 审计记录的 agentId → adapters 声明的显示名；查不到就退回标识符本身。 */
function agentLabelOf(agentId) {
  if (!agentId) return '';
  const hit = (S.boot && S.boot.agents || []).find((a) => a.id === agentId);
  return zhText((hit && hit.label) || agentId);
}

/**
 * 一条审计记录的明细行文本。
 *
 * `agentId` 是标识符（`workbuddy`），回滚这类记录不带 `agentLabel`，于是回退成
 * `agentId` 就在中文界面里露出一个英文词；从 bootstrap 载荷里反查显示名 —— 那是
 * 适配器声明的 `label`，与资产表「归属」列同一个来源。
 *
 * 抽成函数是因为它被用两次：一次做可见文本，一次做 `title`。明细行在样式上被
 * 限制成两行（一条 sandbox 绝对路径动辄上百字符，不限就会压住右上角的时间戳），
 * 所以完整值必须能通过工具提示读到 —— 两份内容若各写一遍，迟早会不一致。
 */
function auditMeta(r) {
  return [zhText(r.agentLabel) || agentLabelOf(r.agentId), r.projectName, r.assetName,
    (r.targets || []).join('  '), zhError(r)]
    .filter(Boolean).join('  ·  ');
}

function labelFor(r) {
  if (r.action === 'scan') return t('activity.scanLabel', { n: r.count ?? '' });
  const key = ACTION_LABEL[r.action];
  // 未知 action 不再把英文标识符直接上屏：`t('verb.change')` 至少是目标语言，
  // 而一个没翻译的 `rollback` 会被当成产品文案。
  const verb = key ? t(key) : t('verb.change');
  // 设置页的写入没有资产名（`assetName` 是改动过的键名清单，一长串英文标识符，
  // 直接显示会把整行撑满）。它的标题到此为止，明细不进标题。
  const name = r.action === 'settings-save' ? '' : (r.assetName || '');
  return `${verb}${name ? ' ' + name : ''}`.trim() + (r.status === 'failed' ? ` ${t('activity.failedSuffix')}` : '');
}

/* ------------------------------------------------------------------ */
/* 视图：设置                                                          */
/* ------------------------------------------------------------------ */

/* ------------------------------------------------------------------ */
/* 发现：搜索公共注册表，通过 git 安装技能                                */
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

  // 按仓库直接安装。引擎一直都接受任意 owner/repo 或 GitHub URL
  // （ak:install-skill）；没有这个输入框，唯一走得通的路径就是「已经在注册表里列
  // 出来了」，这把所有私有或刚建的仓库都排除在外。
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
 * 从粘进来的仓库引用直接安装。复用和注册表结果相同的那个对话框，这样目标选择、
 * 安全扫描和影子层暂存的行为完全一致。
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
      // 安装会把新文件写进 agent 的全局技能目录 —— 这是一次写操作，所以它跟其他
      // 所有写操作一样，在克隆之前先问一句。
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
  openModal(t('discover.installTitle', { name: raw }), body, [cancel, okBtn], { width: 'narrow' });
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
    // invoke 被拒绝时，那个按钮会永远停在 loading 状态。
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
    // 注册表的请求在途时，用骨架行做微光动画。
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
            el('span', { class: 'disc-name', title: item.name }, item.name),
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
  openModal(t('discover.installTitle', { name: item.name }), body, [cancel, okBtn], { width: 'narrow' });
}

function viewSettings() {
  const wrap = el('div', {});
  const cfg = S.settings || {};

  /* --- 每项设置一行 -------------------------------------------------- */
  /**
   * 一行设置是一个真正的 <label for> 加上控件上的一个 id，绝不是一个带样式的
   * div：屏幕阅读器会从 label 念出「通知，开关」，而点文字就能操作控件。控件本身
   * 不可加 label 的那些行（只读路径、按钮组）保留普通 div，由控件自带名字。
   */
  let setRowSeq = 0;
  const LABELABLE = 'input, select, textarea, button, meter, output, progress';
  /**
   * 一行设置。`extra` 是**追加到控件右侧**的只读读数（本机统计那一行用它显示
   * 次数）；它不是控件，所以不参与 label 关联，也不进 Tab 序。
   */
  const row = (label, control, hint, extra) => {
    const target = control.matches && control.matches(LABELABLE)
      ? control
      : (control.querySelector ? control.querySelector(LABELABLE) : null);
    const ctl = extra
      ? el('div', { class: 'set-control' }, control, extra)
      : control;
    if (!target) {
      return el('div', { class: 'set-row' },
        el('div', { class: 'set-label' }, label, hint ? el('div', { class: 'set-hint' }, hint) : null),
        el('div', { class: 'set-control' }, ctl));
    }
    if (!target.id) target.id = `set-row-${++setRowSeq}`;
    const hintId = hint ? `${target.id}-hint` : null;
    if (hintId) target.setAttribute('aria-describedby', hintId);
    // role="switch" 自己不带任何文本：label 就是它唯一的名字。
    if (target.getAttribute('role') === 'switch' && !target.getAttribute('aria-label')) {
      target.setAttribute('aria-label', label);
    }
    return el('div', { class: 'set-row' },
      el('div', { class: 'set-label' },
        el('label', { for: target.id }, label),
        hint ? el('div', { class: 'set-hint', id: hintId }, hint) : null),
      el('div', { class: 'set-control' }, ctl));
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

  /** `name` 会成为这个开关的可访问名字（role=switch 本身没有文本）。 */
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

  /**
   * 「本机统计」那一行的只读读数：扫描 / 写入 / 失败次数。
   *
   * 这些数字是 `local_stats` 现算的（读 `audit.jsonl`），从本机算出来、也只留在
   * 本机。开关关掉时**整行读数不渲染**，而不只是不刷新 —— 否则「关掉统计」之后
   * 数字还挂在屏幕上。
   */
  function localStatsRow() {
    if (cfg.showLocalStats === false) return null;
    const out = el('span', { class: 'stats-value-right muted fs-11', 'aria-live': 'polite' },
      t('common.loading'));
    window.ak.localStats().then((r) => {
      if (!r || r.ok === false || !r.counts) {
        out.textContent = t('settings.localStatsUnavailable');
        return;
      }
      out.textContent = t('settings.localStatsValue', r.counts);
    }).catch(() => {
      // 一次失败的读数不该把这一行永远留在「加载中…」。
      out.textContent = t('settings.localStatsUnavailable');
    });
    return out;
  }

  const save = async (patch) => {
    const r = await window.ak.saveSettings(patch);
    if (r.ok) {
      S.settings = r.settings;
      toast(t('common.saved'), 'ok');
      render();
    } else {
      toast(r.error || t('toast.saveFailed'), 'err');
      // 控件上还显示着那个被拒绝的值，于是界面声称了一个从未写进 settings.json
      // 的变更。重新读一遍真正存下来的东西。
      render();
    }
  };

  /* --- 外观 ---------------------------------------------------------- */
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
      // 「行为」两行曾被整块删掉（commit a24a764「设置页缩短到一屏」），但
      // `local_stats` 命令、bridge 与 `showLocalStats` 键都留着 —— 结果是：
      // 一个「关掉本机统计」的开关跟着 UI 一起消失，而计数接口仍然可调；同时
      // `showLocalStats` 成了一个**没有任何人读**的键（ui-audit 的
      // 「telemetry setting stored but nothing sends counts」就是这条死链）。
      // 恢复这两行：开关有 UI、读写闭环，`ui-audit` 那条告警也随之清空。
      row(t('settings.notifications'),
        toggle(S.settings.notifications !== false, (v) => save({ notifications: v }), t('settings.notifications')),
        t('settings.notificationsHint')),
      row(t('settings.localStats'),
        toggle(S.settings.showLocalStats !== false, (v) => { save({ showLocalStats: v }); },
          t('settings.localStats')),
        t('settings.localStatsHint'),
        localStatsRow()),
      el('div', { class: 'muted fs-11 mt-6' }, t('settings.restartNote'))
    ));

  /**
   * GitHub 的标记，内联（Octicons `mark-github`，16×16）。
   *
   * 用内联 SVG 而不是 <img>：CSP 允许的是 `img-src 'self' data:`，所以一个图标
   * 就得变成一个要由 `copy-frontend.js` 打包发布、并由 dist 关卡保持同步的随包
   * 资源 —— 而这一切只为字形。用 `fill: currentColor`，就让外层的按钮自己掌握
   * 颜色，hover 状态也包含在内。
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
   * 软件更新卡片。
   *
   * 读的是 `S.update` —— 主线程推过来的那份快照。每个控件都经 `ak:update-settings`
   * 保存，并用返回的那份快照重新渲染，所以卡片绝不可能显示出一个更新器并没在用
   * 的值。
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
        // 「被节流」是对上一次结果的一次成功复用 —— 除此之外任何失败（包括
        // GitHub 的冷却）都必须说清楚原因。
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
      // 检查过了，但没有 release 回来。GitHub 对「还没发布任何 release」和「这个
      // 仓库是私有的、而你是匿名用户」两种情况都返回 404 —— 它是故意不做区分的。
      // 在没有保存 token 的前提下，第二种成因才是值得点名的：这块面板过去会在一个
      // 明明有 release 的仓库上说「暂无 release」，读起来就像个 bug。
      if (u.checked && !u.latest) {
        return u.token && u.token.filled === false
          ? t('settings.updateNoReleaseToken')
          : t('settings.updateNoRelease');
      }
      return u.checked ? '' : t('settings.updateNotChecked');
    };

    const checkBtn = el('button', { class: 'btn sm', onclick: (e) => void checkNow(e.currentTarget) }, t('settings.updateCheckNow'));

    /**
     * 一个按钮背后的那套自动更新设置。
     *
     * 更新源、间隔、beta 通道、代理、静默安装和 token 是六个「用户打开一次、之后
     * 再也不碰」的控件。内联在卡片里时，它们会把版本比对 —— 唯一需要用户做决定的
     * 那件事 —— 挤到首屏之外，所以现在它们住在一个对话框里，卡片只承担状态和那
     * 两个动作。
     */
    const openUpdateSettings = () => openUpdateSettingsModal(ucfg, saveUpdate);

    // 版本比对 —— 两个数字加一行结论。
    // actions sit at the right end: 检查更新 is the right-most element, so the
    // 眼睛直接落在主动作上，而不是先去找它。
    const versionRow = el('div', { class: 'upd-version' },
      el('div', { class: 'upd-version-cell' },
        el('div', { class: 'set-hint' }, t('settings.updateCurrent')),
        el('div', { class: 'upd-version-num mono' }, u.current || (S.boot && S.boot.version) || '—')),
      el('div', { class: 'upd-arrow' }, '→'),
      el('div', { class: 'upd-version-cell' },
        el('div', { class: 'set-hint' }, t('settings.updateLatest')),
        // 一个破折号读起来是「这个字段是空的」；诚实的状态是「上次检查没能说出
        // 一个版本号」，所以照实说。
        el('div', { class: `upd-version-num mono${u.hasUpdate === true ? ' is-new' : ''}` }, u.latest || t('settings.updateUnknown'))),
      el('div', { class: 'spacer' }),
      el('button', { class: 'btn sm', onclick: openUpdateSettings }, t('settings.updateSettingsBtn')),
      checkBtn);

    body.appendChild(versionRow);
    const verdict = versionLine();
    if (verdict) body.appendChild(el('div', { class: 'muted fs-11 mt-6' }, verdict));

    // 时间戳：「检查于」指的是真正那次检查的时刻（主线程的时钟），绝不是「因为
    // 页面重渲染了所以说是几分钟前」。
    const stamps = [];
    // `toLocaleString()` 不带 locale 时按运行环境来 —— 同一份构建在不同机器上
    // 会给出 `10/27/2026, 11:41:07 PM` 或 `2026/10/27 23:41:07`。这一页旁边就写着
    // 「检查于 …」，格式却是环境的。统一成 zh-CN（与 app.js `clock()` 一致），
    // 它在 en 界面下也是 ISO 风格的数字顺序，不会被读成月/日。
    const stamp = (iso) => new Date(iso).toLocaleString('zh-CN', {
      year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit'
    });
    if (u.checkedAt) stamps.push(t('settings.updateCheckedAt', { time: stamp(u.checkedAt) }));
    if (u.settings && u.settings.auto && u.nextCheckAt) {
      stamps.push(t('settings.updateNextAt', { time: stamp(u.nextCheckAt) }));
    }
    if (stamps.length) body.appendChild(el('div', { class: 'muted fs-11 mt-6' }, stamps.join(' · ')));
    // 引擎的错误是英文，而这一条是**设置页最常被看到的一行红字**（离线/证书链
    // 不受信任的机器上每次检查都会出现）。以前它整句原样上屏，中文界面里最显眼的
    // 提示是英文 —— 走 zhError 之后剩下的英文只有引号里的 URL 与 TLS 细节。
    if (u.lastError) body.appendChild(el('div', { class: 'alert warn mt-8' }, t('settings.updateError', { error: zhError(u.lastError) })));

    // 下载状态：进行中是进度条，就绪之后变成安装按钮。
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

    // 这张卡片上不放「打开 release 页面」按钮：下面那个更新日志面板已经把每个
    // 版本都链到了它的 release 页，而卡片只需要专注于那个决定（比对、检查、
    // 设置）。

    return el('div', { class: 'card' },
      el('div', { class: 'card-head' },
        el('h2', {}, t('settings.updateTitle')),
        el('span', { class: 'sub spacer' }, u.latest ? `v${u.latest}` : '')),
      body);
  }

  /**
   * Auto-update settings, behind the card's 更新设置 button.
   *
   * 每个控件都经 `saveUpdate` 写入，并用主线程返回的那份快照重新渲染，所以屏幕上
   * 的值永远就是更新器正在用的那个值。间隔输入框把草稿存在 S 里，这样一次重渲染
   * （用户正在打字时一次轮询回来了）不会把打了一半的内容丢掉。
   *
   * 发布仓库是不可编辑的：它是这个构建产物的一项属性（package.json → repository），
   * 而让用户去输入他们自己项目的地址，是这个对话框能做出来的最容易让人困惑的
   * 一件事。
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

    // 更新源是一个链接，不是一句话：仓库是这个对话框里用户唯一可能需要打开的东西
    // —— 读一个 release，或者在去找 token 之前先确认这个仓库是不是私有的。渲染成
    // GitHub 标记加 `owner/repo`，并且复用 release 页链接用的那条命令来打开
    // （这样 URL 白名单就始终留在 Rust 那边）。
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
     * 离开这个字段的两条路，共用同一个校验器。
     *
     * 输入框的 `onchange` 会检查 1–1440，但保存按钮却把敲进去的任何东西（`0`、
     * `99999`）直接写进磁盘，而失败文案还是那句提示语，而不是一句「什么都没保存」
     * 的陈述。
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

    // 代理：自动 / 直连 / Clash 的混合端口。手改过的设置文件里可能存着另一个
    // URL —— 要让它保持可见，而不是悄悄把它改写掉。
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

    // 静默安装：NSIS 的 /S /R。文案只在 Windows 上成立，但这个开关始终可见，
    // 这样等到真需要它的时候，用户已经知道有这个设置了。
    if (window.ak.platform === 'win32') {
      body.appendChild(row(t('settings.updateSilent'),
        switchFor(ucfg.silent !== false, (v) => void saveUpdate({ silent: v }), t('settings.updateSilent')),
        t('settings.updateSilentHint')));
    }

    // Token：状态徽标加一个只写不读的输入框。值永远不回显 —— 主线程只报告
    // 是否已填写以及来源。
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
      [el('button', { class: 'btn primary', onclick: closeModal }, t('common.done'))],
      { width: 'narrow' });
  }

  /**
   * 从完整的 release 列表里挑出要显示的那几条：**最新版、当前版、上一版**，最多三条。
   *
   * 规则（顺序即优先级，同一版本只留第一次出现的）：
   *   1. `releases[0]` —— 最新的那个版本（接口按时间倒序返回）；
   *   2. `current` —— 用户正在运行的那一版；
   *   3. `current` 在列表里的**下一条** —— 上一版，升级上来的人拿它对照。
   *
   * 为什么不是「取前三条」：那三条几乎总是「最新 + 两个更老的」，用户看不到自己
   * 正在用的那一版改了什么，而那恰恰是最相关的一条。
   *
   * 三种边界都成立：
   * - `current` 不在列表里（本地跑的是未发布的开发构建）→ 跳过第 2、3 步，结果
   *   仍是「最新版 + 列表接下来的两条」，不会因为找不到当前版而少显示。
   * - 列表只有 1–2 条 → 返回它本身，不凑数。
   * - `current` 就是最新版 → 第 1、2 步是同一版本，去重后只剩「当前版 + 上一版」，
   *   这是正确结果：没有更新的版本可看。
   */
  function pickChangelogVersions(releases, current) {
    if (!Array.isArray(releases) || !releases.length) return [];
    const out = [];
    const push = (rel) => {
      if (!rel) return;
      if (out.some((x) => String(x.version) === String(rel.version))) return;
      out.push(rel);
    };

    push(releases[0]);

    const idx = current
      ? releases.findIndex((r) => String(r.version) === String(current))
      : -1;
    if (idx >= 0) {
      push(releases[idx]);
      if (idx + 1 < releases.length) push(releases[idx + 1]);
    }

    for (const rel of releases) {
      if (out.length >= 3) break;
      push(rel);
    }
    // 按版本号倒序输出。
    //
    // 选取顺序是「最新 → 当前 → 上一版」，但**显示**顺序必须是版本从新到旧：
    // 用户当前版落后两个版本时（例如在 0.2.1 上，最新是 0.2.3），选取会得到
    // [0.2.3, 0.2.1, 0.2.2]，直接渲染就是「最新、当前、比当前新的那一版」——
    // 中间那一行看起来像是排错了。倒序之后是 [0.2.3, 0.2.2, 0.2.1]，与「更新
    // 日志」这个标题一致，用户也能顺着看出自己落后几版。
    //
    // 比较用数字段而不是字符串：`'0.2.10' > '0.2.9'` 在字符串比较里是 false，
    // 而版本号里这是常事。
    const verKey = (v) => String(v).split(/[.\-+]/).map((x) => parseInt(x, 10) || 0);
    out.sort((a, b) => {
      const A = verKey(a.version), B = verKey(b.version);
      for (let i = 0; i < Math.max(A.length, B.length); i++) {
        const d = (B[i] || 0) - (A[i] || 0);
        if (d) return d;
      }
      return 0;
    });
    return out.slice(0, 3);
  }

  /**
   * 更新日志面板：每个已发布的 release，最新的在前。
   *
   * 它自己单独抓取，只在首次渲染时抓，并在本次会话内缓存 —— release 历史和
   * 更新*检查*（更新器会做节流）是两件不同的事实，所以它绝不能走
   * `updateCheck`：否则点一次「检查更新」就等于「顺带重新抓取 30 份 release
   * 正文」。这里的失败会就地说明并且可以重试，而不是留下一块空面板 —— 那看起来
   * 就像「这个项目从来没有 release」。
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
        // 只显示「上一版 / 当前版 / 最新版」这三个位置，最多三条。
        //
        // 以前是把接口返回的 20 条全部铺出来：一屏塞不满、要滚很久，而且绝大
        // 多数版本与这位用户无关 —— 他真正会关心的是「我正在用的这一版改了什么」
        // 和「下一版会不会修掉我在意的问题」。中间那些已经是历史。
        //
        // 选取顺序（`pickChangelogVersions` 里实现并有单测可查）：
        //   最新版（列表第一条，通常也是最高版本）
        //   当前版（正在运行的那一版）
        //   上一版（当前版之前的那一条 —— 升级上来的人对照用）
        // 去重后取前三；当前版不在列表里（例如本地跑的是未发布的开发版本）时，
        // 「当前版」这一格自然缺席，不会因此少显示一条别的版本。
        const picked = pickChangelogVersions(r, current);
        list.innerHTML = '';
        for (const rel of picked) {
          // 每行不放「打开 release 页面」按钮：二十个那样的按钮，是把同一个动作
          // 重复二十遍。版本号文字本身就是那个链接。
          //
          // 这一行不再是 <details>/<summary> 了。它曾经是，而 <summary> 里那个
          // 可聚焦的链接让 axe 报了 `nested-interactive`（严重级）：两个控件套在
          // 一起，屏幕阅读器会当成一个激活目标。所以行改成始终展开 —— 更新日志
          // 本来就该是这样的 —— 而可滚动的 release notes 带上 `tabindex="0"`，
          // 好让键盘用户能滚动它们（`scrollable-region-focusable`）。
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
          list.appendChild(el('div', { class: 'upd-log-item' },
            el('div', { class: 'upd-log-head' },
              verLabel,
              el('span', { class: 'upd-log-date' }, (rel.date || '').slice(0, 10)),
              String(rel.version) === String(current)
                ? el('span', { class: 'chip ok' }, t('settings.changelogCurrent'))
                : null,
              el('span', { class: 'spacer' }),
              rel.prerelease ? el('span', { class: 'chip warn' }, t('settings.changelogPre')) : null),
            el('pre', {
              class: 'upd-log-notes',
              tabindex: '0',
              'aria-label': t('settings.updateNotesLabel', { version: rel.version })
            }, rel.notes || t('update.noNotes'))));
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

  /* --- 存储策略 ------------------------------------------------------ */

  // 不做备份行：存储层不保留文件副本，旧版本留下的 `backups` 目录会在下次启动
  // 时被删掉（settings.rs 的 `remove_legacy_backups`）。指给用户一个已经不存在
  // 的目录，比什么都不说更糟：用户会去点一个空路径，然后收到一句「找不到」。
  // （旧版本确实留过备份目录，删掉它是为了不再让人以为可以回滚。）
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

  /* --- 适配器 ------------------------------------------------------- */
  const adapters = el('div', { class: 'card' },
    el('div', { class: 'card-head' },
      el('h2', {}, t('settings.adapters')),
      el('span', { class: 'sub spacer' }, t('settings.declared', { n: S.boot.agents.length }))),
    el('div', { class: 'card-body' },
      el('div', { class: 'muted fs-11 mb-8' }, t('settings.adaptersHint')),
      el('div', { class: 'flex flex-wrap gap-sm' },
        ...S.boot.agents.map((a) => el('span', { class: `chip ${a.installed ? 'ok' : 'ghost'}` }, a.label)))
    ));

  wrap.appendChild(appearance);
  // 整行宽度：更新卡片只放版本对比与两个动作；挤在半列里会把它压扁。通道/代
  // 理/令牌这些控件已经挪到它的「更新设置」按钮后面，说明文字在下面的更新日志面板里。
  // controls moved behind its 更新设置 button.
  wrap.appendChild(el('div', { class: 'mt-14' }, updateCard()));
  wrap.appendChild(changelogCard());
  wrap.appendChild(el('div', { class: 'grid grid-2 mt-14' }, storage, adapters));
  return wrap;
}

/**
 * 把主题偏好应用到 <html>。
 *
 * localStorage 优先于持久化设置：它才是用户刚点的那一下，而 settings.json 要等
 * 保存往返之后才刷新。没有这个优先级，重新加载页面会把旧值又盖回来。
 *
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
/* 资产抽屉                                                          */
/* ------------------------------------------------------------------ */

async function openAsset(id) {
  S.selected = id;
  let r;
  try {
    r = await window.ak.readAsset(id);
  } catch (e) {
    // 这里不 catch，rejected 的 invoke 会让整行点击变成死点击：没有面板、没有
    // 提示、什么都没有。
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

  /* 动作 */
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
  // 与右键菜单（`showCtxMenu`）同一道闸：插件缓存或宿主自有的文件没有可写目标，
  // 在那里还摆按钮，只会打开一个空的文件选择器。
  // 按钮因此只在真的有目标时出现。
  if (a.writable && DISTRIBUTE_KEY[a.kind]) {
    actions.appendChild(el('button', { class: 'btn', onclick: () => openDistribute(a) }, t('distribute.go') + '…'));
  }
  if (a.kind !== 'mcp' && a.writable) {
    actions.appendChild(el('button', { class: 'btn danger', onclick: () => doDelete(a) }, t('verb.delete')));
  }
  actions.appendChild(el('button', { class: 'btn ghost', onclick: () => window.ak.openPath(a.file) }, t('assets.reveal')));
  wrap.appendChild(actions);

  /* 编辑器 */
  if (editable) {
    wrap.appendChild(el('div', { class: 'sec-title' }, t('editor.content')));
    const ta = el('textarea', { class: 'editor', id: 'asset-editor' });
    ta.value = r.content;
    ta.addEventListener('input', () => { S.dirty = true; S.editorText = ta.value; });
    wrap.appendChild(ta);
    wrap.appendChild(el('div', { class: 'muted fs-11 mt-6' }, t('drawer.saveHint')));
  } else if (a.kind === 'plugin') {
    wrap.appendChild(el('div', { class: 'sec-title' }, t('drawer.nested', { n: (a.meta.nested || []).length })));
    // .tbl.static：只读清单，行刻意不可点。
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
  // diff 已经跑过，所以对话框能直接点名要改的是哪个文件；写入本身要等用户回答。
  // （先算 diff 再问，是为了让人看清到底会改什么，而不是对着文件名猜。）
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
  // 删除是用户最可能中途走开的那个破坏性动作：即使他没在看窗口，这个确认也必须
  // 送到他面前。
  maybeNotify('notify.deleted', { name: a.name });
  // 资产已经没了，那份没保存的编辑也写不回去了，所以离开时的「放弃未保存」
  // 提示不该再问它。
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
  openModal(t('diff.title', { name: a.name }), body, [el('button', { class: 'btn primary', onclick: () => { closeModal(); saveSelected(); } }, t('diff.applySave'))], { width: 'narrow' });
  run(current);
}

/* ------------------------------------------------------------------ */
/* 编辑器                                                            */
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
  /** 一个真正绑到控件上的 label（点文字能聚焦）。 */
  let fieldSeq = 0;
  const lab = (text, control) => {
    if (!control.id) control.id = `ed-field-${++fieldSeq}`;
    return el('label', { for: control.id }, text);
  };
  const mk = (key, label, hint, value, ph, locked) => {
    const i = el('input', { placeholder: ph || '', value: value ?? '' });
    // 技能名就是目录名。允许改已有资产的这个字段，只会产出一条用户没法处理的
    // 校验错误（「名称必须与目录一致」），所以这个字段是只读的，并把原因写出来。
    // （改名的正确做法是改目录名，引擎会跟着走。）
    //
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
    // 目录级清单：技能的文件夹名来自表单并由引擎校验，在这里拼出 `<dir>\<name>`
    // 只会打印一个根本不会被创建的路径。
    //
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

/** `KEY<sep>value` 对象 ⇄ textarea 行（下面 parseKv 的逆运算）。 */
function kvLines(obj, sep) {
  return Object.entries(obj || {}).map(([k, v]) => k + sep + v).join('\n');
}

/**
 * MCP 服务编辑器。
 *
 * 一个结构化表单，不是 JSON 文本框。引擎已经会校验传输方式（`core/validate.rs`），
 * 而 JSON 文本框等于让用户去当这次集成的最后一道防线，替校验器挡下它本该
 * 提前两栏就拦住的笔误。原始 JSON 框仍保留在「高级」里，给这个表单表达不了的
 * 配置用（自定义头、自定义类型）；用户最后碰过哪一个，就保存哪一个。
 *
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
  // `value` 不是 <textarea> 上的合法属性：el() 设的是属性，文本就永远出不来，
  // 保存时会悄悄丢掉已有的 args / env / headers。这里要赋给 DOM 属性本身。
  //
  const linesBox = (cls, text) => {
    const box = el('textarea', { class: cls });
    box.value = text;
    return box;
  };
  const argsI = linesBox('editor editor-short', (cfg.args || []).join('\n'));
  const urlI = el('input', { value: cfg.url || '', placeholder: 'https://example.com/mcp' });
  const envI = linesBox('editor editor-short', kvLines(cfg.env, '='));
  const headersI = linesBox('editor editor-short', kvLines(cfg.headers, ': '));

  /** 把每个 label 绑到它的控件上，点文字就能聚焦到输入框。 */
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

  /** 当前显示的是哪一段传输配置 —— 这是显示开关，不是重新渲染。 */
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

  /** `KEY<sep>value` 行 ⇄ 对象；空行与 `#` 注释忽略。 */
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
   * 组装要保存的配置：用户动过原始框就用原始框，否则用结构化字段。表单没有建模
   * 的键从原配置里带过来，这样一次编辑不会悄悄把它们丢掉。
   *
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
    // 引擎把这个服务合并进该 Agent 的 MCP 配置文件；是哪个文件由适配器声明，
    // 所以解析方式和分发对话框解析目录的方式一致。
    //
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

/* ---------------- 批量操作 ---------------- */
/**
  * 批量删除。每个资产都走正常的删除路径（各自的备份 + 事务），因为跨异构
  * Agent 的一个大事务比 N 个小的、可回滚的事务更难推理。
  * 只读资产（缓存里的插件）会跳过并给出说明。
  *
 */
/**
  * 串行循环期间把批量操作条标成忙。
 *
  * 以前长批量的写入只有这条操作条作为反馈，而它一直可点：再点一次就会对同一批
  * 资产开第二个循环。现在按钮被禁用，计数变成进度（aria-busy 告诉辅助技术）。
  *
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
  * 批量分发：整批共用一个目标选择器。只有技能/规则/记忆参与 —— 插件是只读的，
  * MCP 配置各有自己的逐服务编辑器。
  *
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
    // 与删除循环同一个理由：这是弹窗还开着时的 N 次串行 IPC 写入，所以按钮必须
    // 显示进度并拒绝再次进入。
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

/* ---------------- Bundle 导入 / 导出 ---------------- */

async function doExportBundle(selected) {
  const ids = selected.map((a) => a.id);
  const r = await window.ak.exportBundle(ids);
  if (!r.ok) return toast(r.canceled ? t('toast.pickCancelled') : r.error, 'warn');
  toast(t('bundle.exported', { n: r.count, out: r.out }), 'ok');
}

async function doImportBundle() {
  // 选择与写入是同一次 IPC 调用，所以只能问一次，而且要在写之前：清单决定每个
  // 资产落到哪个 Agent，而那些目录始终是已安装 Agent 自己的目录。
  //
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

/* ---------------- 右键菜单 ---------------- */

/** 菜单是从哪颗按钮打开的 —— 关掉时焦点要回到那里（菜单本身不在 Tab 序里）。 */
let ctxReturnFocus = null;

/** 关掉菜单并把焦点交还给打开它的那个控件。 */
function hideCtxMenu() {
  const m = $('#ctxmenu');
  if (!m || !m.classList.contains('on')) return;
  m.classList.remove('on');
  const back = ctxReturnFocus;
  ctxReturnFocus = null;
  if (back && document.body.contains(back)) back.focus({ preventScroll: true });
}

/**
 * 菜单内的方向键导航。
 *
 * `role="menu"` + `role="menuitem"` 不只是两个属性：ARIA APG 要求菜单里
 * ↑ / ↓ 在条目之间移动、Home / End 跳到首尾、Tab 离开时必须关闭菜单。上一版
 * 只加了角色和「焦点进第一项」，方向键什么都不做 —— 那比不加角色更糟：读屏
 * 会宣布这是一个菜单，而按键不按菜单的方式工作。
 *
 * 事件挂在容器上（条目每次重建，容器是稳定的），所以不需要在每次
 * `showCtxMenu` 里重新绑定。
 */
function initCtxMenuKeyboard() {
  const m = $('#ctxmenu');
  if (!m || m.dataset.kbWired === '1') return;
  m.dataset.kbWired = '1';
  m.addEventListener('keydown', (e) => {
    const items = [...m.querySelectorAll('button:not([disabled])')];
    if (!items.length) return;
    const i = items.indexOf(document.activeElement);
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      const next = e.key === 'ArrowDown'
        ? (i < 0 ? 0 : (i + 1) % items.length)
        : (i <= 0 ? items.length - 1 : i - 1);
      items[next].focus();
    } else if (e.key === 'Home') {
      e.preventDefault();
      items[0].focus();
    } else if (e.key === 'End') {
      e.preventDefault();
      items[items.length - 1].focus();
    } else if (e.key === 'Tab') {
      // 焦点离开菜单 = 菜单该关了；否则一个看不见的浮层还留在屏幕上。
      hideCtxMenu();
    }
  });
}

/**
 * 资产行的动作菜单。不适用的条目直接不出现。
 *
 * 两个入口：右键（`e` 是鼠标事件，菜单落在指针处）和行末的「⋯」按钮
 * （`fromButton`，菜单对齐到那颗按钮的左下角）。用同一个函数是因为条目必须
 * 一模一样 —— 两个入口各写一份清单，迟早会漏掉其中一条。
 */
function showCtxMenu(e, a, fromButton = false) {
  e.preventDefault();
  const m = $('#ctxmenu');
  if (!m) return;
  // 再点一次同一个「⋯」= 关闭。以前再点只是把菜单重建一遍、停在原处，读起来
  // 像「点了没反应」——而它明明是一个开关式的展开面板。
  if (fromButton && m.classList.contains('on') && ctxReturnFocus === e.currentTarget) {
    hideCtxMenu();
    return;
  }
  m.innerHTML = '';
  ctxReturnFocus = fromButton ? e.currentTarget : document.activeElement;

  const item = (label, fn, cls = '') => {
    const b = el('button', { class: cls, role: 'menuitem', tabindex: '-1' }, label);
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
  m.setAttribute('role', 'menu');
  m.setAttribute('aria-label', t('row.more', { name: a.name }));

  // 先量再显示（定位时菜单是 display:none），然后再显示 ——
  // 先显示再量会在 0,0 位置闪一帧。
  m.classList.add('on');
  const r = m.getBoundingClientRect();
  m.classList.remove('on');
  let left, top;
  if (fromButton) {
    const b = e.currentTarget.getBoundingClientRect();
    left = Math.min(b.left, window.innerWidth - r.width - 8);
    top = Math.min(b.bottom + 4, window.innerHeight - r.height - 8);
  } else {
    left = Math.min(e.clientX, window.innerWidth - r.width - 8);
    top = Math.min(e.clientY, window.innerHeight - r.height - 8);
  }
  m.style.left = Math.max(8, left) + 'px';
  m.style.top = Math.max(8, top) + 'px';
  m.classList.add('on');
  // 两个入口都把焦点送进菜单第一项。
  //
  // 从「⋯」打开时这是必需的（否则 Tab 会从按钮继续往后走，一个浮层留在屏幕上
  // 却够不到）。从右键打开时同样必需：`role="menu"` 一旦出现在无障碍树里，键盘
  // 用户就必须能立刻用方向键 —— 而方向键事件挂在容器上，焦点还在页面上时收不到。
  const first = m.querySelector('button');
  if (first) first.focus({ preventScroll: true });
}

/**
 * 装着各类可分发资产的适配器目录清单。
 *
 * 上面的 `targetPicker` 一直用复数形式的名字，这个选择器却用了单数
 * （`a.globals['skill']`），而 bootstrap 载荷里根本没有这个键 —— 于是每个目标
 * 都被过滤掉，对话框是空的。映射要放在一处，两个选择器不能再走偏。
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
  // 项目目标是「已登记项目 × 已安装 Agent」：引擎把它解析成 registry[projectId] 加上
  // 该 Agent 的项目内目录，所以 Agent 那一半必须来自被点的那一行，不能写死 id。
  //
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
/* 启动                                                                */
/* ------------------------------------------------------------------ */

/**
  * 把启动失败画到主区域里。
 *
  * 用普通 DOM 画（而不是 render()），因为 boot() 失败时还没有任何数据可渲染。
  * 重试按钮重跑 boot()（bootstrap + 扫描），所以一次偶发的 IPC 失败不再需要
  * 用户重启整个应用。
 */
function showStartupError(e) {
  const root = document.getElementById('view-root');
  if (!root) return;
  if (!window.__akI18n) {
    // 不降级原则的最后手段：连字典都没加载成功，t() 本身就会抛。这里用纯中文、
    // 手动转义、不做重试（没有可靠的翻译器可以用来拼按钮文字）。
    //
    root.innerHTML = `<div class="alert err">启动失败：${esc(zhText(e && e.message))}</div>`;
    return;
  }
  root.innerHTML = '';
  root.appendChild(errorPanel(t('err.startupFailed', { msg: zhError(e) }), retryBoot));
}

/** 启动面板的重试入口；重复失败也用同样的方式上报。 */
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

// 在第一次 boot 失败之前就给抽屉的 inert 分支打上标记：即使窗口卡在启动错误
// 画面上，探针也必须能分出原生分支和兜底分支。boot() 会重复这一步，
// 而 setDrawerInert() 是幂等的。
setDrawerInert(true);
wireErrorReporting();
startBoot();
