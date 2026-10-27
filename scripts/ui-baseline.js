#!/usr/bin/env node
'use strict';
/**
 * 视觉回归冒烟基线。
 *
 *   node scripts/ui-baseline.js capture          # 写入本机 .ui-baseline/（不入库）
 *   node scripts/ui-baseline.js compare          # 与基线比像素
 *   node scripts/ui-baseline.js compare --strict # 有漂移就以非 0 退出
 *
 * 通过 WebView2 的调试协议，在同一尺寸下截同样几个页面，再比像素。零依赖：下面
 * 这个 PNG 解码器只处理 `Page.captureScreenshot` 真正会产出的格式（8 位真彩色，
 * 非隔行），不为四个文件拖一整个图像库进来。
 *
 * ## 怎么跑 —— 应用必须先开着调试端口
 *
 *   set WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222
 *   tauri-shell\target\release\agentkeeper-shell.exe
 *   node scripts/ui-baseline.js capture
 *
 * ## 这条基线是、以及不是什么
 *
 * 注意：设置页在**无外网或证书链不被信任**的机器上会多出一条「检查失败」错误横幅，
 * 这条横幅会让该页整块下移、报 10% 以上的漂移。它不是 UI 回归 —— 重拍基线前先确认
 * 应用能正常检查更新，否则你比对的是网络状态，不是布局。
 *
 * 它是**冒烟**基线：能抓到「卡片少了内边距」「更新日志两列挤成一列」这类事故，
 * 抓不到两个像素的字距变化。页面里全是活数据 —— 资产总数、扫描时间、版本号 ——
 * 换一台机器、或者本机资产变了，侧栏徽标和统计行就会报漂移。所以 `compare` 默认
 * 只给结论不拦人，阈值也只是参考：看图判断，不要盲信数字。把这些易变区域遮掉是
 * 下一步；假装它们稳定不是。
 *
 * 退出码：0 = 在阈值内（或仅提示），1 = `--strict` 且超阈值，2 = 用法/连接失败。
 */

const fs = require('fs');
const path = require('path');
const zlib = require('zlib');
const http = require('http');

const ROOT = path.join(__dirname, '..');
/**
 * 基线目录放在本机、不入库。
 *
 * 截图里带着本机的资产数量、安装路径与时间戳，换一台机器必然不同；把它提交进公开
 * 仓库只会塞进一堆机器相关的大二进制，而且永远对不上别人的机器。它是开发者的本机
 * 回归工具，不是仓库产物。
 */
const BASELINE = path.join(ROOT, '.ui-baseline');
const TMP = path.join(ROOT, '.ui-baseline-tmp');
const PORT = Number(process.env.AK_CDP_PORT || 9222);

/** 固定几何：基线只在一个尺寸下有意义。 */
const VIEWPORT = { width: 1440, height: 900 };
/** 页面列表：`[文件名, 导航 data-view]`，取自运行中的导航按钮。 */
const VIEWS = [['overview', 'overview'], ['assets', 'assets'], ['settings', 'settings']];
/** 单像素三通道差值超过它，才算这个像素变了。 */
const PIXEL_TOLERANCE = 12;
/** 变化像素占比超过它，就认为这一页漂移了。 */
const DEFAULT_THRESHOLD = 0.02;
/**
 * 易变区域：截图前先隐藏。
 *
 * 页面里全是活数据 —— 资产总数、统计数字、发布日期、机器相关的路径 —— 不遮掉它们，
 * 换台机器或本机资产一变，基线就报漂移，而那种漂移没有任何信息量。选择器按语义列，
 * 不按坐标：坐标随布局走，改一次样式就失效。用 `visibility` 而不是 `display`，
 * 是为了保住占位 —— 元素一消失布局就重排，反而看不出真正的破版。
 */
const VOLATILE_SELECTORS = [
  '.nav-badge',        // 资产 · 904、健康检查 · 2
  '.num',              // 表格里的计数列
  '.upd-log-date',     // 发布说明的发布日期
  '.kv dd',            // 存储策略里的绝对路径
  // 侧栏底部状态条：「扫描于 3 分钟前 · 2 项待处理」。时间与计数都是活数据 ——
  // 换台机器、或者隔几分钟再拍一次，文字必然不同。但它的**布局**（条高、圆点
  // 位置、窄轨道下是否只剩短读数）仍然该被基线守住，所以只藏文字、不藏容器。
  '.side-foot-text',
  '.side-foot-short'
];
/**
 * 兜底：按文本盖。选择器会漏 —— 同一个数字在这页是 `.num`，在另一页只是表格里一个
 * `<td>`。凡是**叶子节点**且整段文本就是「数」（计数 `904`、体积 `1.5 KB`、
 * `103.1M`）、日期（`2026-10-26`）或相对时间（`3 分钟前`）的，都算活数据。
 * 仍然只盖叶子，布局照旧 —— 这也是为什么用 `visibility` 而不是 `display`。
 */
