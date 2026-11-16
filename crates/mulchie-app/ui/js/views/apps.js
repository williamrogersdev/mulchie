// The Apps view: every app (a program bundle with all its helper processes)
// with its CPU, memory and swap. A row opens the app in the inspector, where
// every action is planned first.
import { st } from '../store.js';
import { $, into, esc, B, sortRows, th, meter, rowBtn, kindGlyph } from '../util.js';
import { cores } from '../consumers.js';

export function renderApps() {
  const S = st.S, ui = st.ui;
  const f = $('#aFilter').value.trim().toLowerCase();
  let rows = (S.snapshot.consumers?.apps || []).filter(a => !f || a.name.toLowerCase().includes(f) || (a.bundle_id || '').toLowerCase().includes(f));
  rows = sortRows(rows, ui.apSort);
  const cm = Math.max(1, ...rows.map(a => a.cpu_pct)), mm = Math.max(1, ...rows.map(a => a.mem_bytes)), sm = Math.max(1, ...rows.map(a => a.swap_bytes || 0));
  const sel = st.ui.detail?.type === 'group' ? st.ui.detail.key : null;
  into($('#apps')).html = th([{ l: 'App', k: 'name' }, { l: 'Bundle id', k: 'bundle_id', c: 'c-lg' }, { l: 'Procs', k: 'procs', n: 1, w: '56px' },
    { l: 'CPU', k: 'cpu_pct', n: 1, w: '128px' }, { l: 'Mem', k: 'mem_bytes', n: 1, w: '124px' }, { l: 'Swap', k: 'swap_bytes', n: 1, w: '112px', c: 'c-md' }, { l: '', w: '64px' }], ui.apSort, 'ap') +
    (rows.map(a => `<tr class="click k-app ${sel === a.key ? 'sel' : ''}" role="button" tabindex="0" data-d="group" data-dk="${esc(a.key)}">
      <td title="${esc(a.bundle_path || a.name)}"><span class="nm">${kindGlyph('app')} ${esc(a.name)}</span>${a.protected ? ' <span class="chip">protected</span>' : ''}</td>
      <td class="dim mono c-lg" title="${esc(a.bundle_id || '')}">${esc(a.bundle_id || '—')}</td><td class="n">${a.procs}</td>
      <td class="n">${meter(a.cpu_pct, cm)}${esc(cores(a.cpu_pct))}</td>
      <td class="n">${meter(a.mem_bytes, mm, 'var(--c2)')}${B(a.mem_bytes)}</td>
      <td class="n c-md">${a.swap_bytes ? meter(a.swap_bytes, sm, 'var(--warn)') + B(a.swap_bytes) : '<span class="dim">—</span>'}</td>
      <td class="n">${a.protected || !a.actions?.includes('quit') ? '' : rowBtn('Quit…', 'g-quit', encodeURIComponent(JSON.stringify({ key: a.key, name: a.name })), { danger: true, title: 'Review, then ask this app and its helpers to quit' })}</td></tr>`).join('')
      || `<tr><td colspan="7" class="empty">${f ? 'No app matches.' : 'No apps are running, or this system does not group processes into apps.'}</td></tr>`);
}
