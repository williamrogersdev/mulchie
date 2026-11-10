import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

export const SITE = 'https://mulchie.com';
export const REPO = 'https://github.com/williamrogersdev/mulchie';
export const RELEASES = `${REPO}/releases/latest`;

/** Reads `[workspace.package] version` from the workspace Cargo.toml. The build runs from `site/`. */
function readVersion(): string {
  const file = resolve(process.cwd(), '../Cargo.toml');
  const toml = readFileSync(file, 'utf8');
  const section = toml.split(/^\[workspace\.package\]\s*$/m)[1]?.split(/^\[/m)[0] ?? '';
  const m = section.match(/^version\s*=\s*"([^"]+)"/m);
  if (!m) throw new Error(`Could not read workspace version from ${file}`);
  return m[1];
}

export const VERSION = readVersion();