const VOLATILE_PATTERNS = [
  '^[\\d.,\\s]*\\d[\\d.,\\s]*(?:[A-Za-z%]+)?$', // 904 / 1.5 KB / 103.1M
  '\\d{4}-\\d{1,2}-\\d{1,2}',                   // 2026-10-26
  '\\d+\\s*(?:秒|分钟|小时|天)前'                // 3 分钟前
];
/**
 * 按视图追加的遮罩：整块都是活数据的地方。
 *
 * 首页那两张表（按 Agent 统计、最近改动）每一行都跟着扫描结果变，遮零散数字挡不住
 * —— 换一批资产，行数、名称、时间全变。所以整个表体都藏，只留下卡片边框与标题，
 * 那部分才是布局回归要看的地方。
 *
 * 资产页同理，而且更彻底：903 行里的**名称列**是这台机器上装了什么技能/插件，
 * 编号、agent 名、体积全都跟着机器走。实测遮完数字之后，资产页仍有 61 个名称类
 * 文本叶子可见 —— 那才是这条基线换台机器就对不上的真正原因（原来的注释把它归因
 * 于「资产数量与路径」，只对了一半）。表体整块遮掉之后，基线守的是**骨架**：
 * 列宽、行高、表头、筛选条、分页条的位置。
 */
const VOLATILE_BY_VIEW = {
  overview: ['table.tbl'],
  assets: ['table.tbl tbody'],
};

async function hideVolatile(evaluate, view) {
  // `AK_BASELINE_NO_MASK=1` 只用于自检：把「遮罩后」与「遮罩前」各截一次做对比，
  // 用像素差证明遮罩真的盖住了这些区域。正常跑 capture/compare 时不要开。
  if (process.env.AK_BASELINE_NO_MASK) return 'masking disabled by AK_BASELINE_NO_MASK';
  const selectors = JSON.stringify([...VOLATILE_SELECTORS, ...(VOLATILE_BY_VIEW[view] || [])]);
  const patterns = JSON.stringify(VOLATILE_PATTERNS);
  return evaluate(`(() => {
    const patterns = ${patterns}.map((p) => new RegExp(p, 'u'));
    const isVolatileText = (text) => {
      for (const re of patterns) if (re.test(text)) return true;
      return false;
    };
    let hidden = 0;
    for (const selector of ${selectors}) {
      for (const node of document.querySelectorAll(selector)) {
        // 整块藏（表体）与零散藏（叶子）都保住占位，所以用 visibility 而不是
        // display：元素一消失布局就重排，那就不是「布局没变」的证据了。
        node.style.setProperty('visibility', 'hidden');
        hidden++;
      }
    }
    for (const node of document.querySelectorAll('*')) {
      if (node.children.length) continue;
      const text = (node.textContent || '').trim();
      if (!text || !isVolatileText(text)) continue;
      node.style.setProperty('visibility', 'hidden');
      hidden++;
    }
    return hidden + ' node(s) hidden';
  })()`);
}

/* -------------------------------------------------------------- PNG 解码 */

function decodePng(buf) {
  if (buf.readUInt32BE(0) !== 0x89504e47) throw new Error('not a PNG');
  let off = 8;
  let width = 0;
  let height = 0;
  let colourType = 0;
  const idat = [];
  while (off + 8 <= buf.length) {
    const len = buf.readUInt32BE(off);
    const type = buf.toString('ascii', off + 4, off + 8);
    const data = buf.subarray(off + 8, off + 8 + len);
    if (type === 'IHDR') {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      if (data[8] !== 8) throw new Error(`unsupported PNG: bit depth ${data[8]}`);
      colourType = data[9];
      // `Page.captureScreenshot` 输出真彩色：页面不透明时是 RGB，透明时是 RGBA。
      // 两者都是逐行未隔行，一个解码器就够 —— 只有每像素字节数不同。
      if (colourType !== 2 && colourType !== 6) {
        throw new Error(`unsupported PNG colour type ${colourType}`);
      }
    } else if (type === 'IDAT') idat.push(data);
    else if (type === 'IEND') break;
    off += 12 + len;
  }
  const raw = zlib.inflateSync(Buffer.concat(idat));
  const bpp = colourType === 6 ? 4 : 3;
  const stride = width * bpp;
  const out = Buffer.alloc(height * stride);
  let p = 0;
  for (let y = 0; y < height; y++) {
    const filter = raw[p++];
    const line = raw.subarray(p, p + stride);
    p += stride;
    const prev = y ? out.subarray((y - 1) * stride, y * stride) : Buffer.alloc(stride);
    const cur = out.subarray(y * stride, (y + 1) * stride);
    for (let x = 0; x < stride; x++) {
      const a = x >= bpp ? cur[x - bpp] : 0;
      const b = prev[x];
      const c = x >= bpp ? prev[x - bpp] : 0;
      const v = line[x];
      let value;
      if (filter === 0) value = v;
      else if (filter === 1) value = v + a;
      else if (filter === 2) value = v + b;
      else if (filter === 3) value = v + ((a + b) >> 1);
      else if (filter === 4) {
        const pa = Math.abs(b - c);
        const pb = Math.abs(a - c);
        const pc = Math.abs(a + b - 2 * c);
        value = v + (pa <= pb && pa <= pc ? a : pb <= pc ? b : c);
      } else throw new Error(`unknown PNG filter ${filter}`);
      cur[x] = value & 0xff;
    }
  }
  return { width, height, data: out };
}

