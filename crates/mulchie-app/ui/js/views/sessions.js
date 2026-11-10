// The Sessions view: agent sessions (an agent process and everything it
// started), from the polled state (`state.sessions`, the running ones and
// the most recently ended, at most 50). A row opens the session detail,
// with its processes, breaches, what Mulchie paused and its activity record.
import { st } from '../store.js';
import { $, esc, B, dur, sortRows, th, tilde, ago } from '../util.js';
import { noticesHtml } from '../findings.js';

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

const statusChip = r => r.status === 'running' ? '<span class="chip ok">running</span>' : '<span class="chip">ended</span>';

function rowsHtml(rows, S, { ended }) {
  const by = breachesBy(S), writes = S.sessions?.write_bytes_available !== false, sel = st.ui.detail?.type === 'session' ? st.ui.detail.key : null;
  return rows.map(r => {
    const n = by.get(r.id) || 0;
    const label = [`Session ${r.agent} in ${r.workspace ? tilde(r.workspace) : 'no workspace'}`, r.status === 'running' ? 'running' : 'ended',
      !ended && `CPU ${coresText(r.cpu_cores)} cores`, !ended && `memory ${MB(r.memory_mb)}`, !ended && `${r.processes} processes`,
      n ? `${n} limit(s) exceeded` : 'not over a limit', r.source === 'hook' ? 'registered by a hook, reported only' : ''].filter(Boolean).join(', ');
    return `<tr class="click ${sel === r.id ? 'sel' : ''}" role="button" tabindex="0" data-d="session" data-dk="${esc(r.id)}" aria-label="${esc(label)}">
      <td>${esc(r.agent)}${r.source === 'hook' ? ' <span class="chip" title="Registered by an agent hook: limits report on it but never act">hook</span>' : ''}</td>
      <td title="${esc(r.workspace || '')}">${r.workspace ? esc(tilde(r.workspace)) : '<span class="dim">none</span>'}</td>
      <td class="c-md">${statusChip(r)}</td>
      <td class="n">${ended ? '<span class="dim">—</span>' : esc(coresText(r.cpu_cores))}</td>
      <td class="n">${ended ? '<span class="dim">—</span>' : MB(r.memory_mb)}</td>
      <td class="n c-md">${ended ? '<span class="dim">—</span>' : r.processes}</td>
      <td class="n c-lg">${esc(dur(r.runtime_secs))}</td>
      ${writes ? `<td class="n c-xl">${r.write_mb_per_hour == null ? '<span class="dim">—</span>' : `${MB(r.write_mb_per_hour)}/h`}</td>` : ''}
      <td class="n">${n ? `<span class="chip bad">${n}</span>` : '<span class="dim">0</span>'}</td></tr>`;
  }).join('');
}

const cols = writes => [{ l: 'Agent', k: 'agent' }, { l: 'Workspace', k: 'workspace' }, { l: 'Status', w: '82px', c: 'c-md' },
  { l: 'CPU (cores)', k: 'cpu_cores', n: 1, w: '108px' }, { l: 'Memory', k: 'memory_mb', n: 1, w: '86px' }, { l: 'Procs', k: 'processes', n: 1, w: '58px', c: 'c-md' },
  { l: 'Runtime', k: 'runtime_secs', n: 1, w: '76px', c: 'c-lg' }, writes && { l: 'Written', k: 'write_mb_per_hour', n: 1, w: '86px', c: 'c-xl' }, { l: 'Breaches', w: '92px', n: 1 }].filter(Boolean);

export function renderSessions() {
  const S = st.S, s = S.sessions || {}, ui = st.ui;
  const writes = s.write_bytes_available !== false;
  const f = $('#sFilter').value.trim().toLowerCase();
  const list = (s.list || []).filter(r => !f || `${r.agent} ${r.workspace || ''} ${r.client || ''} ${r.id}`.toLowerCase().includes(f));
  const running = sortRows(list.filter(r => r.status === 'running'), ui.sSort);
  const ended = list.filter(r => r.status !== 'running').sort((a, b) => (b.ended_unix || 0) - (a.ended_unix || 0));
  $('#sNote').innerHTML = noticesHtml(sessionNotes(S));
  const overN = breachesBy(S).size;
  $('#sCount').textContent = s.enabled === false ? 'tracking off' : `${s.running ?? running.length} running · ${s.ended ?? ended.length} ended${overN ? ` · ${overN} over a limit` : ''}`;
  const n = cols(writes).length;
  const empty = s.enabled === false ? 'Session tracking is off.' : f ? 'No session matches the filter.' : 'No agent is running now. Sessions appear when Claude Code, Codex, Cursor or another agent starts.';
  $('#sess').innerHTML = th(cols(writes), ui.sSort, 's') + (rowsHtml(running, S, { ended: false }) || `<tr><td colspan="${n}" class="empty">${esc(empty)}</td></tr>`);
  $('#sessEnded').innerHTML = ended.length
    ? th(cols(writes).map(c => ({ ...c, k: undefined })), null, 'x') + rowsHtml(ended, S, { ended: true })
    : `<tr><td class="empty">${s.enabled === false ? 'Session tracking is off.' : 'No ended sessions are kept.'}</td></tr>`;
  $('#sEndedH').textContent = `Ended (${ended.length})${ended[0]?.ended_unix ? ` · last ${ago(ended[0].ended_unix)} ago` : ''}`;
}
