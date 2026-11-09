// The Activity view: the engine's journal, newest first. The list is fetched
// only while this view is open and only when it changed.
import { st } from '../store.js';
import { $, $$, esc, B, tilde, th } from '../util.js';
import { reasonText } from '../dialogs.js';

const COLOR = { reclaim: 'ok', worktree: 'ok', throttle: 'warn', pause: 'warn', stop: 'bad', alert: 'bad', error: 'bad' };

export function renderActs() {
  const ui = st.ui;
  const rows = st.actions.filter(a => !ui.aSrc || a.source === ui.aSrc);
  $$('#aSeg button').forEach(b => b.classList.toggle('on', b.dataset.s === ui.aSrc));
  $('#acts').innerHTML = th([{ l: 'Time' }, { l: 'Source' }, { l: 'Kind' }, { l: 'Workspace' }, { l: 'Detail' }, { l: 'Freed', n: 1 }]) +
    (rows.map(a => {
      const skipped = a.status === 'skipped';
      const extra = skipped && a.reason ? ` <span class="warn">(${esc(reasonText(a.reason))})</span>` : '';
      const bk = (a.backups || []).filter(b => b.kind !== 'branch').map(b => ` <button class="btn sm" data-reveal="${esc(b.location)}" title="${esc(tilde(b.location))}">backup</button>`).join('');
      return `<tr><td class="mono dim">${new Date(a.at_unix * 1000).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', second: '2-digit' })}</td>
      <td><span class="chip">${esc(a.source)}</span></td><td class="${skipped ? 'dim' : COLOR[a.kind] || ''}">${esc(a.kind)}${skipped ? ' · skipped' : ''}</td><td class="trunc" style="max-width:260px">${esc(a.workspace || '—')}</td>
      <td class="trunc" style="max-width:520px" title="${esc(a.detail)}">${esc(a.detail)}${extra}${bk}</td><td class="n">${a.bytes && a.kind !== 'alert' ? B(a.bytes) : ''}</td></tr>`;
    }).join('') || '<tr><td colspan="6" class="empty">Nothing yet.</td></tr>');
}
