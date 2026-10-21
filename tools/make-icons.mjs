// Builds every icon file from the vector design below (no dependencies):
//   assets/icon.svg            the design as SVG, for reference and other uses
//   assets/downloaderx.ico     16-256 px, for the exe, installer and window
//   assets/icon-32.png, assets/icon-256.png   tray and window icon
//   extension/icons/16.png, 48.png, 128.png
// The icon is a toolbar block from the app: burnt-orange block, thick ink outline, hard
// ink drop shadow, darker bottom band and corner rivets, with a cream X whose lower-right
// leg curves down into a download arrow. Sizes up to 32 px use a bolder version.
// Run: node tools/make-icons.mjs
import { writeFileSync } from 'node:fs';
import { deflateSync } from 'node:zlib';

const INK = '#2A1A12', ORANGE = '#C8672E', CREAM = '#FFF4E0';

// The design on a 256 x 256 canvas, as a list of layers painted in order.
function design(small) {
  const bo = small ? 14 : 8;               // block outline
  const o = small ? 10 : 6;                // glyph outline
  const w = small ? 38 : 32;               // glyph stroke
  const bx = 14, by = 12, bw = 228, bh = 216, r = small ? 46 : 40, shadow = 12;
  const s = small ? 1 : 0.84, cx = 124, cy = 128, dy = small ? -4 : -6;  // glyph scale around its centre
  const P = (x, y) => [cx + (x - cx) * s, cy + dy + (y - cy) * s];
  const top = P(0, 50)[1], foot = P(0, 184)[1];
  const legs = [
    [P(186, 24), P(62, 210)],
    [P(60, 24), P(122, 118), ...cubic(P(122, 118), P(144, 148), P(168, 138), P(168, 162)), P(168, 176)],
  ];
  const head = [P(130, 168), P(206, 168), P(168, 208)];
  // Leg ends are cut flat: the straight leg at the top and bottom, the curved one at the top only.
  const cut = pad => [{ x: 0, y: top - pad, w: 256, h: foot - top + 2 * pad }, { x: 130, y: top - pad, w: 126, h: 256 }];
  const block = { x: bx, y: by, w: bw, h: bh, r };
  return [
    { rrect: { ...block, y: by + shadow }, color: INK },
    { rrect: block, color: ORANGE },
    { rect: { x: 0, y: by + bh - (small ? 26 : 30), w: 256, h: 40 }, within: block, color: INK, alpha: 0.22 },
    { ring: block, width: bo, color: INK },
    ...(small ? [] : [[42, 40], [214, 40], [42, 196], [214, 196]].map(([x, y]) => ({ circle: { x, y, r: 6 }, color: INK }))),
    ...legs.map(line => ({ line, width: w + 2 * o, clip: cut(o), color: INK })),
    { poly: head, grow: o, color: INK },
    ...legs.map(line => ({ line, width: w, clip: cut(0), color: CREAM })),
    { poly: head, color: CREAM },
  ];
}
function cubic(a, b, c, d, n = 24) {
  const out = [];
  for (let i = 1; i <= n; i++) {
    const t = i / n, u = 1 - t;
    out.push([0, 1].map(k => u * u * u * a[k] + 3 * u * u * t * b[k] + 3 * u * t * t * c[k] + t * t * t * d[k]));
  }
  return out;
}

