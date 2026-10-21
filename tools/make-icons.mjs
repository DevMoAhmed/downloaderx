// Builds every icon file from the pixel source in assets/icon.txt (no dependencies):
//   assets/downloaderx.ico   16-256 px, for the exe, installer and window
//   assets/icon-32.png, assets/icon-256.png   tray and window icon
//   extension/icons/16.png, 48.png, 128.png
// Run: node tools/make-icons.mjs
import { readFileSync, writeFileSync } from 'node:fs';
import { deflateSync } from 'node:zlib';

const PAL = {
  k: '#F2D3A6', c: '#FFF4E0', i: '#2A1A12', m: '#8E2E2A', o: '#C8672E', g: '#F0B23C',
  s: '#7FB7B5', t: '#5C6B73', l: '#24160F', r: '#C9402F', e: '#4F8A4B', y: '#FFC83D',
};
const root = new URL('..', import.meta.url);
const rows = readFileSync(new URL('assets/icon.txt', root), 'utf8').split(/\r?\n/).filter(l => l && !l.startsWith('#'));
const N = rows.length;
if (rows.some(r => r.length !== N)) throw new Error('assets/icon.txt must be square');

// RGBA image from a pixel grid.
function toImage(grid) {
  const n = grid.length, px = new Uint8Array(n * n * 4);
  grid.forEach((row, y) => [...row].forEach((ch, x) => {
    if (ch === '.') return;
    const h = PAL[ch]; if (!h) throw new Error(`unknown color "${ch}"`);
    px.set([parseInt(h.slice(1, 3), 16), parseInt(h.slice(3, 5), 16), parseInt(h.slice(5, 7), 16), 255], (y * n + x) * 4);
  }));
  return { w: n, h: n, px };
}
// Whole-number enlargement keeps the pixels crisp.
function nearest(img, size) {
  const out = new Uint8Array(size * size * 4), f = size / img.w;
  for (let y = 0; y < size; y++) for (let x = 0; x < size; x++) {
    const sx = Math.floor(x / f), sy = Math.floor(y / f);
    out.set(img.px.subarray((sy * img.w + sx) * 4, (sy * img.w + sx) * 4 + 4), (y * size + x) * 4);
  }
  return { w: size, h: size, px: out };
}
// Area-weighted shrink with premultiplied alpha, for sizes between the crisp ones.
function shrink(img, size) {
  const out = new Uint8Array(size * size * 4), f = img.w / size;
  for (let y = 0; y < size; y++) for (let x = 0; x < size; x++) {
    let r = 0, g = 0, b = 0, a = 0, area = 0;
    const x0 = x * f, x1 = x0 + f, y0 = y * f, y1 = y0 + f;
    for (let sy = Math.floor(y0); sy < Math.ceil(y1); sy++) for (let sx = Math.floor(x0); sx < Math.ceil(x1); sx++) {
      const wgt = (Math.min(x1, sx + 1) - Math.max(x0, sx)) * (Math.min(y1, sy + 1) - Math.max(y0, sy));
      const p = (sy * img.w + sx) * 4, al = img.px[p + 3] / 255;
      r += img.px[p] * al * wgt; g += img.px[p + 1] * al * wgt; b += img.px[p + 2] * al * wgt; a += al * wgt; area += wgt;
    }
    const A = a / area;
    out.set(A ? [r / a, g / a, b / a, A * 255].map(Math.round) : [0, 0, 0, 0], (y * size + x) * 4);
  }
  return { w: size, h: size, px: out };
}
// 16 px by hand rules: each 2 x 2 block keeps ink when it has two ink pixels (so the
// outline survives), else its most common color.
function half(grid) {
  const n = grid.length / 2, out = [];
  for (let y = 0; y < n; y++) {
    let row = '';
    for (let x = 0; x < n; x++) {
      const cell = [grid[2 * y][2 * x], grid[2 * y][2 * x + 1], grid[2 * y + 1][2 * x], grid[2 * y + 1][2 * x + 1]];
      const filled = cell.filter(v => v !== '.');
      if (cell.filter(v => v === 'i').length >= 2) { row += 'i'; continue; }
      if (filled.length < 2) { row += '.'; continue; }
      const count = {}; filled.forEach(v => (count[v] = (count[v] || 0) + 1));
      row += Object.keys(count).sort((a, b) => count[b] - count[a])[0];
    }
    out.push(row);
  }
  return out;
}

// --- PNG and ICO writers ---
const CRC = new Int32Array(256).map((_, n) => { let c = n; for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c; });
const crc32 = buf => { let c = -1; for (const b of buf) c = CRC[(c ^ b) & 255] ^ (c >>> 8); return (c ^ -1) >>> 0; };
function chunk(type, data) {
  const len = Buffer.alloc(4); len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type), data]), crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
}
function png(img) {
  const ihdr = Buffer.alloc(13); ihdr.writeUInt32BE(img.w, 0); ihdr.writeUInt32BE(img.h, 4); ihdr.set([8, 6, 0, 0, 0], 8);
  const raw = Buffer.alloc(img.h * (img.w * 4 + 1));
  for (let y = 0; y < img.h; y++) raw.set(img.px.subarray(y * img.w * 4, (y + 1) * img.w * 4), y * (img.w * 4 + 1) + 1);
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ihdr), chunk('IDAT', deflateSync(raw, { level: 9 })), chunk('IEND', Buffer.alloc(0))]);
}
function ico(pngs) {
  const head = Buffer.alloc(6); head.writeUInt16LE(1, 2); head.writeUInt16LE(pngs.length, 4);
  let offset = 6 + 16 * pngs.length;
  const dir = pngs.map(([size, data]) => {
    const e = Buffer.alloc(16); e[0] = size % 256; e[1] = size % 256; e.writeUInt16LE(1, 4); e.writeUInt16LE(32, 6);
    e.writeUInt32LE(data.length, 8); e.writeUInt32LE(offset, 12); offset += data.length; return e;
  });
  return Buffer.concat([head, ...dir, ...pngs.map(p => p[1])]);
}

// A hand-drawn 16 px version wins over the automatic one when present.
let rows16 = null;
try { rows16 = readFileSync(new URL('assets/icon16.txt', root), 'utf8').split(/\r?\n/).filter(l => l && !l.startsWith('#')); } catch {}
if (rows16 && (rows16.length !== 16 || rows16.some(r => r.length !== 16))) throw new Error('assets/icon16.txt must be 16 x 16');
const base = toImage(rows), big = nearest(base, 256), small = toImage(rows16 || half(rows));
const at = size => size === 16 ? small : size % N === 0 ? nearest(base, size) : shrink(big, size);
const sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256];
writeFileSync(new URL('assets/downloaderx.ico', root), ico(sizes.map(s => [s, png(at(s))])));
writeFileSync(new URL('assets/icon-32.png', root), png(at(32)));
writeFileSync(new URL('assets/icon-256.png', root), png(at(256)));
for (const s of [16, 48, 128]) writeFileSync(new URL(`extension/icons/${s}.png`, root), png(at(s)));
console.log('16 px version' + (rows16 ? ' (hand-drawn)' : ' (automatic)') + ':\n' + (rows16 || half(rows)).join('\n'));
console.log('wrote assets/downloaderx.ico, assets/icon-32.png, assets/icon-256.png, extension/icons/{16,48,128}.png');
