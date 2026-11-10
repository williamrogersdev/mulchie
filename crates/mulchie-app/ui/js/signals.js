// Signalling processes: always a list first, then each signal carries the
// start time the user saw, so a pid reused meanwhile is refused (the backend
// checks it against the engine's current snapshot).
import { st, hooks } from './store.js';
import { invoke } from './ipc.js';
import { esc, B, toast } from './util.js';
import { modal, reasonText } from './dialogs.js';
import { cores } from './consumers.js';

export const ACTIONS = {
  term: { verb: 'Quit', title: 'Quit', note: 'Sends SIGTERM so each process can save and exit cleanly.', danger: true },
  kill: { verb: 'Force quit', title: 'Force quit', note: 'Sends SIGKILL. It cannot be caught, so unsaved work in these processes is lost.', danger: true },
  stop: { verb: 'Pause', title: 'Pause', note: 'Sends SIGSTOP. The processes freeze until you resume them.', danger: false },
  cont: { verb: 'Resume', title: 'Resume', note: 'Sends SIGCONT to paused processes.', danger: false },
  renice: { verb: 'Lower priority', title: 'Lower the priority of', note: 'Renices the processes so other work gets the CPU first. They keep running.', danger: false },
};

const row = p => `<div class="pi${p.own ? '' : ' off'}">${p.own ? `<input type="checkbox" data-sp="${p.pid}" checked>` : '<span></span>'}
  <div><div class="nm">${esc(p.name)} <span class="dim mono">${p.pid}</span> <span class="chip ${esc(p.kind)}">${esc(p.kind)}</span></div>
  <div class="path" title="${esc(p.cmd)}">${esc(p.cmd)}</div></div>
  <div class="sz">${esc(cores(p.cpu_pct))} · ${B(p.mem_bytes)}</div>
  ${p.own ? '' : '<div class="meta"><span class="why">Another user\'s process; left alone</span></div>'}</div>`;

// Confirm, then send `action` to the checked processes of `procs`. `what`
// names the group in the title ("Quit Google Chrome?").
export async function signalProcs(procs, action, what) {
  const a = ACTIONS[action];
  if (!procs.length) return toast('No processes to signal.');
  const mine = procs.filter(p => p.own);
  const html = `<div>${a.note}</div>
    <div class="sum"><span>Processes <b>${mine.length}</b></span>${procs.length > mine.length ? `<span>Not yours <b>${procs.length - mine.length}</b></span>` : ''}</div>
    <div class="plan">${procs.map(row).join('')}</div>
    <div class="note">Each process is checked again before the signal: if its pid now belongs to a different process, it is skipped.</div>`;
  const picked = () => [...document.querySelectorAll('#mBody input[data-sp]')].filter(x => x.checked).map(x => procs.find(p => p.pid === +x.dataset.sp));
  const ok = await modal({
    title: `${a.title} ${what || `${mine.length} process(es)`}?`, html, wide: true, yes: a.verb, danger: a.danger,
    setup: (body, yesB) => {
      const upd = () => { const n = picked().length; yesB.disabled = !n; yesB.textContent = n ? `${a.verb} ${n}` : a.verb; };
      body.onchange = upd;
      upd();
    },
  });
  if (!ok) return;
  const list = picked();
  const body = action === 'renice' ? { action: 'renice', nice: Math.max(10, st.S.config?.build_nice || 10) } : { action };
  const results = [];
  for (const p of list) {
    try { await invoke('process_action', { pid: p.pid, startTime: p.start_time, action: body }); results.push([p, null]); }
    catch (e) { results.push([p, String(e)]); }
  }
  await hooks.refresh(true);
  const bad = results.filter(r => r[1]);
  if (!bad.length) return toast(`${a.verb}: ${results.length} process(es)`);
  const html2 = `<div class="sum"><span>Done <b>${results.length - bad.length}</b></span><span>Not sent <b>${bad.length}</b></span></div>
    <div class="plan">${results.map(([p, err]) => `<div class="pi ${err ? 'off' : ''}"><span class="${err ? 'warn' : 'ok'}">${err ? '–' : '✓'}</span>
      <div><div class="nm">${esc(p.name)} <span class="dim mono">${p.pid}</span></div></div><div class="sz"></div>
      ${err ? `<div class="meta"><span class="why">${esc(reasonText(err))}</span></div>` : ''}</div>`).join('')}</div>`;
  return modal({ title: `${a.verb}: ${results.length - bad.length} of ${results.length} done`, html: html2, wide: true, yes: '', cancel: 'Close' });
}
