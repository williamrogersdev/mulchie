// Copies the installer into the output so https://mulchie.com/install.sh works.
// The script is owned by another package; the build must not fail without it.
import { copyFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const src = fileURLToPath(new URL('../../scripts/install.sh', import.meta.url));
const dest = fileURLToPath(new URL('../dist/install.sh', import.meta.url));
if (existsSync(src)) {
  copyFileSync(src, dest);
  console.log('postbuild: copied scripts/install.sh to dist/install.sh');
} else {
  console.warn('postbuild: scripts/install.sh not found, /install.sh will 404');
}

// Maintainer notes (`<!-- VERIFY: ... -->`) live in the Markdown sources only; do not ship them.
import { readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const dist = fileURLToPath(new URL('../dist', import.meta.url));
let stripped = 0;
const walk = (dir) => {
  for (const f of readdirSync(dir)) {
    const p = join(dir, f);
    if (statSync(p).isDirectory()) walk(p);
    else if (f.endsWith('.html')) {
      const html = readFileSync(p, 'utf8');
      const out = html.replace(/<!--\s*VERIFY:[\s\S]*?-->\s*/g, '');
      if (out !== html) {
        writeFileSync(p, out);
        stripped++;
      }
    }
  }
};
walk(dist);
if (stripped) console.log(`postbuild: stripped VERIFY notes from ${stripped} page(s)`);
