#!/usr/bin/env node
'use strict';
/**
 * 视觉回归冒烟基线。
 *
 *   node scripts/ui-baseline.js capture          # 写入 docs/ui-baseline/
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
const BASELINE = path.join(ROOT, 'docs', 'ui-baseline');
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

async function captureViews(dir) {
  fs.mkdirSync(dir, { recursive: true });
  const { send, evaluate, close } = await connect();
  await send('Runtime.enable');
  await send('Page.enable');
  await send('Emulation.setDeviceMetricsOverride', { ...VIEWPORT, deviceScaleFactor: 1, mobile: false });
  const written = [];
  for (const [name, view] of VIEWS) {
    await evaluate(`document.querySelector('[data-view="${view}"]').click()`);
    await wait(1400);
    const shot = await send('Page.captureScreenshot', { format: 'png' });
    const file = path.join(dir, `${name}-${VIEWPORT.width}x${VIEWPORT.height}.png`);
    fs.writeFileSync(file, Buffer.from(shot.result.data, 'base64'));
    written.push(file);
    console.log(`  captured ${path.relative(ROOT, file)}`);
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
    console.log('[ui-baseline] baseline written — commit it with the change it belongs to');
    return 0;
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