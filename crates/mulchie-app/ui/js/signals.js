// Acting on processes: always the engine's plan first (POST /v1/process/plan
// or the engine method behind it), shown as the list of processes it would
// affect, then exactly the items the user left ticked go to execute. Every
// item carries the start time the plan saw, and the safety gate checks it
// again, so a pid reused meanwhile is skipped. A protected target (the
// operating system, Mulchie, the app or terminal that started it) comes back
// refused, with the reason shown.
import { st, hooks } from './store.js';
import { invoke } from './ipc.js';
import { esc, toast, hideToast } from './util.js';
import { modal, reasonText } from './dialogs.js';

// Group actions the engine plans (`ConsumerGroup.actions`).
export const GROUP_ACTIONS = {
  quit: { verb: 'Quit', note: 'Asks the main process to quit (SIGTERM) so it can save and exit cleanly; its helper processes are expected to exit with it.', danger: true },
  force_quit: { verb: 'Force quit', note: 'Sends SIGKILL to every process. It cannot be caught, so unsaved work is lost.', danger: true },
  pause: { verb: 'Pause', note: 'Sends SIGSTOP. The processes freeze until you resume them.', danger: false },
  resume: { verb: 'Resume', note: 'Sends SIGCONT to paused processes.', danger: false },
  lower_priority: { verb: 'Lower priority', note: 'Raises the nice value so other work gets the CPU first. They keep running.', danger: false },
  stop_builds: { verb: 'Stop builds', note: 'Asks the build processes here to quit (SIGTERM). Agents and editors keep running.', danger: true },
  restart: { verb: 'Restart', note: 'Quits the app, waits for it to exit, then opens it again.', danger: true },
};

// Actions on an agent session's processes (`{session, action}` plans): what
// happens to them, and to the agent itself.
export const SESSION_NOTES = {
  pause: 'Pauses (SIGSTOP) these processes. They freeze, keep their memory and carry on where they were when resumed.',
  resume: 'Resumes (SIGCONT) the processes of this session that are paused.',
  lower_priority: 'Raises the nice value of these processes so other work gets the CPU first. They keep running.',
  quit: 'Asks each of these processes to quit (SIGTERM) so it can clean up. Work they have not saved may be lost.',
  force_quit: 'Kills each of these processes (SIGKILL). It cannot be caught, so anything unsaved is lost.',
};
export const sessionNote = (action, includeAgent) => `${SESSION_NOTES[action] || GROUP_ACTIONS[action]?.note || ''} ${includeAgent
  ? 'The agent process itself is included.'
  : 'The agent process itself is left alone and keeps running.'}`;

// Signals for one process (`ProcessPlanRequest.signal`).
export const SIGNALS = {
  term: { verb: 'Quit', note: 'Sends SIGTERM so the process can save and exit cleanly.', danger: true },
  kill: { verb: 'Force quit', note: 'Sends SIGKILL. It cannot be caught, so unsaved work is lost.', danger: true },
  stop: { verb: 'Pause', note: 'Sends SIGSTOP. The process freezes until you resume it.', danger: false },
  cont: { verb: 'Resume', note: 'Sends SIGCONT.', danger: false },
};

const HOW = { signalled: 'signalled', exits_with_app: 'exits with the app', reniced: 'lower priority' };
const REFUSED = {
  protected: 'Mulchie will not act on this',
  unknown_group: 'Nothing of it is running now',
  unknown_pid: 'That process is not running',
  restart_unavailable: 'Restart is not available for this',
  no_builds: 'No build processes are running here',
  invalid_request: 'Not a valid request',
};

const itemRow = (i, idx) => `<div class="pi ${i.ready ? '' : 'off'}">
  ${i.ready ? `<input type="checkbox" data-pi="${idx}" checked aria-label="Include ${esc(i.name || i.target)}">` : '<span></span>'}
  <div><div class="nm">${esc(i.name || i.target)} <span class="dim mono">${esc(i.target)}</span></div>
  ${i.verdict ? `<div class="path">${esc(i.verdict)}</div>` : ''}</div><div class="sz">${esc((i.request?.signal || i.request?.action || '').replace(/_/g, ' '))}</div>
  ${i.ready ? '' : `<div class="meta"><span class="why">${esc(reasonText(i.reason))}</span></div>`}</div>`;

const affectedHtml = plan => plan.affected?.length
  ? `<h4>Affected (${plan.affected.length})</h4>${plan.affected.slice(0, 60).map(a => `<div class="pi"><span></span><div><div class="nm">${esc(a.name)} <span class="dim mono">${a.pid}</span></div></div><div class="sz dim">${esc(HOW[a.how] || a.how)}</div></div>`).join('')}${plan.affected.length > 60 ? `<div class="dim">and ${plan.affected.length - 60} more</div>` : ''}`
  : '';

