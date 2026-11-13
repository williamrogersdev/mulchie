// The Sessions view: agent sessions (an agent process and everything it
// started), from the polled state (`state.sessions`, the running ones and
// the most recently ended, at most 50). A row opens the session detail,
// with its processes, breaches, what Mulchie paused and its activity record.
// Each running row says how long the session has been idle and can be ended
// from the row, or several at once from the selection.
import { st } from '../store.js';
import { $, $$, esc, B, dur, sortRows, th, tilde, ago, agoCell, rowBtn } from '../util.js';
import { noticesHtml, idleSecs } from '../findings.js';
import { endSessions } from '../signals.js';

const MB = mb => B((mb || 0) * 1048576);
export const coresText = c => c == null ? '—' : c < 0.1 && c > 0 ? '<0.1' : c.toFixed(c < 10 ? 1 : 0);

// The breaches of each session, active ones only (`cleared_unix` null).
function breachesBy(S) {
  const m = new Map();
  for (const b of S.sessions?.breaches || []) if (b.cleared_unix == null) m.set(b.session_id, (m.get(b.session_id) || 0) + 1);
  return m;
}

// Plain notes on what the list can and cannot show.
export function sessionNotes(S) {
  const s = S.sessions || {}, out = [];
  if (s.enabled === false) out.push(['warn', 'Session tracking is off', 'sessions.enabled is false in the settings file, so no sessions, activity records or limits exist. Turn it on in Settings → Limits.']);
  if (s.enabled !== false && s.write_bytes_available === false) out.push(['info', 'Disk writes per session are not available here', 'This system does not report bytes written per process, so "written per hour" stays empty and a write limit never acts.']);
  if (s.record_error) out.push(['bad', 'The activity record could not be written', s.record_error]);
  if (s.paused_left?.length) out.push(['warn', `${s.paused_left.length} process(es) still paused by an earlier engine`, 'This engine did not resume them. Resume them from Settings → Limits.', 'settings']);
  return out;
}

// Whether this engine would act on a breach of the session's limits
// (`enforced`, `not_enforced_reason`), in words; null when it would. An
// engine without the field: only hook-registered sessions are known not to be.
const NOT_ENFORCED = {
  hook_registered: 'Registered by an agent hook, not found in the process table, so limits report on it but never lower its priority, pause or stop it.',
  not_automating: 'This engine does not act on limits now: another Mulchie engine holds the lock, or the settings file did not load.',
  no_budget: 'No budget applies to its workspace, so a limit never acts on it.',
  notify_only: 'Every cap of its budget only notifies, so nothing is signalled.',
  ended: 'The session has ended.',
};
export const isHookOnly = r => r.not_enforced_reason === 'hook_registered' || (r.enforced == null && r.source === 'hook');
export function notEnforcedText(r) {
  if (r.enforced === false) return NOT_ENFORCED[r.not_enforced_reason] || `Limits are not enforced on it (${String(r.not_enforced_reason || 'no reason given').replace(/_/g, ' ')}).`;
  return r.enforced == null && r.source === 'hook' ? NOT_ENFORCED.hook_registered : null;
}

// How idle counts, for tooltips and the drawer.
export const IDLE_HELP = 'Idle: since the session last used 5% of a core or more, started a process, or its agent called a hook. Counted from when this engine first saw the session.';
// "active", or how long it has been idle (amber from a day).
export function idleHtml(r) {
  const s = idleSecs(r);
  if (s == null) return '<span class="dim">—</span>';
  const when = new Date(r.last_active_unix * 1000).toLocaleString();
  if (s < 120) return `<span class="ok" title="${esc(IDLE_HELP)}">active</span>`;
  return `<span class="${s >= 86400 ? 'warn' : ''}" title="Last active ${esc(when)}. ${esc(IDLE_HELP)}">${ago(r.last_active_unix)}</span>`;
}

const statusChip = r => r.status === 'running' ? '<span class="chip ok">running</span>' : '<span class="chip">ended</span>';

// The filters above the running list.
const S_F = { all: () => true, h1: r => idleSecs(r) >= 3600, d1: r => idleSecs(r) >= 86400, over: (r, by) => by.has(r.id) };
const S_L = { all: 'All', h1: 'Idle 1h+', d1: 'Idle 1d+', over: 'Over a limit' };

