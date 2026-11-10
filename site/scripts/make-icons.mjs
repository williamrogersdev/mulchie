// Derives the favicon set from the app icon. Read-only use of the source file.
// Run with `bun run icons`; the outputs are committed under public/.
import sharp from 'sharp';
import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const src = fileURLToPath(new URL('../../crates/mulchie-app/icons/icon.png', import.meta.url));
const out = (n) => fileURLToPath(new URL(`../public/${n}`, import.meta.url));
const png = (size, flatten) => {
  let img = sharp(src).resize(size, size);
  if (flatten) img = img.flatten({ background: flatten });
  return img.png({ compressionLevel: 9 }).toBuffer();
};

// Apple touch icons are opaque; iOS applies its own corner mask.
const green = { r: 0x4a, g: 0x7c, b: 0x3f };
writeFileSync(out('apple-touch-icon.png'), await png(180, green));
writeFileSync(out('icon-192.png'), await png(192));
writeFileSync(out('icon-512.png'), await png(512));
writeFileSync(out('favicon-32.png'), await png(32));

// favicon.ico with embedded PNGs (16, 32, 48).
const sizes = [16, 32, 48];
const imgs = await Promise.all(sizes.map((s) => png(s)));
const header = Buffer.alloc(6);
header.writeUInt16LE(1, 2);
header.writeUInt16LE(sizes.length, 4);
let offset = 6 + 16 * sizes.length;
const dir = imgs.map((buf, i) => {
  const e = Buffer.alloc(16);
  e.writeUInt8(sizes[i], 0);
  e.writeUInt8(sizes[i], 1);
  e.writeUInt16LE(1, 4);
  e.writeUInt16LE(32, 6);
  e.writeUInt32LE(buf.length, 8);
  e.writeUInt32LE(offset, 12);
  offset += buf.length;
  return e;
});
writeFileSync(out('favicon.ico'), Buffer.concat([header, ...dir, ...imgs]));
console.log('icons written');