// Show the plans; resolve to the approved requests, or null.
async function review(plans, { title, verb, note, danger, lines = [] }) {
  const items = plans.flatMap(p => p.items);
  const ready = items.filter(i => i.ready).length;
  const refused = plans.filter(p => p.refused);
  const html = `${note ? `<div>${esc(note)}</div>` : ''}
    ${lines.map(l => `<p class="why-s warn">${esc(l)}</p>`).join('')}
    ${plans.map(p => `<p class="${p.refused ? 'why-s warn' : 'dim'}">${p.refused ? `<b>${esc(REFUSED[p.refused] || reasonText(p.refused))}:</b> ` : ''}${esc(p.detail)}</p>${p.restart ? `<p class="dim">Then: ${esc(p.restart)}</p>` : ''}`).join('')}
    ${items.length ? `<div class="sum"><span>Ready <b>${ready}</b></span>${items.length > ready ? `<span>Left alone <b>${items.length - ready}</b></span>` : ''}</div>
    <div class="plan">${items.map(itemRow).join('')}${plans.map(affectedHtml).join('')}</div>` : ''}
    ${ready ? '<div class="note">Each item is checked again just before it is sent: a process that exited, or whose pid now belongs to another process, is skipped.</div>' : ''}`;
  const picked = () => [...document.querySelectorAll('#mBody input[data-pi]')].filter(x => x.checked).map(x => items[+x.dataset.pi]);
  const ok = await modal({
    title: refused.length === plans.length ? `${title}: not possible` : title, html, wide: true, danger,
    yes: ready ? verb : '', cancel: ready ? 'Cancel' : 'Close',
    setup: (body, yesB) => {
      const upd = () => { const n = picked().length; yesB.disabled = !n; yesB.textContent = n ? `${verb} (${n})` : verb; };
      body.onchange = upd;
      upd();
    },
  });
  if (!ok) return null;
  return picked().map(i => i.request);
}

function showResults(results, verb) {
  const done = results.filter(r => r.status === 'done');
  const html = `<div class="sum"><span>Done <b>${done.length}</b></span>${results.length > done.length ? `<span>Skipped <b>${results.length - done.length}</b></span>` : ''}</div>
    <div class="plan">${results.map(r => `<div class="pi ${r.status === 'done' ? '' : 'off'}"><span class="${r.status === 'done' ? 'ok' : 'warn'}">${r.status === 'done' ? '✓' : '–'}</span>
      <div><div class="nm">${esc(r.target)} <span class="dim">${esc(String(r.action || '').replace(/_/g, ' '))}</span></div></div><div class="sz"></div>
      ${r.status === 'done' ? '' : `<div class="meta"><span class="why">${esc(reasonText(r.reason))}</span> <span class="dim">${esc(r.detail || '')}</span></div>`}</div>`).join('')}</div>`;
  return modal({ title: `${verb}: ${done.length} of ${results.length} done`, html, wide: true, yes: '', cancel: 'Close' });
}

// Plan each request, review them together, execute what was approved.
// `expect` (optional): {pid, start_time} that a plan must still name, for
// actions started from something the user looked at earlier (a finding).
// `same` (optional): [{pid, start_time}] the requests are about; an item
// for one of those pids with another start time is a different process
// now, so it is left alone and said so. `after` runs once the user
// approved and the items are being sent.
export async function planAndRun(requests, { title, verb, note, danger = true, expect, same, after } = {}) {
  toast('Checking…');
  // One call plans them all, and the engine keeps them as one plan, so a
  // selection of any size is one review and one approval.
  let plans;
  try { plans = (await invoke('process_plans', { requests })).plans; }
  catch (e) { return toast(String(e), true); }
  hideToast();
  if (expect) {
    const same = plans.some(p => p.affected?.some(a => a.pid === expect.pid && a.start_time === expect.start_time));
    if (!same) {
      return modal({ title: `${title}: not possible`, html: `<p class="why-s warn">Process ${esc(expect.pid)} has exited, or its pid now belongs to a different process than the one observed. Nothing was sent.</p>`, yes: '', cancel: 'Close' });
    }
  }
  const lines = same ? keepSameProcesses(plans, same) : [];
  const items = await review(plans, { title, verb, note, danger, lines });
  if (!items?.length) return;
  try { after?.(); } catch { /* the action was sent; only the callback failed */ }
  toast(`${verb}…`);
  let res;
  try { res = await invoke('execute', { batch: 'process', items }); }
  catch (e) { await hooks.refresh(true); return toast(String(e), true); }
  hideToast();
  await hooks.refresh(true);
  return showResults(res, verb);
}

