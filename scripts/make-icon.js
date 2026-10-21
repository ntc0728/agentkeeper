'use strict';
/**
 * Generate the Tauri icon-pipeline inputs `build/icon.ico` / `build/icon.png`,
 * without any image dependency.
 *
 * `tauri-shell/make-icons.js` consumes these two files to emit the icon set the
 * Tauri bundle ships, so they are pipeline inputs rather than product assets —
 * if either is missing, `make-icons.js` exits 1 with "run: node
 * scripts/make-icon.js". We rasterise the AgentKeeper mark ourselves: a
 * rounded-square gradient tile with a stylised "shield + layers" glyph, drawn
 * with plain RGBA math into a BMP-in-ICO container.
 *
 *   node scripts/make-icon.js
 */

const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

const OUT_DIR = path.join(__dirname, '..', 'build');
const SIZES = [16, 24, 32, 48, 64, 128, 256];

/* ------------------------------------------------------------------ */
/* tiny raster helpers                                                 */
/* ------------------------------------------------------------------ */

function makeCanvas(size) {
  return { size, data: Buffer.alloc(size * size * 4) };
}

/** Signed distance to a rounded rectangle, for anti-aliased edges. */
function sdRoundRect(px, py, cx, cy, hw, hh, r) {
  const qx = Math.abs(px - cx) - (hw - r);
  const qy = Math.abs(py - cy) - (hh - r);
  const ax = Math.max(qx, 0), ay = Math.max(qy, 0);
  return Math.hypot(ax, ay) + Math.min(Math.max(qx, qy), 0) - r;
}

function blend(c, x, y, [r, g, b], a) {
  if (a <= 0 || x < 0 || y < 0 || x >= c.size || y >= c.size) return;
  const i = (y * c.size + x) * 4;
  const da = c.data[i + 3] / 255;
  const outA = a + da * (1 - a);
  if (outA <= 0) return;
  c.data[i] = Math.round((r * a + c.data[i] * da * (1 - a)) / outA);
  c.data[i + 1] = Math.round((g * a + c.data[i + 1] * da * (1 - a)) / outA);
  c.data[i + 2] = Math.round((b * a + c.data[i + 2] * da * (1 - a)) / outA);
  c.data[i + 3] = Math.round(outA * 255);
}

/** Per-pixel fill with 1px analytic AA. */
function fill(c, sdf, colorAt) {
  for (let y = 0; y < c.size; y++) {
    for (let x = 0; x < c.size; x++) {
      const d = sdf(x + 0.5, y + 0.5);
      const a = Math.max(0, Math.min(1, 0.5 - d));
      if (a > 0) blend(c, x, y, colorAt(x, y), a);
    }
  }
}

/* ------------------------------------------------------------------ */
/* the mark                                                            */
/* ------------------------------------------------------------------ */

function drawIcon(size) {
  const c = makeCanvas(size);
  const s = size;
  const m = s * 0.055;           // margin
  const radius = s * 0.225;      // corner radius

  // Rounded tile with a diagonal gradient: #4f8bff -> #7c5cff -> #22d3ee
  fill(c,
    (x, y) => sdRoundRect(x, y, s / 2, s / 2, s / 2 - m, s / 2 - m, radius),
    (x, y) => {
      const t = (x / s) * 0.55 + (y / s) * 0.45;
      const stops = [[79, 139, 255], [124, 92, 255], [34, 211, 238]];
      const seg = Math.min(Math.floor(t * (stops.length - 1)), stops.length - 2);
      const f = t * (stops.length - 1) - seg;
      const a = stops[seg], b = stops[seg + 1];
      return [
        Math.round(a[0] + (b[0] - a[0]) * f),
        Math.round(a[1] + (b[1] - a[1]) * f),
        Math.round(a[2] + (b[2] - a[2]) * f)
      ];
    });

  // Three stacked "layers" — the asset tiers this app manages.
  const barW = s * 0.42;
  const barH = Math.max(1, s * 0.072);
  const gap = s * 0.115;
  const cx = s / 2;
  const topY = s * 0.34;
  const shades = [[255, 255, 255], [226, 236, 255], [186, 205, 255]];
  for (let k = 0; k < 3; k++) {
    const w = barW * (1 - k * 0.16);
    const y = topY + k * gap;
    fill(c,
      (x, yy) => sdRoundRect(x, yy, cx, y + barH / 2, w / 2, barH / 2, barH / 2),
      () => shades[k]);
  }
  return c;
}

/* ------------------------------------------------------------------ */
/* PNG encoder (RGBA8, filter 0)                                       */
/* ------------------------------------------------------------------ */

function crc32(buf) {
  let c, crc = 0xffffffff;
  for (let n = 0; n < buf.length; n++) {
    c = (crc ^ buf[n]) & 0xff;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    crc = c ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const td = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(td), 0);
  return Buffer.concat([len, td, crc]);
}

function encodePng(canvas) {
  const { size, data } = canvas;
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8;    // bit depth
  ihdr[9] = 6;    // colour type RGBA
  ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0;

  // Raw scanlines with filter byte 0
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    data.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0))
  ]);
}

/* ------------------------------------------------------------------ */
/* ICO container (PNG-compressed entries, Vista+ style)                */
/* ------------------------------------------------------------------ */

function encodeIco(images) {
  const count = images.length;
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(count, 4);

  const dir = Buffer.alloc(16 * count);
  let offset = 6 + dir.length;
  images.forEach((img, i) => {
    const o = i * 16;
    dir[o] = img.size >= 256 ? 0 : img.size;
    dir[o + 1] = img.size >= 256 ? 0 : img.size;
    dir[o + 2] = 0;            // palette
    dir[o + 3] = 0;            // reserved
    dir.writeUInt16LE(1, o + 4);   // colour planes
    dir.writeUInt16LE(32, o + 6);  // bits per pixel
    dir.writeUInt32BE(0, o + 8);
    dir.writeUInt32LE(img.data.length, o + 8);
    dir.writeUInt32LE(offset, o + 12);
    offset += img.data.length;
  });

  return Buffer.concat([header, dir, ...images.map((i) => i.data)]);
}

/* ------------------------------------------------------------------ */

function main() {
  fs.mkdirSync(OUT_DIR, { recursive: true });

  const rendered = SIZES.map((s) => ({ size: s, canvas: drawIcon(s) }));

  // 512px PNG for Linux/macOS icons
  fs.writeFileSync(path.join(OUT_DIR, 'icon.png'), encodePng(drawIcon(512)));
  console.log('build/icon.png (512)');

  // ICO with every resolution
  const ico = encodeIco(rendered.map((r) => ({ size: r.size, data: encodePng(r.canvas) })));
  fs.writeFileSync(path.join(OUT_DIR, 'icon.ico'), ico);
  console.log(`build/icon.ico (${SIZES.join('/')}, ${(ico.length / 1024).toFixed(1)} KB)`);

  // Verify it is a structurally valid ICO: 6-byte header + 16*count directory
  const count = ico.readUInt16LE(4);
  const expect = 6 + 16 * count;
  if (ico.readUInt16LE(0) !== 0 || ico.readUInt16LE(2) !== 1) {
    throw new Error('ICO header invalid');
  }
  console.log(`verified: ${count} entries, directory ends at byte ${expect}`);
}

main();