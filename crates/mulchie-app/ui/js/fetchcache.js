// Data a view asks the backend for itself (the Issues and Performance views),
// kept per key and asked for again when it is older than `ttl` and the view
// is drawn. A view draws on every poll, so what is shown follows the engine
// within a few seconds, and nothing is fetched while the view is closed.
import { invoke } from './ipc.js';
import { hooks } from './store.js';

const cache = new Map();

// `{data, error, at, busy}` for `key`, starting a fetch of `cmd` when it is
// older than `ttl` ms. The view is drawn again when the answer comes.
export function cached(key, cmd, args = {}, ttl = 5000) {
  let e = cache.get(key);
  if (!e) { e = { data: null, error: null, at: 0, busy: false }; cache.set(key, e); }
  if (!e.busy && Date.now() - e.at >= ttl) {
    e.busy = true;
    invoke(cmd, args).then(d => { e.data = d; e.error = null; }, err => { e.error = String(err); })
      .finally(() => { e.busy = false; e.at = Date.now(); hooks.render(); });
  }
  return e;
}

// Ask again at the next draw, for every key that starts with `prefix`.
export function stale(prefix) {
  for (const [k, e] of cache) if (k.startsWith(prefix)) e.at = 0;
}
