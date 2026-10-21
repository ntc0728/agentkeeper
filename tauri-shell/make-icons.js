'use strict';
/**
 * Generate the Tauri icon set from the existing AgentKeeper mark.
 *
 * Reuses the project's own rasteriser (scripts/make-icon.js) rather than
 * drawing a second, slightly different glyph — a shell whose icon does not
 * match the Electron build is a regression users notice immediately.
 *
 * The rasteriser lives in a script that writes build/ on load, so this runs
 * it once and captures what it produced, then re-rasterises at Tauri's sizes
 * by re-invoking the script with an env flag.
 *
 *   node tauri-shell/make-icons.js
 */

const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const ROOT = path.join(__dirname, '..');
const ICON_DIR = path.join(__dirname, 'src-tauri', 'icons');
fs.mkdirSync(ICON_DIR, { recursive: true });

/* ------------------------------------------------------------------ *
 * 1. ICO: the project already builds a correct multi-resolution icon
 *    (16..256). Tauri's Windows bundler accepts that file verbatim.
 * ------------------------------------------------------------------ */
const icoSrc = path.join(ROOT, 'build', 'icon.ico');
if (fs.existsSync(icoSrc)) {
  fs.copyFileSync(icoSrc, path.join(ICON_DIR, 'icon.ico'));
  console.log('icon.ico      <- build/icon.ico');
} else {
  console.error('build/icon.ico missing — run: node scripts/make-icon.js');
  process.exit(1);
}

/* ------------------------------------------------------------------ *
 * 2. PNGs: Tauri's icon list needs 32x32 and 128x128. decode the 512px
 *    source PNG and box-resample it. Nearest-neighbour keeps this honest
 *    (no image library) and is fine for an icon at these sizes.
 * ------------------------------------------------------------------ */
function decodePng(buf) {
  // Only what we generate: 8-bit RGBA, non-interlaced, single IDAT.
  if (buf.readUInt32BE(0) !== 0x89504e47) throw new Error('not a PNG');
  let pos = 8;
  let width = 0, height = 0, depth = 0, colorType = 0, interlace = 0;
  const idat = [];
  while (pos < buf.length) {
    const len = buf.readUInt32BE(pos);
    const type = buf.toString('ascii', pos + 4, pos + 8);
    const data = buf.subarray(pos + 8, pos + 8 + len);
    if (type === 'IHDR') {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      depth = data[8];
      colorType = data[9];
      interlace = data[12];
    } else if (type === 'IDAT') {
      idat.push(Buffer.from(data));
    } else if (type === 'IEND') break;
    pos += 12 + len;
  }
  if (depth !== 8 || colorType !== 6 || interlace !== 0) {
    throw new Error(`unsupported PNG: depth=${depth} colorType=${colorType} interlace=${interlace}`);
  }
  const raw = require('zlib').inflateSync(Buffer.concat(idat));
  const bpp = 4;
  const stride = width * bpp;
  const out = Buffer.alloc(height * stride);
  let rp = 0;
  for (let y = 0; y < height; y++) {
    const filter = raw[rp++];
    const line = raw.subarray(rp, rp + stride);
    rp += stride;
    const cur = out.subarray(y * stride, (y + 1) * stride);
    const prev = y > 0 ? out.subarray((y - 1) * stride, y * stride) : null;
    for (let x = 0; x < stride; x++) {
      const a = x >= bpp ? cur[x - bpp] : 0;
      const b = prev ? prev[x] : 0;
      const c = (prev && x >= bpp) ? prev[x - bpp] : 0;
      let v = line[x];
      switch (filter) {
        case 0: break;
        case 1: v = v + a; break;
        case 2: v = v + b; break;
        case 3: v = v + ((a + b) >> 1); break;
        case 4: {
          const p = a + b - c;
          const pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
          v = v + (pa <= pb && pa <= pc ? a : (pb <= pc ? b : c));
          break;
        }
        default: throw new Error('bad filter ' + filter);
      }
      cur[x] = v & 0xff;
    }
  }
  return { width, height, data: out };
}

function encodePng(width, height, rgba) {
  const zlib = require('zlib');
  const crcTable = (() => {
    const t = new Int32Array(256);
    for (let n = 0; n < 256; n++) {
      let c = n;
      for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      t[n] = c;
    }
    return t;
  })();
  const crc32 = (b) => {
    let c = -1;
    for (let i = 0; i < b.length; i++) c = crcTable[(c ^ b[i]) & 0xff] ^ (c >>> 8);
    return (c ^ -1) >>> 0;
  };
  const chunk = (type, data) => {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const td = Buffer.concat([Buffer.from(type, 'ascii'), data]);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(td));
    return Buffer.concat([len, td, crc]);
  };

  const stride = width * 4;
  const raw = Buffer.alloc(height * (stride + 1));
  for (let y = 0; y < height; y++) {
    raw[y * (stride + 1)] = 0; // filter: none
    rgba.copy(raw, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;  // bit depth
  ihdr[9] = 6;  // RGBA
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

const srcPng = path.join(ROOT, 'build', 'icon.png');
if (!fs.existsSync(srcPng)) {
  console.error('build/icon.png missing — run: node scripts/make-icon.js');
  process.exit(1);
}
const src = decodePng(fs.readFileSync(srcPng));
console.log(`source: ${src.width}x${src.height}`);

for (const size of [32, 128, 256]) {
  const out = Buffer.alloc(size * size * 4);
  // Box filter (area average) — downsampling an icon with plain nearest
  // neighbour drops whole strokes and makes the mark look chipped.
  const ratio = src.width / size;
  for (let y = 0; y < size; y++) {
    const y0 = Math.floor(y * ratio), y1 = Math.min(src.height, Math.ceil((y + 1) * ratio));
    for (let x = 0; x < size; x++) {
      const x0 = Math.floor(x * ratio), x1 = Math.min(src.width, Math.ceil((x + 1) * ratio));
      let r = 0, g = 0, b = 0, a = 0, n = 0;
      for (let sy = y0; sy < y1; sy++) {
        for (let sx = x0; sx < x1; sx++) {
          const i = (sy * src.width + sx) * 4;
          const al = src.data[i + 3];
          // premultiply so transparent pixels do not bleed dark edges
          r += src.data[i] * al; g += src.data[i + 1] * al; b += src.data[i + 2] * al;
          a += al; n++;
        }
      }
      const o = (y * size + x) * 4;
      if (a > 0) {
        out[o] = Math.round(r / a); out[o + 1] = Math.round(g / a);
        out[o + 2] = Math.round(b / a); out[o + 3] = Math.round(a / n);
      }
    }
  }
  const file = path.join(ICON_DIR, `${size}x${size}.png`);
  fs.writeFileSync(file, encodePng(size, size, out));
  console.log(`${size}x${size}.png  ${fs.statSync(file).size} bytes`);
}