// Mark items whose pid now names another process (another start time) as
// not ready; return a line for each.
export function keepSameProcesses(plans, same) {
  const want = new Map(same.map(p => [Number(p.pid), Number(p.start_time)]));
  const lines = [];
  for (const p of plans) {
    for (const i of p.items || []) {
      const pid = i.request?.pid;
      if (!want.has(pid) || i.request.expected_start_time === want.get(pid)) continue;
      i.ready = false;
      i.reason = 'not_same_process';
      lines.push(`pid ${pid} is no longer the same process (it started at a different time), so it is left alone.`);
    }
    for (const a of p.affected || []) {
      if (want.has(a.pid) && a.start_time !== want.get(a.pid) && !lines.some(l => l.startsWith(`pid ${a.pid} `))) {
        lines.push(`pid ${a.pid} is no longer the same process (it started at a different time), so it is left alone.`);
      }
    }
    if (p.affected) p.affected = p.affected.filter(a => !want.has(a.pid) || a.start_time === want.get(a.pid));
  }
  return lines;
}

// Resume processes Mulchie paused ([{pid, start_time}]), each only while it
// is still that process.
export function resumePaused(list, title = 'Resume the processes Mulchie paused?') {
  return planAndRun(list.map(p => ({ pid: p.pid, signal: 'cont' })), {
    title, verb: 'Resume', danger: false, same: list,
    note: 'Sends SIGCONT to each process that is still the one Mulchie paused: the same pid and the same start time.',
  });
}

// A group action ("quit Google Chrome").
export function groupAction(key, action, name) {
  const a = GROUP_ACTIONS[action];
  return planAndRun([{ group: key, action }], { title: `${a.verb} ${name}?`, verb: a.verb, note: a.note, danger: a.danger });
}

// A signal to one or more processes (each planned on its own).
export function signalPids(pids, signal, what) {
  const a = SIGNALS[signal];
  return planAndRun(pids.map(pid => ({ pid, signal })), { title: `${a.verb} ${what || `${pids.length} process(es)`}?`, verb: a.verb, note: a.note, danger: a.danger });
}

// The nice value "Lower priority" sets: a finding's own suggestion, else
// the configured build_nice, else 10.
export const reniceTarget = nice => Math.min(20, Math.max(1, Math.round(nice ?? st.S?.config?.build_nice ?? 10)));
// Whether lowering would change anything: not when it already runs at or
// above that nice value.
export const canLower = (p, nice) => !(p?.nice >= reniceTarget(nice));

// Lower the priority of one or more processes: plans have no per-process
// renice, so one confirmation lists them all, then the gate checks each
// start time the user saw. Cancel sends nothing.
export async function reniceProcs(procs, nice) {
  const target = reniceTarget(nice);
  const todo = procs.filter(p => canLower(p, target));
  const already = procs.length - todo.length;
  if (!todo.length) return toast(`Already at nice ${target} or lower priority; nothing to change.`);
  const list = todo.slice(0, 40).map(p => `<div class="pi"><span></span><div><div class="nm">${esc(p.name)} <span class="dim mono">${p.pid}</span></div></div><div class="sz dim">${p.nice != null ? `nice ${esc(p.nice)} → ${target}` : `→ ${target}`}</div></div>`).join('');
  const ok = await modal({
    title: todo.length === 1 ? `Lower the priority of ${todo[0].name} (${todo[0].pid})?` : `Lower the priority of ${todo.length} processes?`,
    html: `<div>Sets the nice value to ${target}, so other work gets the CPU first. They keep running. Only the processes started at the times Mulchie saw are changed.</div>
      <div class="plan">${list}${todo.length > 40 ? `<div class="dim">and ${todo.length - 40} more</div>` : ''}</div>${already ? `<p class="dim">${already} already at nice ${target} or above, left alone.</p>` : ''}`,
    yes: 'Lower priority', danger: false, wide: todo.length > 1,
  });
  if (!ok) return;
  const done = [], not = [];
  for (const p of todo) {
    try {
      const r = await invoke('renice', { pid: p.pid, startTime: p.start_time, nice: target });
      (r.status === 'done' ? done : not).push(r.status === 'done' ? p : `${p.name} (${p.pid}): ${reasonText(r.reason)}`);
    } catch (e) { not.push(`${p.name} (${p.pid}): ${e}`); }
  }
  await hooks.refresh(true);
  if (!not.length) return toast(done.length === 1 ? `${done[0].name} now runs at nice ${target}` : `${done.length} processes now run at nice ${target}`);
  return toast(`Lowered ${done.length} of ${todo.length}. Not changed: ${not.join('; ')}`, true);
}

export const reniceProc = (p, nice) => reniceProcs([p], nice);
