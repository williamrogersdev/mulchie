// mulchie.com: static assets, plus /download/, which redirects to the newest
// release's macOS DMG (pre-releases included). If GitHub cannot be reached or
// no release has a DMG, the static /download/ page is served instead; it links
// to the releases page.
const REPO = 'williamrogersdev/mulchie';
const CACHE_SECS = 600;

async function latestDmg(ctx) {
  const cache = caches.default;
  const key = new Request('https://mulchie.com/__cache/latest-dmg');
  const hit = await cache.match(key);
  if (hit) return hit.text();
  const res = await fetch(`https://api.github.com/repos/${REPO}/releases?per_page=10`, {
    headers: { 'User-Agent': 'mulchie-site', Accept: 'application/vnd.github+json' },
  });
  if (!res.ok) return null;
  const releases = await res.json();
  for (const r of releases) {
    if (r.draft) continue;
    const dmg = (r.assets || []).find((a) => a.name.endsWith('_universal.dmg'));
    if (dmg) {
      ctx.waitUntil(cache.put(key, new Response(dmg.browser_download_url, { headers: { 'Cache-Control': `max-age=${CACHE_SECS}` } })));
      return dmg.browser_download_url;
    }
  }
  return null;
}

export default {
  async fetch(request, env, ctx) {
    const { pathname } = new URL(request.url);
    if (pathname === '/download' || pathname === '/download/') {
      try {
        const url = await latestDmg(ctx);
        if (url) return Response.redirect(url, 302);
      } catch {
        // Fall through to the static page.
      }
    }
    return env.ASSETS.fetch(request);
  },
};
