// The Processes view. A row opens the process detail; bulk actions go
// through the same confirm-and-start-time path as everything else.
import { st } from '../store.js';
import { $, esc, B, BS, dur, sortRows, th, meter, kindChip } from '../util.js';
import { signalProcs } from '../signals.js';
import { cores } from '../consumers.js';

export function procRows() {
  const S = st.S, ui = st.ui;
  const f = $('#pFilter').value.trim().toLowerCase(), mine = $('#pMine').checked;
  let rows = S.snapshot.top.filter(p => (!ui.pKind || p.kind === ui.pKind) && (!mine || p.own));
  if (f) rows = rows.filter(p => (p.name + ' ' + p.pid + ' ' + p.group + ' ' + p.cmd).toLowerCase().includes(f));
  const k = ui.pSort[0] === 'io' ? (p => p.read_bps + p.write_bps) : ui.pSort[0];
  return sortRows(rows, [k, ui.pSort[1]]);
}

export function renderProcs() {
  const S = st.S, ui = st.ui;
  const rows = procRows(), cm = Math.max(1, ...rows.map(p => p.cpu_pct)), mm = Math.max(1, ...rows.map(p => p.mem_bytes));
  for (const pid of ui.pSel) if (!S.snapshot.top.some(p => p.pid === pid)) ui.pSel.delete(pid);
  const sel = st.ui.detail?.type === 'proc' ? +st.ui.detail.key : null;
  $('#pCount').textContent = `${rows.length} shown · ${S.snapshot.proc_count} total · the top by CPU, memory and I/O are tracked`;
  $('#procs').innerHTML = th([{ check: 1 }, { l: 'PID', k: 'pid', n: 1, w: '64px', c: 'c-md' }, { l: 'Name', k: 'name' }, { l: 'Kind', k: 'kind', w: '78px', c: 'c-lg' }, { l: 'Workspace / app', k: 'group' },
    { l: 'CPU', k: 'cpu_pct', n: 1, w: '128px' }, { l: 'Mem', k: 'mem_bytes', n: 1, w: '124px' }, { l: 'Disk I/O', k: 'io', n: 1, w: '90px', c: 'c-xl' }, { l: 'NI', k: 'nice', n: 1, w: '44px', c: 'c-md' }, { l: 'Up', k: 'run_secs', n: 1, w: '64px', c: 'c-lg' }, { l: 'Command', c: 'c-xl' }], ui.pSort, 'p') +
    (rows.map(p => `<tr class="click ${sel === p.pid ? 'sel' : ''}" role="button" tabindex="0" data-d="proc" data-dk="${p.pid}">
      <td><input type="checkbox" data-psel="${p.pid}" aria-label="Select ${esc(p.name)}" ${ui.pSel.has(p.pid) ? 'checked' : ''} ${p.own ? '' : 'disabled'}></td>
      <td class="n c-md">${p.pid}</td><td title="${esc(p.name)} (${p.pid})">${esc(p.name)}${p.stopped ? ' <span class="chip warn">paused</span>' : ''}</td><td class="c-lg">${kindChip(p.kind)}</td>
      <td title="${esc(p.group)}">${esc(p.group)}</td>
      <td class="n" title="${p.cpu_pct.toFixed(1)}% of one core">${meter(p.cpu_pct, cm)}${esc(cores(p.cpu_pct))}</td><td class="n">${meter(p.mem_bytes, mm, 'var(--c2)')}${B(p.mem_bytes)}</td>
      <td class="n c-xl">${p.read_bps + p.write_bps ? BS(p.read_bps + p.write_bps) : '<span class="dim">—</span>'}</td>
      <td class="n c-md ${p.nice > 0 ? 'warn' : ''}">${p.nice}</td><td class="n c-lg">${dur(p.run_secs)}</td><td class="cmd c-xl" title="${esc(p.cmd)}">${esc(p.cmd)}</td></tr>`).join('') || '<tr><td colspan="11" class="empty">No matching processes.</td></tr>');
  $('#pBulk').classList.toggle('on', ui.pSel.size > 0);
  $('#pBulkN').textContent = `${ui.pSel.size} selected`;
}

// Bulk actions on the selected rows.
export function procBulk(action) {
  const top = st.S.snapshot.top, procs = [...st.ui.pSel].map(pid => top.find(p => p.pid === pid)).filter(Boolean);
  st.ui.pSel.clear();
  return signalProcs(procs, action);
}