function diffRatio(a, b) {
  if (a.width !== b.width || a.height !== b.height) return 1;
  const pixels = a.width * a.height;
  let changed = 0;
  for (let i = 0; i < pixels; i++) {
    const o = i * 4;
    const d = Math.abs(a.data[o] - b.data[o])
      + Math.abs(a.data[o + 1] - b.data[o + 1])
      + Math.abs(a.data[o + 2] - b.data[o + 2]);
    if (d > PIXEL_TOLERANCE) changed++;
  }
  return changed / pixels;
}

/* ------------------------------------------------------------ 调试协议 */

const get = (url) => new Promise((resolve, reject) => {
  http.get(url, (res) => {
    let body = '';
    res.on('data', (c) => (body += c));
    res.on('end', () => resolve(body));
  }).on('error', reject);
});

async function connect() {
  let targets;
  try {
    targets = JSON.parse(await get(`http://127.0.0.1:${PORT}/json`));
  } catch (e) {
    throw new Error(
      `no WebView2 devtools endpoint on 127.0.0.1:${PORT} — start the app with\n` +
        '  set WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222'
    );
  }
  const target = targets.find((t) => t.type === 'page');
  if (!target) throw new Error('no page target — is the window open?');
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  let id = 0;
  const pending = new Map();
  ws.addEventListener('message', (event) => {
    const msg = JSON.parse(event.data);
    if (msg.id && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  });
  await new Promise((resolve) => ws.addEventListener('open', resolve));
  const send = (method, params = {}) => new Promise((resolve) => {
    const i = ++id;
    pending.set(i, resolve);
    ws.send(JSON.stringify({ id: i, method, params }));
  });
  const evaluate = async (expression) => {
    const r = await send('Runtime.evaluate', { expression, returnByValue: true });
    const details = r.result && r.result.exceptionDetails;
    if (details) {
      const detail = (details.exception && details.exception.description) || details.text || 'unknown';
      throw new Error(`CDP evaluate failed: ${detail}`);
    }
    return r.result && r.result.result ? r.result.result.value : undefined;
  };
  return { send, evaluate, close: () => ws.close() };
}

const wait = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * 等到视图**渲染完成**，而不是等 1400 ms。
 *
 * 固定 sleep 是这里唯一的假红来源：慢机器上 1400 ms 可能还在画第一行，快机器上
 * 早就稳定了 —— 而两次截图落在渲染的不同阶段，比对出来的「漂移」是时间差，不是
 * 布局变化（同一份数据连跑 capture + compare 就会报出可观的漂移）。
 *
 * 判据是「骨架屏没了 + 视图里有真实的卡/表 + 扫描条已收」，与
 * `real-ui-probe.mjs` 的 `waitReady()` 同一套信号。连续两帧同一结论才算稳定。
 */
async function waitRendered(evaluate, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const settled = await evaluate(`(async () => {
      const root = document.getElementById('view-root');
      if (!root || !root.children.length) return false;
      if (root.querySelector('.skeleton')) return false;
      if (document.querySelector('#scanbar:not(.hidden)')) return false;
      if (!root.querySelector('.card')) return false;
      // 两帧同一结论：单帧可能在布局中途。
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      return true;
    })()`).catch(() => false);
    if (settled) return true;
    await wait(80);
  }
  return false;
}