function rowsHtml(rows, S, { ended }) {
  const by = breachesBy(S), writes = S.sessions?.write_bytes_available !== false, sel = st.ui.detail?.type === 'session' ? st.ui.detail.key : null;
  return rows.map(r => {
    const n = by.get(r.id) || 0, idle = idleSecs(r);
    const label = [`Session ${r.agent} in ${r.workspace ? tilde(r.workspace) : 'no workspace'}`, r.status === 'running' ? 'running' : 'ended',
      !ended && `CPU ${coresText(r.cpu_cores)} cores`, !ended && `memory ${MB(r.memory_mb)}`, !ended && `${r.processes} processes`,
      !ended && idle != null && (idle < 120 ? 'active now' : `idle ${ago(r.last_active_unix)}`),
      n ? `${n} limit(s) exceeded` : 'not over a limit', isHookOnly(r) ? 'registered by a hook, reported only' : ''].filter(Boolean).join(', ');
    return `<tr class="click ${sel === r.id ? 'sel' : ''}" role="button" tabindex="0" data-d="session" data-dk="${esc(r.id)}" aria-label="${esc(label)}">
      <td>${ended ? '' : `<input type="checkbox" data-ssel="${esc(r.id)}" aria-label="Select ${esc(r.agent)}" ${st.ui.sSel.has(r.id) ? 'checked' : ''}>`}</td>
      <td>${esc(r.agent)}${isHookOnly(r) ? ' <span class="chip" title="Registered by an agent hook: limits report on it but never act">hook</span>' : ''}</td>
      <td title="${esc(r.workspace || '')}">${r.workspace ? esc(tilde(r.workspace)) : '<span class="dim">none</span>'}</td>
      <td class="c-md">${statusChip(r)}</td>
      <td class="n">${ended ? '<span class="dim">—</span>' : esc(coresText(r.cpu_cores))}</td>
      <td class="n">${ended ? '<span class="dim">—</span>' : MB(r.memory_mb)}</td>
      <td class="n c-md">${ended ? '<span class="dim">—</span>' : r.processes}</td>
      <td class="n c-lg">${esc(dur(r.runtime_secs))}</td>
      <td class="n">${ended ? agoCell(r.ended_unix) : idleHtml(r)}</td>
      ${writes ? `<td class="n c-xl">${r.write_mb_per_hour == null ? '<span class="dim">—</span>' : `${MB(r.write_mb_per_hour)}/h`}</td>` : ''}
      <td class="n">${n ? `<span class="chip bad">${n}</span>` : '<span class="dim">0</span>'}</td>
      <td class="n">${ended ? '' : rowBtn('End…', 's-end', r.id, { danger: true, title: 'Review, then ask this agent and everything it started to quit' })}</td></tr>`;
  }).join('');
}

const cols = (writes, ended) => [ended ? { l: '', w: '30px' } : { check: 1 }, { l: 'Agent', k: 'agent' }, { l: 'Workspace', k: 'workspace' }, { l: 'Status', w: '82px', c: 'c-md' },
  { l: 'CPU (cores)', k: 'cpu_cores', n: 1, w: '108px' }, { l: 'Memory', k: 'memory_mb', n: 1, w: '86px' }, { l: 'Procs', k: 'processes', n: 1, w: '58px', c: 'c-md' },
  { l: 'Runtime', k: 'runtime_secs', n: 1, w: '76px', c: 'c-lg' }, { l: ended ? 'Ended' : 'Idle', k: 'idle', n: 1, w: '70px' }, writes && { l: 'Written', k: 'write_mb_per_hour', n: 1, w: '86px', c: 'c-xl' },
  { l: 'Breaches', w: '84px', n: 1 }, { l: '', w: '58px' }].filter(Boolean);

const running = S => (S.sessions?.list || []).filter(r => r.status === 'running');

export function renderSessions() {
  const S = st.S, s = S.sessions || {}, ui = st.ui;
  const writes = s.write_bytes_available !== false, by = breachesBy(S);
  const f = $('#sFilter').value.trim().toLowerCase();
  const list = (s.list || []).filter(r => !f || `${r.agent} ${r.workspace || ''} ${r.client || ''} ${r.id}`.toLowerCase().includes(f));
  const live = running(S);
  for (const id of ui.sSel) if (!live.some(r => r.id === id)) ui.sSel.delete(id);
  $$('#sSeg button').forEach(b => { b.classList.toggle('on', b.dataset.f === ui.sF); b.textContent = `${S_L[b.dataset.f]} ${live.filter(r => S_F[b.dataset.f](r, by)).length}`; });
  const key = ui.sSort[0] === 'idle' ? (r => idleSecs(r) ?? -1) : ui.sSort[0];
  const shown = sortRows(list.filter(r => r.status === 'running' && S_F[ui.sF](r, by)), [key, ui.sSort[1]]);
  const ended = list.filter(r => r.status !== 'running').sort((a, b) => (b.ended_unix || 0) - (a.ended_unix || 0));
  $('#sNote').innerHTML = noticesHtml(sessionNotes(S));
  const overN = by.size;
  $('#sCount').textContent = s.enabled === false ? 'tracking off' : `${s.running ?? live.length} running · ${s.ended ?? ended.length} ended${overN ? ` · ${overN} over a limit` : ''}`;
  const n = cols(writes).length;
  const empty = s.enabled === false ? 'Session tracking is off.' : f || ui.sF !== 'all' ? 'No session matches the filter.' : 'No agent is running now. Sessions appear when Claude Code, Codex, Cursor or another agent starts.';
  $('#sess').innerHTML = th(cols(writes), ui.sSort, 's') + (rowsHtml(shown, S, { ended: false }) || `<tr><td colspan="${n}" class="empty">${esc(empty)}</td></tr>`);
  $('#sessEnded').innerHTML = ended.length
    ? th(cols(writes, true).map(c => ({ ...c, k: undefined })), null, 'x') + rowsHtml(ended, S, { ended: true })
    : `<tr><td class="empty">${s.enabled === false ? 'Session tracking is off.' : 'No ended sessions are kept.'}</td></tr>`;
  $('#sEndedH').textContent = `Ended (${ended.length})${ended[0]?.ended_unix ? ` · last ${ago(ended[0].ended_unix)} ago` : ''}`;
  $('#sBulk').classList.toggle('on', ui.sSel.size > 0);
  $('#sBulkN').textContent = `${ui.sSel.size} selected`;
}

// "End selected…": one plan and one review for every ticked session.
export async function endSelectedSessions() {
  const rows = running(st.S).filter(r => st.ui.sSel.has(r.id));
  st.ui.sSel.clear();
  return endSessions(rows);
}
