// The Processes view and process actions.
import { st, hooks } from '../store.js';
import { invoke } from '../ipc.js';
import { $, esc, B, BS, dur, tilde, toast, sortRows, th, meter, kindChip } from '../util.js';
import { confirmBox } from '../dialogs.js';

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
  $('#pCount').textContent = `${rows.length} shown · ${S.snapshot.proc_count} total · the top by CPU, memory and I/O are tracked`;
  $('#procs').innerHTML = th([{ check: 1 }, { l: 'PID', k: 'pid', n: 1 }, { l: 'Name', k: 'name' }, { l: 'Kind', k: 'kind' }, { l: 'Workspace / app', k: 'group' }, { l: 'CPU %', k: 'cpu_pct', n: 1, w: '110px' }, { l: 'Mem', k: 'mem_bytes', n: 1, w: '120px' }, { l: 'Disk I/O', k: 'io', n: 1, c: 'wide' }, { l: 'NI', k: 'nice', n: 1 }, { l: 'Up', k: 'run_secs', n: 1, c: 'wide' }, { l: 'Command', c: 'wide' }], ui.pSort, 'p') +
    (rows.map(p => `<tr class="click ${ui.pFocus === p.pid ? 'sel' : ''}" data-pid="${p.pid}">
      <td><input type="checkbox" data-psel="${p.pid}" ${ui.pSel.has(p.pid) ? 'checked' : ''} ${p.own ? '' : 'disabled'}></td>
      <td class="n">${p.pid}</td><td class="trunc" style="max-width:190px" title="${esc(p.name)}">${esc(p.name)}${p.stopped ? ' <span class="chip warn">stopped</span>' : ''}</td><td>${kindChip(p.kind)}</td>
      <td class="trunc">${esc(p.group)}</td>
      <td class="n">${meter(p.cpu_pct, cm)}${p.cpu_pct.toFixed(1)}</td><td class="n">${meter(p.mem_bytes, mm, 'var(--c2)')}${B(p.mem_bytes)}</td>
      <td class="n wide">${p.read_bps + p.write_bps ? BS(p.read_bps + p.write_bps) : '<span class="dim">—</span>'}</td>
      <td class="n ${p.nice > 0 ? 'warn' : ''}">${p.nice}</td><td class="n wide">${dur(p.run_secs)}</td><td class="cmd wide" title="${esc(p.cmd)}">${esc(p.cmd)}</td></tr>`).join('') || '<tr><td colspan="11" class="empty">No matching processes.</td></tr>');
  $('#pBulk').classList.toggle('on', ui.pSel.size > 0);
  $('#pBulkN').textContent = `${ui.pSel.size} selected`;
  renderProcSide();
}

export function findProc(pid) {
  const s = st.S.snapshot;
  return s.top.find(x => x.pid === pid) || s.workspaces.flatMap(w => w.procs).find(x => x.pid === pid);
}

function renderProcSide() {
  const ui = st.ui, s = st.S.snapshot;
  $('#pSplit').classList.toggle('closed', !ui.pFocus);
  if (!ui.pFocus) return;
  const p = findProc(ui.pFocus);
  if (!p) { $('#pSide').innerHTML = `<div class="chead"><h2>PID ${ui.pFocus}</h2><button class="btn sm" data-pclose>×</button></div><div class="muted">Exited, or no longer among the tracked top processes.</div>`; return; }
  const all = [...s.top, ...s.workspaces.flatMap(w => w.procs)];
  const parent = all.find(x => x.pid === p.ppid);
  const kids = [...new Map(all.filter(x => x.ppid === p.pid).map(x => [x.pid, x])).values()];
  $('#pSide').innerHTML = `<div class="chead"><h2>${esc(p.name)} <span class="dim mono">${p.pid}</span></h2><button class="btn sm" data-pclose>×</button></div>
    <div>${kindChip(p.kind)} ${p.own ? '' : '<span class="chip">other user</span>'} ${p.stopped ? '<span class="chip warn">stopped</span>' : ''} ${p.nice > 0 ? `<span class="chip warn">nice ${p.nice}</span>` : ''}</div>
    <dl class="kv">
      <dt>cpu</dt><dd>${p.cpu_pct.toFixed(1)}% <span class="dim">(100% = one core)</span></dd>
      <dt>memory</dt><dd>${B(p.mem_bytes)}</dd>
      <dt>disk read</dt><dd>${BS(p.read_bps)}</dd>
      <dt>disk write</dt><dd>${BS(p.write_bps)}</dd>
      <dt>uptime</dt><dd>${dur(p.run_secs)}</dd>
      <dt>parent</dt><dd>${parent ? `<a style="color:var(--c1);cursor:pointer" data-pfocus="${parent.pid}">${parent.pid}</a> ${esc(parent.name)}` : p.ppid}</dd>
      <dt>children</dt><dd>${kids.length ? kids.slice(0, 12).map(k => `<a style="color:var(--c1);cursor:pointer" data-pfocus="${k.pid}">${k.pid}</a> ${esc(k.name)}`).join(', ') : '—'}</dd>
      <dt>group</dt><dd>${esc(p.group)}</dd>
      <dt>workspace</dt><dd>${esc(tilde(p.workspace) || '—')}</dd>
    </dl>
    <pre class="code">${esc(p.cmd)}</pre>
    ${p.own ? `<div class="actions">
      ${p.stopped ? '<button class="btn" data-pact="cont">Resume</button>' : '<button class="btn" data-pact="stop">Pause</button>'}
      <button class="btn" data-pact="renice">Lower priority</button>
      <button class="btn danger" data-pact="term">Terminate</button>
      <button class="btn danger" data-pact="kill">Kill -9</button>
      ${p.workspace ? `<button class="btn" data-ws="${esc(p.workspace)}">Workspace →</button>` : ''}
    </div>` : '<div class="muted" style="margin-top:10px">Owned by another user; Mulchie will not signal it.</div>'}`;
}

export async function procAct(pids, action) {
  const list = pids.map(pid => findProc(pid) || { pid, name: '?' });
  if (['term', 'kill'].includes(action)) {
    const ok = await confirmBox(action === 'kill' ? `Kill ${list.length} process(es) with SIGKILL?` : `Terminate ${list.length} process(es)?`,
      `${action === 'kill' ? 'SIGKILL cannot be caught, so unsaved work in these processes is lost.' : 'Sends SIGTERM so each process can exit cleanly.'}<ul>${list.map(p => `<li>${p.pid} ${esc(p.name)} <span class="dim">${esc(p.group || '')}</span></li>`).join('')}</ul>`,
      action === 'kill' ? 'Kill' : 'Terminate');
    if (!ok) return;
  }
  const body = action === 'renice' ? { action: 'renice', nice: Math.max(10, st.S.config.build_nice) } : { action };
  let okN = 0; const errs = [];
  for (const pid of pids) {
    try { await invoke('process_action', { pid, action: body }); okN++; } catch (e) { errs.push(`${pid}: ${e}`); }
  }
  st.ui.pSel.clear();
  if (errs.length) toast(`${okN} ok, ${errs.length} failed\n${errs.slice(0, 4).join('\n')}`, true);
  else toast(`${action} → ${okN} process(es)`);
  await hooks.refresh(true);
}
