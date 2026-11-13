// The Workspaces view. A row opens the workspace detail.
import { st } from '../store.js';
import { $, $$, esc, B, BS, sortRows, th, meter, rowBtn } from '../util.js';
import { sparkline } from '../chart.js';
import { cores } from '../consumers.js';

export function renderWss() {
  const S = st.S, ui = st.ui;
  const f = $('#wFilter').value.trim().toLowerCase();
  let rows = S.snapshot.workspaces.filter(w => !f || w.label.toLowerCase().includes(f) || (w.branch || '').toLowerCase().includes(f));
  const k = ui.wSort[0] === 'io' ? (w => w.read_bps + w.write_bps) : ui.wSort[0] === 'procs' ? (w => w.procs.length) : ui.wSort[0];
  rows = sortRows(rows, [k, ui.wSort[1]]);
  const cm = Math.max(1, ...rows.map(w => w.cpu_pct)), mm = Math.max(1, ...rows.map(w => w.mem_bytes));
  const sel = st.ui.detail?.type === 'ws' ? st.ui.detail.key : null;
  $('#wss').innerHTML = th([{ l: 'Workspace', k: 'label' }, { l: 'Branch', k: 'branch', c: 'c-lg' }, { l: 'Agents', k: 'agents', n: 1, w: '62px' }, { l: 'Builds', k: 'builds', n: 1, w: '58px' }, { l: 'Procs', k: 'procs', n: 1, w: '56px', c: 'c-md' },
    { l: 'CPU', k: 'cpu_pct', n: 1, w: '128px' }, { l: 'Last 5 min', w: '96px', c: 'c-xl' }, { l: 'Mem', k: 'mem_bytes', n: 1, w: '124px' }, { l: 'Disk I/O', k: 'io', n: 1, w: '90px', c: 'c-xl' }, { l: 'State', w: '120px', c: 'c-md' }, { l: '', w: '64px' }], ui.wSort, 'w') +
    (rows.map(w => `<tr class="click ${sel === w.path ? 'sel' : ''}" role="button" tabindex="0" data-d="ws" data-dk="${esc(w.path)}">
      <td title="${esc(w.path)}">${esc(w.label)}</td><td class="dim mono c-lg" title="${esc(w.branch || '')}">${esc(w.branch || '—')}</td>
      <td class="n">${w.agents || '<span class="dim">0</span>'}</td><td class="n">${w.builds || '<span class="dim">0</span>'}</td><td class="n c-md">${w.procs.length}</td>
      <td class="n">${meter(w.cpu_pct, cm)}${esc(cores(w.cpu_pct))}</td><td class="c-xl"><canvas class="spark" data-spark="${esc(w.label)}"></canvas></td>
      <td class="n">${meter(w.mem_bytes, mm, 'var(--c2)')}${B(w.mem_bytes)}</td><td class="n c-xl">${BS(w.read_bps + w.write_bps)}</td>
      <td class="c-md">${w.orphan ? '<span class="chip warn">orphan</span> ' : ''}${w.paused ? '<span class="chip warn">paused</span> ' : ''}${w.throttled ? `<span class="chip">${w.throttled} reniced</span> ` : ''}${!w.exists ? '<span class="chip bad">deleted</span>' : ''}</td>
      <td class="n">${rowBtn('Stop…', 'stop-here', w.path, { danger: true, title: 'Review, then ask everything running here to quit' })}</td></tr>`).join('') || '<tr><td colspan="11" class="empty">No workspaces with running processes.</td></tr>');
  const tail = st.H.slice(-100);
  $$('#wss canvas[data-spark]').forEach(c => sparkline(c, tail.map(x => (x.ws.find(w => w[0] === c.dataset.spark) || [0, 0])[1])));
}
