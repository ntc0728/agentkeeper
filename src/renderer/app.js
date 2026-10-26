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
  notifyWired: false
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

/* ------------------------------------------------------------------ */
/* 错误上报                                                             */
/* ------------------------------------------------------------------ */

/**
 * 渲染层异常 → 主线程日志（`<storeRoot>/ui-errors.jsonl`）。
 *
 * 为什么要有：以前这个窗口里抛的异常只在 WebView2 的 devtools 里留痕，一旦打包
 * 就没人看得见。外壳每条上报追加一行 JSON（只保留最新 200 条，永不 panic）。
 *
 * 上报的字段结构是 Rust 命令定死的：
 *   { kind, message, stack, view, at }
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
    // unhandled rejection。
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
/* toast / modal                                                       */
/* ------------------------------------------------------------------ */

function toast(msg, kind = '') {
  // 引擎的错误是英文过来的；中文界面绝不能直接把原文显示出来。
  msg = zhError(msg);
  // 叫 `node` 而不是 `t` —— `t` 是模块作用域里的 i18n 翻译函数。
  const node = el('div', { class: `toast ${kind}` }, msg);
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
  ]);
  toast(t(symlink ? 'restore.toast.symlink' : 'restore.toast.partial'), 'err');
}

/* 主线程在一次失败的事务上抛出的错误码（store.rs）。这里故意写成字面量：bridge
 * 跨的是字符串边界，一个文件里打错字的话，表现会是「落到通用兜底」而不是「缺了
 * 一条翻译」。`scripts/check.js bridge` 只校验调用形状、不校验取值，所以这份清单
 * 是人工对着 store.rs 核对的。 */
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
    openModal(title, body, [cancel, ok]);
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
  openModal(t('update.dialogTitle'), body, [skip, later, goBtn]);
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
  openModal(t('update.readyTitle'), body, [later, goBtn]);
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
      render();
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
        // `title`: the name column ellipsizes at narrow widths, and a
        // truncated skill name is not recoverable from anywhere else in the row.
        el('div', { class: 't-name', title: a.name }, a.name),
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
        el('div', { class: 't-name', title: p.name }, p.name),
        el('div', { class: 't-mono', title: p.dir }, p.dir)
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

    // No "open the release page" button on this card: the changelog panel
    // below already links every version to its release page, and the card stays
    // about the decision (compare, check, settings).

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
          // No per-row "open the release page" button: twenty of them are one
          // action repeated twenty times. The version label itself is the link.
          //
          // The row is NOT a <details>/<summary> any more. It was, and the
          // focusable link inside <summary> made axe report `nested-interactive`
          // (serious): two controls nested in one, which screen readers read as
          // one activation target. Rows are always expanded instead — that is
          // what a changelog is for — and the scrollable notes carry
          // `tabindex="0"` so a keyboard user can scroll them
          // (`scrollable-region-focusable`).
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

  wrap.appendChild(appearance);
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
