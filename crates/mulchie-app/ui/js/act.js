// Run an action, toast its outcome, refresh.
import { hooks } from './store.js';
import { toast } from './util.js';

export async function act(fn, ok) {
  try {
    const r = await fn();
    if (ok) toast(typeof ok === 'function' ? ok(r) : ok);
    await hooks.refresh(true);
    return r;
  } catch (e) {
    toast(String(e), true);
  }
}
