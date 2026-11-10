// Walks dist/**/*.html and fails the build on SEO and link problems.
import { readdirSync, readFileSync, existsSync, statSync } from 'node:fs';
import { join, relative, resolve, extname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ORIGIN = 'https://mulchie.com';
const dist = resolve(fileURLToPath(new URL('../dist', import.meta.url)));
if (!existsSync(dist)) {
  console.error('check-seo: dist/ not found. Run the build first.');
  process.exit(1);
}

const walk = (dir) =>
  readdirSync(dir).flatMap((f) => {
    const p = join(dir, f);
    return statSync(p).isDirectory() ? (f === 'pagefind' ? [] : walk(p)) : [p];
  });
const files = walk(dist);
const pages = files.filter((f) => f.endsWith('.html'));
const errors = [];
const err = (page, msg) => errors.push(`${relative(dist, page)}: ${msg}`);

const decode = (s) =>
  s.replace(/&amp;/g, '&').replace(/&quot;/g, '"').replace(/&#39;|&#x27;/g, "'").replace(/&lt;/g, '<').replace(/&gt;/g, '>');
const tags = (html, name) => [...html.matchAll(new RegExp(`<${name}\\b([^>]*)>`, 'gi'))].map((m) => m[1]);
const attr = (s, a) => {
  const m = s.match(new RegExp(`(?:^|\\s)${a}=(?:"([^"]*)"|'([^']*)'|([^\\s>]+))`, 'i'));
  return m ? decode(m[1] ?? m[2] ?? m[3]) : undefined;
};
const meta = (html, key, val) => tags(html, 'meta').map((t) => ({ k: attr(t, key), c: attr(t, 'content') })).filter((m) => m.k === val);

const titles = new Map();
const descs = new Map();
const ids = new Map();

// Resolves a site-relative path to a file in dist, or undefined.
const resolveLocal = (pathname) => {
  const p = decodeURIComponent(pathname);
  const base = join(dist, p);
  if (!base.startsWith(dist)) return undefined;
  const candidates = p.endsWith('/') ? [join(base, 'index.html')] : [base, join(base, 'index.html'), `${base}.html`];
  return candidates.find((c) => existsSync(c) && statSync(c).isFile());
};

const pageUrl = (page) => {
  const rel = relative(dist, page).replace(/\\/g, '/');
  if (rel === '404.html') return undefined;
  return '/' + rel.replace(/index\.html$/, '');
};

for (const page of pages) {
  const html = readFileSync(page, 'utf8');
  const noindex = meta(html, 'name', 'robots').some((m) => /noindex/.test(m.c ?? ''));

  // title
  const t = [...html.matchAll(/<title>([\s\S]*?)<\/title>/gi)];
  if (t.length !== 1) err(page, `expected 1 <title>, found ${t.length}`);
  else {
    const title = decode(t[0][1]).trim();
    if (!title) err(page, 'empty <title>');
    if (title.length > 60) err(page, `title is ${title.length} chars (max 60): "${title}"`);
    if (titles.has(title)) err(page, `duplicate title also on ${titles.get(title)}: "${title}"`);
    titles.set(title, relative(dist, page));
  }

  // description
  const d = meta(html, 'name', 'description');
  if (d.length !== 1) err(page, `expected 1 meta description, found ${d.length}`);
  else {
    const desc = (d[0].c ?? '').trim();
    if (!desc) err(page, 'empty meta description');
    if (desc.length > 160) err(page, `description is ${desc.length} chars (max 160)`);
    if (descs.has(desc)) err(page, `duplicate description also on ${descs.get(desc)}`);
    descs.set(desc, relative(dist, page));
  }

  // canonical
  const canon = tags(html, 'link').filter((l) => attr(l, 'rel') === 'canonical');
  if (canon.length !== 1) err(page, `expected 1 canonical, found ${canon.length}`);
  else {
    const href = attr(canon[0], 'href') ?? '';
    if (!href.startsWith(`${ORIGIN}/`)) err(page, `canonical not on ${ORIGIN}: ${href}`);
    const expected = pageUrl(page);
    if (expected && !noindex && href !== `${ORIGIN}${expected}`) err(page, `canonical ${href} does not match page URL ${ORIGIN}${expected}`);
  }

  // Open Graph and Twitter
  for (const [k, v] of [['property', 'og:title'], ['property', 'og:description'], ['property', 'og:url'], ['name', 'twitter:card']]) {
    if (!meta(html, k, v).some((m) => m.c)) err(page, `missing ${v}`);
  }
  const og = meta(html, 'property', 'og:image');
  if (og.length < 1 || !og[0].c) err(page, 'missing og:image');
  else {
    if (og.length > 1) err(page, `multiple og:image tags (${og.length})`);
    if (!og[0].c.startsWith(`${ORIGIN}/`)) err(page, `og:image not absolute on ${ORIGIN}: ${og[0].c}`);
    else if (!resolveLocal(new URL(og[0].c).pathname)) err(page, `og:image file missing in dist: ${og[0].c}`);
  }

  // headings and landmarks
  const h1 = (html.match(/<h1[\s>]/gi) ?? []).length;
  if (h1 !== 1) err(page, `expected 1 <h1>, found ${h1}`);
  if (!/<html[^>]*\slang=/i.test(html)) err(page, 'missing <html lang>');
  if (!/<main[\s>]/i.test(html)) err(page, 'missing <main>');

  // duplicate ids (breaks anchors and a11y)
  const seen = new Set();
  for (const m of html.matchAll(/\sid="([^"]+)"/g)) {
    if (seen.has(m[1])) err(page, `duplicate id "${m[1]}"`);
    seen.add(m[1]);
  }
  ids.set(page, seen);

  // images need dimensions
  for (const img of tags(html, 'img')) {
    if (!attr(img, 'width') || !attr(img, 'height')) err(page, `<img> without width and height: ${img.slice(0, 80)}`);
    if (!/(^|\s)alt(=|\s|$)/.test(img)) err(page, `<img> without alt: ${img.slice(0, 80)}`);
  }
}

// internal links and anchors
for (const page of pages) {
  const html = readFileSync(page, 'utf8');
  const here = pageUrl(page) ?? '/404/';
  for (const a of tags(html, 'a')) {
    const href = attr(a, 'href');
    if (!href || /^(mailto:|tel:|javascript:)/i.test(href)) continue;
    if (href.startsWith('#')) {
      if (href.length > 1 && !ids.get(page)?.has(decodeURIComponent(href.slice(1)))) err(page, `broken anchor ${href}`);
      continue;
    }
    let url;
    try {
      url = new URL(href, `${ORIGIN}${here}`);
    } catch {
      err(page, `invalid href "${href}"`);
      continue;
    }
    if (url.origin !== ORIGIN) continue;
    const target = resolveLocal(url.pathname);
    if (!target) {
      err(page, `broken internal link ${href}`);
      continue;
    }
    if (url.hash && extname(target) === '.html') {
      const set = ids.get(target) ?? new Set([...readFileSync(target, 'utf8').matchAll(/\sid="([^"]+)"/g)].map((m) => m[1]));
      if (!set.has(decodeURIComponent(url.hash.slice(1)))) err(page, `broken anchor ${href}`);
    }
  }
}

// sitemap and robots
const sm = join(dist, 'sitemap-0.xml');
if (!existsSync(sm)) errors.push('sitemap-0.xml missing');
else {
  const xml = readFileSync(sm, 'utf8');
  for (const p of pages) {
    const u = pageUrl(p);
    if (u && !xml.includes(`<loc>${ORIGIN}${u}</loc>`)) err(p, 'not in sitemap');
  }
}
const robots = join(dist, 'robots.txt');
if (!existsSync(robots) || !/^Sitemap:\s*https:\/\/mulchie\.com\/sitemap-index\.xml/m.test(readFileSync(robots, 'utf8'))) {
  errors.push('robots.txt missing or does not point at the sitemap');
}
for (const f of ['llms.txt', 'favicon.ico', 'site.webmanifest', '_headers']) {
  if (!existsSync(join(dist, f))) errors.push(`${f} missing from dist`);
}

// landing JSON-LD
const home = readFileSync(join(dist, 'index.html'), 'utf8');
const ld = home.match(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/);
if (!ld) errors.push('index.html: missing JSON-LD');
else {
  try {
    const j = JSON.parse(ld[1]);
    if (j['@type'] !== 'SoftwareApplication') errors.push('index.html: JSON-LD is not SoftwareApplication');
    for (const k of ['name', 'description', 'applicationCategory', 'operatingSystem', 'offers', 'license', 'downloadUrl', 'softwareVersion']) {
      if (!j[k]) errors.push(`index.html: JSON-LD missing ${k}`);
    }
  } catch (e) {
    errors.push(`index.html: JSON-LD does not parse: ${e.message}`);
  }
}

// no client JS on the landing and install pages
for (const f of ['index.html', 'install/index.html']) {
  const html = readFileSync(join(dist, f), 'utf8');
  const scripts = [...html.matchAll(/<script\b([^>]*)>/gi)].filter((m) => !/application\/ld\+json/.test(m[1]));
  if (scripts.length) errors.push(`${f}: ships ${scripts.length} script tag(s); landing and install must have none`);
}

if (errors.length) {
  console.error(`check-seo: ${errors.length} problem(s)\n` + errors.map((e) => `  - ${e}`).join('\n'));
  process.exit(1);
}
console.log(`check-seo: ${pages.length} pages OK`);