// --- geometry tests ---
const inRect = (q, x, y) => x >= q.x && x <= q.x + q.w && y >= q.y && y <= q.y + q.h;
function rrectDist(q, x, y) {   // signed distance to a rounded rectangle, negative inside
  const hx = q.w / 2, hy = q.h / 2, px = Math.abs(x - q.x - hx) - (hx - q.r), py = Math.abs(y - q.y - hy) - (hy - q.r);
  return Math.hypot(Math.max(px, 0), Math.max(py, 0)) + Math.min(Math.max(px, py), 0) - q.r;
}
function segDist(x, y, [ax, ay], [bx, by]) {
  const dx = bx - ax, dy = by - ay, t = Math.max(0, Math.min(1, ((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)));
  return Math.hypot(x - ax - t * dx, y - ay - t * dy);
}
function inPoly(pts, x, y) {
  let inside = false;
  for (let i = 0, j = pts.length - 1; i < pts.length; j = i++) {
    const [xi, yi] = pts[i], [xj, yj] = pts[j];
    if ((yi > y) !== (yj > y) && x < ((xj - xi) * (y - yi)) / (yj - yi) + xi) inside = !inside;
  }
  return inside;
}
function covers(L, x, y) {
  if (L.clip && !L.clip.some(q => inRect(q, x, y))) return false;
  if (L.within && rrectDist(L.within, x, y) > 0) return false;
  if (L.rrect) return rrectDist(L.rrect, x, y) <= 0;
  if (L.rect) return inRect(L.rect, x, y);
  if (L.ring) { const d = rrectDist(L.ring, x, y); return d <= 0 && d >= -L.width; }
  if (L.circle) return Math.hypot(x - L.circle.x, y - L.circle.y) <= L.circle.r;
  if (L.line) { for (let i = 1; i < L.line.length; i++) if (segDist(x, y, L.line[i - 1], L.line[i]) <= L.width / 2) return true; return false; }
  if (L.poly) {
    if (inPoly(L.poly, x, y)) return true;
    if (!L.grow) return false;
    for (let i = 0; i < L.poly.length; i++) if (segDist(x, y, L.poly[i], L.poly[(i + 1) % L.poly.length]) <= L.grow) return true;
    return false;
  }
  return false;
}
const rgb = h => [1, 3, 5].map(i => parseInt(h.slice(i, i + 2), 16));

// Render at a pixel size with 8 x 8 samples per pixel.
function render(size) {
  const layers = design(size <= 32).map(L => ({ ...L, rgb: rgb(L.color), a: L.alpha ?? 1 }));
  const px = new Uint8Array(size * size * 4), S = 8, f = 256 / size;
  for (let y = 0; y < size; y++) for (let x = 0; x < size; x++) {
    let r = 0, g = 0, b = 0, a = 0;
    for (let sy = 0; sy < S; sy++) for (let sx = 0; sx < S; sx++) {
      const X = (x + (sx + 0.5) / S) * f, Y = (y + (sy + 0.5) / S) * f;
      let cr = 0, cg = 0, cb = 0, ca = 0;   // premultiplied, painted bottom to top
      for (const L of layers) if (covers(L, X, Y)) {
        cr = L.rgb[0] * L.a + cr * (1 - L.a); cg = L.rgb[1] * L.a + cg * (1 - L.a); cb = L.rgb[2] * L.a + cb * (1 - L.a); ca = L.a + ca * (1 - L.a);
      }
      r += cr; g += cg; b += cb; a += ca;
    }
    px.set(a ? [r / a, g / a, b / a, (a / (S * S)) * 255].map(Math.round) : [0, 0, 0, 0], (y * size + x) * 4);
  }
  return { w: size, h: size, px };
}

// The same layers as SVG.
function svg() {
  const f = n => +n.toFixed(1), pts = p => p.map(([x, y]) => `${f(x)},${f(y)}`).join(' ');
  const defs = [], body = [];
  design(false).forEach((L, i) => {
    let clip = '';
    if (L.clip || L.within) {
      const shapes = L.clip ? L.clip.map(q => `<rect x="${f(q.x)}" y="${f(q.y)}" width="${f(q.w)}" height="${f(q.h)}"/>`).join('')
        : `<rect x="${L.within.x}" y="${L.within.y}" width="${L.within.w}" height="${L.within.h}" rx="${L.within.r}"/>`;
      defs.push(`<clipPath id="c${i}">${shapes}</clipPath>`); clip = ` clip-path="url(#c${i})"`;
    }
    const paint = `fill="${L.color}"${L.alpha ? ` fill-opacity="${L.alpha}"` : ''}${clip}`;
    if (L.rrect) body.push(`<rect x="${L.rrect.x}" y="${L.rrect.y}" width="${L.rrect.w}" height="${L.rrect.h}" rx="${L.rrect.r}" ${paint}/>`);
    else if (L.rect) body.push(`<rect x="${L.rect.x}" y="${L.rect.y}" width="${L.rect.w}" height="${L.rect.h}" ${paint}/>`);
    else if (L.ring) { const q = L.ring, h = L.width / 2; body.push(`<rect x="${q.x + h}" y="${q.y + h}" width="${q.w - L.width}" height="${q.h - L.width}" rx="${q.r - h}" fill="none" stroke="${L.color}" stroke-width="${L.width}"/>`); }
    else if (L.circle) body.push(`<circle cx="${L.circle.x}" cy="${L.circle.y}" r="${L.circle.r}" ${paint}/>`);
    else if (L.line) body.push(`<polyline points="${pts(L.line)}" fill="none" stroke="${L.color}" stroke-width="${L.width}" stroke-linecap="round" stroke-linejoin="round"${clip}/>`);
    else if (L.poly) body.push(`<polygon points="${pts(L.poly)}" ${paint}${L.grow ? ` stroke="${L.color}" stroke-width="${2 * L.grow}" stroke-linejoin="round"` : ''}/>`);
  });
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256">\n<defs>${defs.join('')}</defs>\n${body.join('\n')}\n</svg>\n`;
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

const root = new URL('..', import.meta.url);
const out = {};
const at = size => (out[size] ??= png(render(size)));
writeFileSync(new URL('assets/icon.svg', root), svg());
writeFileSync(new URL('assets/downloaderx.ico', root), ico([16, 20, 24, 32, 40, 48, 64, 128, 256].map(s => [s, at(s)])));
writeFileSync(new URL('assets/icon-32.png', root), at(32));
writeFileSync(new URL('assets/icon-256.png', root), at(256));
for (const s of [16, 48, 128]) writeFileSync(new URL(`extension/icons/${s}.png`, root), at(s));
console.log('wrote assets/icon.svg, assets/downloaderx.ico, assets/icon-32.png, assets/icon-256.png, extension/icons/{16,48,128}.png');
