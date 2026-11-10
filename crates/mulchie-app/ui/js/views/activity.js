// The Activity view: the engine's journal, newest first. The list is fetched
// only while this view is open and only when it changed.
import { st } from '../store.js';
import { $, $$, esc, B, th } from '../util.js';
import { actKey } from '../detail.js';
import { reasonText } from '../dialogs.js';

const COLOR = { reclaim: 'ok', worktree: 'ok', throttle: 'warn', pause: 'warn', stop: 'bad', alert: 'bad', error: 'bad' };

export function renderActs() {
  const ui = st.ui, d = st.ui.detail;
  const rows = st.actions.filter(a => !ui.aSrc || a.source === ui.aSrc);
  $$('#aSeg button').forEach(b => b.classList.toggle('on', b.dataset.s === ui.aSrc));
  $('#acts').innerHTML = th([{ l: 'Time', w: '136px' }, { l: 'Source', w: '86px', c: 'c-md' }, { l: 'Kind', w: '120px' }, { l: 'Workspace', c: 'c-lg', w: '22%' }, { l: 'Detail' }, { l: 'Freed', n: 1, w: '72px' }]) +
    (rows.map(a => {
      const skipped = a.status === 'skipped', k = actKey(a);
      const extra = skipped && a.reason ? ` <span class="warn">(${esc(reasonText(a.reason))})</span>` : '';
      const bk = (a.backups || []).length ? '<span class="chip">backup</span> ' : '';
      return `<tr class="click ${d?.type === 'act' && d.key === k ? 'sel' : ''}" role="button" tabindex="0" data-d="act" data-dk="${esc(k)}"><td class="mono dim">${new Date(a.at_unix * 1000).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', second: '2-digit' })}</td>
      <td class="c-md"><span class="chip">${esc(a.source)}</span></td><td class="${skipped ? 'dim' : COLOR[a.kind] || ''}">${esc(a.kind)}${skipped ? ' · skipped' : ''}</td><td class="c-lg" title="${esc(a.workspace || '')}">${esc(a.workspace || '—')}</td>
      <td title="${esc(a.detail)}">${bk}${esc(a.detail)}${extra}</td><td class="n">${a.bytes && a.kind !== 'alert' ? B(a.bytes) : ''}</td></tr>`;
    }).join('') || '<tr><td colspan="6" class="empty">Nothing yet.</td></tr>');
}
