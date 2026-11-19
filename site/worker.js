// mulchie.com: static assets, plus /download/, which redirects to the newest
// release's macOS DMG (pre-releases included). If GitHub cannot be reached or
// no release has a DMG, the static /download/ page is served instead; it links
// to the releases page.
const REPO = 'williamrogersdev/mulchie';
const CACHE_SECS = 600;

// GitHub's releases feed lists the newest releases first, pre-releases
// included. It is used instead of the REST API, which refuses unauthenticated
// calls from Cloudflare's shared addresses. A release without a DMG (a
// CLI-only release) is skipped by checking the asset before redirecting to it.
async function latestDmg(ctx) {
  const cache = caches.default;
  const key = new Request('https://mulchie.com/__cache/latest-dmg');
  const hit = await cache.match(key);
  if (hit) return hit.text();
  const res = await fetch(`https://github.com/${REPO}/releases.atom`, { headers: { 'User-Agent': 'mulchie-site' } });
  if (!res.ok) return null;
  const feed = await res.text();
  const tags = [...feed.matchAll(/\/releases\/tag\/(v[0-9A-Za-z.+-]+)/g)].map((m) => m[1]);
  for (const tag of [...new Set(tags)].slice(0, 5)) {
    const url = `https://github.com/${REPO}/releases/download/${tag}/Mulchie_${tag.slice(1)}_universal.dmg`;
    const head = await fetch(url, { method: 'HEAD', redirect: 'manual', headers: { 'User-Agent': 'mulchie-site' } });
    if (head.status === 302 || head.status === 200) {
      ctx.waitUntil(cache.put(key, new Response(url, { headers: { 'Cache-Control': `max-age=${CACHE_SECS}` } })));
      return url;
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