async function captureViews(dir) {
  fs.mkdirSync(dir, { recursive: true });
  const { send, evaluate, close } = await connect();
  await send('Runtime.enable');
  await send('Page.enable');
  await send('Emulation.setDeviceMetricsOverride', { ...VIEWPORT, deviceScaleFactor: 1, mobile: false });
  const written = [];
  for (const [name, view] of VIEWS) {
    await evaluate(`document.querySelector('[data-view="${view}"]').click()`);
    await waitRendered(evaluate);
    // 每次渲染后重新隐藏：换个页面 DOM 就是新的，之前设的 visibility 早就没了。
    await hideVolatile(evaluate, view);
    const shot = await send('Page.captureScreenshot', { format: 'png' });
    const file = path.join(dir, `${name}-${VIEWPORT.width}x${VIEWPORT.height}.png`);
    fs.writeFileSync(file, Buffer.from(shot.result.data, 'base64'));
    written.push(file);
    console.log(`  已截图 ${path.relative(ROOT, file)}`);
  }
  await send('Emulation.clearDeviceMetricsOverride');
  close();
  return written;
}

/* ------------------------------------------------------------------ 入口 */

async function main(argv) {
  const command = argv[0];
  if (command === 'capture') {
    console.log(`[ui-baseline] capture ${VIEWPORT.width}x${VIEWPORT.height} → ${path.relative(ROOT, BASELINE)}`);
    await captureViews(BASELINE);
    console.log('[ui-baseline] 基线已更新（本机，不进仓库）；改动确认无误后再 commit 代码本身');
    return 0;
  }
  /**
   * `mask-check` —— 证明遮罩真的盖住了活数据。
   *
   * 拿「遮罩后」的基线和「不遮罩」的一次新截图比像素。这个差值就是被遮罩覆盖的
   * 面积：数字越大，说明越多活数据不会误报漂移。反过来，如果这里是 0%，说明
   * VOLATILE_SELECTORS 一个都没命中，遮罩是摆设。
   */
  if (command === 'mask-check') {
    if (!fs.existsSync(BASELINE)) {
      console.error('[ui-baseline] 还没有基线 —— 先跑 capture');
      return 2;
    }
    const masked = TMP;
    const unmasked = TMP + '-nomask';
    await captureViews(masked);
    process.env.AK_BASELINE_NO_MASK = '1';
    await captureViews(unmasked);
    let worst = 0;
    for (const [name] of VIEWS) {
      const file = `${name}-${VIEWPORT.width}x${VIEWPORT.height}.png`;
      const a = decodePng(fs.readFileSync(path.join(masked, file)));
      const b = decodePng(fs.readFileSync(path.join(unmasked, file)));
      const ratio = diffRatio(a, b);
      worst = Math.max(worst, ratio);
      console.log(`  遮罩覆盖 ${file}  ${(ratio * 100).toFixed(2)}%`);
    }
    fs.rmSync(masked, { recursive: true, force: true });
    fs.rmSync(unmasked, { recursive: true, force: true });
    console.log(`[ui-baseline] 遮罩面积最大 ${(worst * 100).toFixed(2)}%`);
    return worst > 0 ? 0 : 1;
  }
  if (command === 'compare') {
    const strict = argv.includes('--strict');
    const at = argv.indexOf('--threshold');
    const threshold = at >= 0 ? Number(argv[at + 1]) : DEFAULT_THRESHOLD;
    if (!fs.existsSync(BASELINE)) {
      console.error('[ui-baseline] no baseline yet — run `capture` first');
      return 2;
    }
    await captureViews(TMP);
    let worst = 0;
    let drifted = 0;
    for (const [name] of VIEWS) {
      const file = `${name}-${VIEWPORT.width}x${VIEWPORT.height}.png`;
      const base = decodePng(fs.readFileSync(path.join(BASELINE, file)));
      const now = decodePng(fs.readFileSync(path.join(TMP, file)));
      const ratio = diffRatio(base, now);
      worst = Math.max(worst, ratio);
      const over = ratio > threshold;
      if (over) drifted++;
      console.log(`  ${over ? 'DRIFT' : 'ok   '} ${file}  ${(ratio * 100).toFixed(2)}% of pixels changed`);
    }
    fs.rmSync(TMP, { recursive: true, force: true });
    console.log(`[ui-baseline] worst ${(worst * 100).toFixed(2)}% (threshold ${(threshold * 100).toFixed(2)}%)`);
    if (drifted && strict) {
      console.error('[ui-baseline] FAILED — review the diff, then re-capture if the change is intended');
      return 1;
    }
    if (drifted) console.log('[ui-baseline] advisory run — pass --strict to make drift an error');
    return 0;
  }
  console.error('usage: node scripts/ui-baseline.js <capture|compare> [--strict] [--threshold 0.02]');
  return 2;
}

main(process.argv.slice(2)).then(
  (code) => {
    process.exitCode = code;
  },
  (e) => {
    console.error(`[ui-baseline] ${(e && (e.stack || e.message)) || e}`);
    process.exitCode = 2;
  }
);