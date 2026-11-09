// The Workspaces view and workspace actions.
import { st } from '../store.js';
import { invoke } from '../ipc.js';
import { $, $$, esc, B, BS, P, tilde, sortRows, th, meter, kindChip } from '../util.js';
import { sparkline } from '../chart.js';
import { confirmBox } from '../dialogs.js';
import { act } from '../act.js';

export function renderWss() {
  const S = st.S, ui = st.ui;
  const f = $('#wFilter').value.trim().toLowerCase();
  let rows = S.snapshot.workspaces.filter(w => !f || w.label.toLowerCase().includes(f) || (w.branch || '').toLowerCase().includes(f));
  const k = ui.wSort[0] === 'io' ? (w => w.read_bps + w.write_bps) : ui.wSort[0] === 'procs' ? (w => w.procs.length) : ui.wSort[0];
  rows = sortRows(rows, [k, ui.wSort[1]]);
  const cm = Math.max(1, ...rows.map(w => w.cpu_pct)), mm = Math.max(1, ...rows.map(w => w.mem_bytes));
  $('#wss').innerHTML = th([{ l: 'Workspace', k: 'label' }, { l: 'Branch', k: 'branch', c: 'wide' }, { l: 'Agents', k: 'agents', n: 1 }, { l: 'Builds', k: 'builds', n: 1 }, { l: 'Procs', k: 'procs', n: 1 }, { l: 'CPU', k: 'cpu_pct', n: 1, w: '110px' }, { l: 'Last 5 min', w: '96px', c: 'wide' }, { l: 'Mem', k: 'mem_bytes', n: 1, w: '120px' }, { l: 'Disk I/O', k: 'io', n: 1, c: 'wide' }, { l: 'State', c: 'wide' }], ui.wSort, 'w') +
    (rows.map(w => `<tr class="click ${ui.wFocus === w.path ? 'sel' : ''}" data-wpath="${esc(w.path)}">
      <td class="trunc">${esc(w.label)}</td><td class="trunc dim mono wide" style="max-width:180px">${esc(w.branch || '—')}</td>
      <td class="n">${w.agents || '<span class="dim">0</span>'}</td><td class="n">${w.builds || '<span class="dim">0</span>'}</td><td class="n">${w.procs.length}</td>
      <td class="n">${meter(w.cpu_pct, cm)}${P(w.cpu_pct)}</td><td class="wide"><canvas class="spark" data-spark="${esc(w.label)}"></canvas></td>
      <td class="n">${meter(w.mem_bytes, mm, 'var(--c2)')}${B(w.mem_bytes)}</td><td class="n wide">${BS(w.read_bps + w.write_bps)}</td>
      <td class="wide">${w.orphan ? '<span class="chip warn">orphan</span> ' : ''}${w.paused ? '<span class="chip warn">paused</span> ' : ''}${w.throttled ? `<span class="chip">${w.throttled} reniced</span> ` : ''}${!w.exists ? '<span class="chip bad">deleted</span>' : ''}</td></tr>`).join('') || '<tr><td colspan="10" class="empty">No workspaces with running processes.</td></tr>');
  const tail = st.H.slice(-100);
  $$('#wss canvas[data-spark]').forEach(c => sparkline(c, tail.map(x => (x.ws.find(w => w[0] === c.dataset.spark) || [0, 0])[1])));
  renderWsSide();
}

function renderWsSide() {
  const S = st.S, ui = st.ui;
  $('#wSplit').classList.toggle('closed', !ui.wFocus);
  if (!ui.wFocus) return;
  const w = S.snapshot.workspaces.find(x => x.path === ui.wFocus);
  if (!w) { $('#wSide').innerHTML = `<div class="chead"><h2>Workspace</h2><button class="btn sm" data-wclose>×</button></div><div class="muted">${esc(tilde(ui.wFocus))}<br><br>No running processes here any more.</div>`; return; }
  const wt = S.diskmap?.worktrees?.find(x => x.path === w.path);
  const procs = [...w.procs].sort((a, b) => b.cpu_pct - a.cpu_pct);
  $('#wSide').innerHTML = `<div class="chead"><h2 class="trunc">${esc(w.label)}</h2><button class="btn sm" data-wclose>×</button></div>
    <dl class="kv"><dt>path</dt><dd>${esc(tilde(w.path))}</dd><dt>branch</dt><dd>${esc(w.branch || '—')}</dd>
    <dt>cpu</dt><dd>${P(w.cpu_pct)}</dd><dt>memory</dt><dd>${B(w.mem_bytes)}</dd><dt>disk i/o</dt><dd>${BS(w.read_bps)} r · ${BS(w.write_bps)} w</dd>
    ${wt ? `<dt>on disk</dt><dd>${B(wt.bytes)} · ${wt.dirty_files} dirty</dd>` : ''}
    ${w.orphan ? `<dt>orphan</dt><dd class="warn">${esc(w.orphan)}</dd>` : ''}</dl>
    <div class="actions">
      ${w.paused ? '<button class="btn" data-wact="resume">Resume all</button>' : '<button class="btn" data-wact="pause">Pause all</button>'}
      <button class="btn" data-wact="deprioritize">Lower priority</button>
      <button class="btn danger" data-wact="stop_builds" ${w.builds ? '' : 'disabled'}>Stop builds</button>
      <button class="btn danger" data-wact="kill_all">Terminate all</button>
      <button class="btn" data-reveal="${esc(w.path)}">Finder</button>
      <button class="btn" data-term="${esc(w.path)}">Terminal</button>
    </div>
    <h3 style="margin-top:14px">${procs.length} processes</h3>
    <table>${th([{ l: 'PID', n: 1 }, { l: 'Name' }, { l: 'CPU', n: 1 }, { l: 'Mem', n: 1 }, { l: 'NI', n: 1 }])}${procs.map(p => `<tr class="click" data-pjump="${p.pid}"><td class="n">${p.pid}</td><td class="trunc" style="max-width:150px">${esc(p.name)} ${p.kind !== 'other' ? kindChip(p.kind) : ''}${p.stopped ? ' <span class="chip warn">T</span>' : ''}</td><td class="n">${p.cpu_pct.toFixed(1)}</td><td class="n">${B(p.mem_bytes)}</td><td class="n">${p.nice}</td></tr>`).join('')}</table>`;
}

export async function wsAct(path, action) {
  const w = st.S.snapshot.workspaces.find(x => x.path === path);
  const words = {
    pause: ['Pause all', 'Sends SIGSTOP to every process here. Agents freeze until you resume them.'],
    stop_builds: ['Stop builds', `Sends SIGTERM to ${w?.builds} build process(es). Agents keep running.`],
    kill_all: ['Terminate all', `Sends SIGTERM to all ${w?.procs.length} processes here, agents included.`],
  }[action];
  if (words && !(await confirmBox(`${words[0]}: ${w?.label}?`, words[1], words[0]))) return;
  act(() => invoke('workspace_action', { path, action }), n => `${action.replace('_', ' ')} → ${n} process(es)`);
}
