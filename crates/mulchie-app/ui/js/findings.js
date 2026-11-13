// Findings ("needs attention") and status notices about Mulchie itself.
import { st } from './store.js';
import { esc, B, ago, clock } from './util.js';
import { cpuSummary, memSummary, ranked } from './consumers.js';

// The engine's watch findings ("observations"), most severe first. Dismissed
// ones only when asked for.
export const SEV_LVL = { high: 'bad', medium: 'warn', low: 'info', info: 'info' };
export const SEV_TEXT = { high: 'high', medium: 'medium', low: 'low', info: 'info' };
export function watchFindings(S, dismissed = false) {
  return (S?.findings || []).filter(f => !!f.dismissed === dismissed);
}

// Finding categories (`Finding.category`), in the order lists show them.
export const CATEGORY = { security: 'Security', limit: 'Limits', hygiene: 'Hygiene', posture: 'Mulchie setup', update: 'Updates', other: 'Other' };
export const CATEGORY_ORDER = ['security', 'limit', 'hygiene', 'posture', 'update', 'other'];

// The API call a suggestion names: `api_request` ({method, route, body}),
// or for an engine without it, the same call parsed from the `api` words.
function apiCall(sg) {
  const r = sg?.api_request;
  if (r && typeof r.method === 'string' && typeof r.route === 'string') return { method: r.method, route: r.route, body: r.body ?? null };
  const m = /^(GET|POST) (\/v1\/\S+)(?: (\{.*\}))?$/.exec(sg?.api || '');
  if (!m) return null;
  try { return { method: m[1], route: m[2], body: m[3] ? JSON.parse(m[3]) : null }; } catch (e) { return null; }
}
const SID = /^s-\d+-\d+$/;
// A suggestion that plans an action on a session's processes (limit
// findings): `POST /v1/process/plan {"session": …, "action": …}`.
export function planApi(sg) {
  const c = apiCall(sg), b = c?.body;
  return c?.method === 'POST' && c.route === '/v1/process/plan' && b && typeof b.session === 'string' && SID.test(b.session) && typeof b.action === 'string'
    ? { session: b.session, action: b.action } : null;
}
// A suggestion to look at a session: `GET /v1/sessions/<id>` → the id.
export function sessionApi(sg) {
  const c = apiCall(sg), m = c?.method === 'GET' && /^\/v1\/sessions\/(s-\d+-\d+)$/.exec(c.route);
  return m ? m[1] : null;
}
// A command the user may run themselves (update findings, engines without
// `evidence.updates`): "Run: <cmd>".
export const runCommand = sg => !sg?.request && !sg?.api && !sg?.api_request && /^Run: /.test(sg?.label || '') ? sg.label.slice(5) : null;
// An update finding's packages (`evidence.updates`), one per row.
export const updateItems = f => Array.isArray(f?.evidence?.updates) ? f.evidence.updates.filter(u => u && typeof u.package === 'string') : [];

// The short button for a suggestion: what it does, not the whole sentence.
export function suggestionLabel(sg) {
  const r = sg.request;
  const pr = planApi(sg);
  if (pr) return { pause: 'Pause what it started…', resume: 'Resume…', lower_priority: 'Lower its priority…', quit: 'Quit what it started…' }[pr.action] || 'Plan an action…';
  if (!r) return sg.api ? 'Dismiss…' : sg.label;
  if (r.action === 'signal') return { term: 'Quit…', kill: 'Force quit…', stop: 'Pause…', cont: 'Resume…' }[r.signal] || sg.label;
  if (r.action === 'renice') return 'Lower priority…';
  return sg.label;
}

// A watch finding as a "needs attention" row: opens its detail, and its
// first suggestion with a request is the primary action.
function fromWatch(f) {
  const i = f.suggested.findIndex(sg => sg.request || planApi(sg));
  return {
    lvl: SEV_LVL[f.severity] || 'info', t: f.title, d: f.why, open: ['finding', f.id], since: f.first_seen_unix,
    act: i >= 0 ? { label: suggestionLabel(f.suggested[i]), run: `fx:${i}:${f.id}` } : { label: 'Details', run: `d:finding:${f.id}` },
  };
}

// How long a running session has done nothing the engine saw, in seconds;
// null for an ended one, or an engine that does not report it.
export const idleSecs = r => r.status === 'running' && r.last_active_unix ? Math.max(0, Date.now() / 1000 - r.last_active_unix) : null;
export const IDLE_SESSION_SECS = 86400;

// What can be cleaned up now, each with how long it has been unused and the
// action that resolves it: stale build caches, idle agent worktrees, and
// agent sessions that have done nothing for a day.
export function cleanup(S) {
  const out = [], c = S.config || {};
  const oldest = ts => ts.filter(Boolean).reduce((a, t) => Math.min(a, t), Infinity);
  const sweep = (S.diskmap?.worktrees || []).filter(w => w.sweepable);
  if (sweep.length) {
    const t = oldest(sweep.map(w => w.last_activity_unix));
    out.push({ kind: 'cleanup', lvl: 'warn', t: `${sweep.length} idle agent worktree${sweep.length === 1 ? '' : 's'} · ${B(S.diskmap.sweepable_bytes)}`, age: isFinite(t) ? `idle up to ${ago(t)}` : '',
      d: `Idle ${c.worktree_idle_days ?? '?'}+ days with nothing running in them. A sweep keeps branches and saves uncommitted changes first.`, go: 'disk', act: { label: 'Sweep…', run: 'sweep' } });
  }
  const stale = (S.scan?.items || []).filter(i => i.stale), bytes = S.scan?.stale_bytes || 0;
  if (bytes > 1e9) {
    const t = oldest(stale.map(i => i.last_used_unix));
    out.push({ kind: 'cleanup', lvl: 'warn', t: `${B(bytes)} of stale build caches`, age: isFinite(t) ? `unused up to ${ago(t)}` : '',
      d: `${stale.length} cache(s) unused within the stale window, with nothing running in the project. The next build recreates them.`, go: 'disk', act: { label: 'Clear…', run: 'stale' } });
  }
  const idle = (S.sessions?.list || []).map(idleSecs).filter(s => s != null && s >= IDLE_SESSION_SECS);
  if (idle.length) {
    out.push({ kind: 'cleanup', lvl: 'info', t: `${idle.length} agent session${idle.length === 1 ? '' : 's'} idle for over a day`, age: `longest ${ago(Date.now() / 1000 - Math.max(...idle))}`,
      d: 'They used almost no CPU, started nothing and reported nothing in that time. Review them and end the ones you are done with.', go: 'sessions', act: { label: 'Review…', run: 'idle-sessions' } });
  }
  return out;
}

// What needs attention, each with where it leads (`open`: a detail, or `go`:
// a view) and one primary action (`act`: a runner name main.js knows): the
// engine's findings first, then disk, CPU and memory pressure.
export function findings() {
  const S = st.S, out = [], s = S.snapshot, c = S.config;
  if (!s.disk_total || !c) return out;
  out.push(...watchFindings(S).filter(f => f.severity !== 'info').map(fromWatch));
  const stale = S.scan?.stale_bytes || 0, n = S.diskmap?.worktrees?.filter(w => w.sweepable).length || 0;
  const diskAct = stale ? { label: 'Clear stale…', run: 'stale' } : n ? { label: 'Sweep…', run: 'sweep' } : { label: 'Show disk', run: 'go:disk' };
  const free = s.disk_free / 1e9;
  if (free < c.low_disk_gb) out.push({ lvl: 'bad', t: `Disk nearly full: ${B(s.disk_free)} free`, d: 'Clear stale caches or sweep idle worktrees.', go: 'disk', act: diskAct });
  else if (S.low_disk) out.push({ lvl: 'warn', t: `Free space under ${c.alert_below_gb} GB`, d: `${B(s.disk_free)} free.`, go: 'disk', act: diskAct });
  if (S.forecast?.hours_to_low != null && S.forecast.hours_to_low < 24) out.push({ lvl: 'warn', t: `Disk filling at ${B(S.forecast.bytes_per_hour)}/h`, d: `Reaches the ${c.low_disk_gb} GB floor in about ${S.forecast.hours_to_low.toFixed(1)} h at this rate.`, go: 'disk', act: diskAct });
  if (s.cpu_count && s.load[0] > s.cpu_count * 2) {
    const sum = cpuSummary(S), top = ranked(S, 'cpu_pct')[0];
    out.push({ lvl: s.load[0] > s.cpu_count * 6 ? 'bad' : 'warn', t: `CPU ${sum.word}: ${(s.load[0] / s.cpu_count).toFixed(1)}× more work than cores`, d: sum.headline, open: ['cpu', ''], act: top ? { label: `Show ${top.short}`, run: `d:group:${top.key}` } : { label: 'Show CPU', run: 'd:cpu:' } });
  }
  const mp = s.mem_total ? s.mem_available / s.mem_total * 100 : 100;
  if (mp < c.throttle_mem_free_pct || s.swap_used > 8e9) {
    const sum = memSummary(S), top = ranked(S, 'mem_bytes')[0];
    out.push({ lvl: mp < c.throttle_mem_free_pct ? 'bad' : 'warn', t: mp < c.throttle_mem_free_pct ? `Memory pressure: ${mp.toFixed(0)}% available` : `${B(s.swap_used)} in swap`, d: sum.headline, open: ['mem', ''], act: top ? { label: `Show ${top.short}`, run: `d:group:${top.key}` } : { label: 'Show memory', run: 'd:mem:' } });
  }
  out.push(...cleanup(S));
  if (S.throttled) out.push({ lvl: 'ok', t: `${S.throttled} build process(es) at lower priority`, d: 'The optimizer reniced them under pressure.', open: ['agents', ''], act: { label: 'Show', run: 'd:agents:' } });
  if (S.slots?.waiting?.length) out.push({ lvl: 'ok', t: `${S.slots.waiting.length} job(s) queued for a build slot`, d: S.slots.waiting.map(w => w.label).join(', '), open: ['agents', ''], act: { label: 'Show', run: 'd:agents:' } });
  return out;
}

// Where a finding leads when its text is clicked: a detail or a view.
const target = f => f.open ? `data-d="${esc(f.open[0])}" data-dk="${esc(f.open[1])}"` : `data-go="${esc(f.go || 'overview')}"`;

// `compact`: the tray panel's short list, at most that many (true: 3).
// Each row says how long it has been so (`age`, or `since` as "for 3h").
export function findingsHtml(list, compact, empty = 'Nothing needs attention.') {
  if (!list.length) return `<div class="empty" style="padding:12px">${esc(empty)}</div>`;
  return list.slice(0, compact ? (compact === true ? 3 : compact) : 99).map(f => {
    const age = f.age || (f.since ? `for ${ago(f.since)}` : '');
    return `<div class="finding"><span class="ic" style="background:var(--${f.lvl})"></span><div class="t" role="button" tabindex="0" ${target(f)} title="${esc(f.t)}${compact && f.d ? '\n' + esc(f.d) : ''}"><div class="ft"><span class="ftt">${esc(f.t)}</span>${age ? `<span class="age">${esc(age)}</span>` : ''}</div>${compact ? '' : `<div class="muted">${esc(f.d)}</div>`}</div>${f.act ? `<button class="btn sm" data-run="${esc(f.act.run)}">${esc(f.act.label)}</button>` : ''}</div>`;
  }).join('');
}

// An error string as a sentence: capitalised and ending in a full stop.
const sentence = s => {
  s = String(s || '').trim();
  if (!s) return '';
  s = s[0].toUpperCase() + s.slice(1);
  return /[.!?]$/.test(s) ? s : s + '.';
};

// Problems with Mulchie itself, in words a user can act on. Each is
// [level, title, detail, go?].
export function notices() {
  const out = [], S = st.S, conn = st.conn;
  if (conn?.mode === 'disconnected') {
    out.push(['bad', 'Not connected to Mulchie’s engine',
      `${sentence(conn.detail) || 'No engine answers.'} Retrying every few seconds${S ? '; the figures shown are from before the connection was lost' : ''}. If it stays gone, this app starts its own engine.`]);
  }
  if (st.error && conn?.mode !== 'disconnected') {
    out.push(['bad', 'Could not read the engine’s state',
      `${sentence(st.error)} ${S ? `The figures shown are from ${S.snapshot.at_unix ? clock(S.snapshot.at_unix) : 'earlier'}.` : ''} Retrying every few seconds.`]);
  }
  if (!S) return out;
  if (S.config_error) {
    out.push(['bad', 'Your settings file is invalid, so Mulchie runs on safe defaults',
      `${S.config_error}. The file is left as it is and not overwritten; automatic cleanup and sweeping are off until it is fixed. Fix the file by hand, or save Settings here${conn?.mode === 'client' ? ' (saved through the running engine)' : ''}; the invalid file is kept beside the new one.`, 'settings']);
  }
  if (S.health === 'stale') {
    const age = S.sample_age_secs;
    out.push(['warn', 'Figures are out of date',
      age == null ? 'The engine has not taken its first sample yet.' : `The last sample is ${age} s old (normally every 3 s). The engine may be overloaded or stuck.`]);
  }
  const loops = S.loops || {};
  for (const [name, l] of Object.entries(loops)) {
    if (!l?.restarts) continue;
    out.push(['warn', `The ${name} loop restarted ${l.restarts} time(s)`,
      `Last failure ${l.last_panic_unix ? ago(l.last_panic_unix) + ' ago' : ''}: ${l.last_panic || 'unknown'}. Mulchie recovered on its own; if this keeps happening, report it with the Activity log.`, 'activity']);
  }
  if (S.automation === false && !S.config_error) {
    out.push(['info', 'Automatic actions are off',
      conn?.mode === 'client' ? 'The engine this app is connected to runs with automation off.' : 'Automatic cleanup, sweeping and build throttling are not running on this engine.']);
  }
  if (S.restore_failed) {
    out.push(['warn', `${S.restore_failed} build process(es) are still at lower priority`,
      'Mulchie lowered their priority under pressure and macOS would not let it raise it back without admin rights. They return to normal when they exit; restart them to restore it sooner.', 'activity']);
  }
  if (S.journal_error) {
    out.push(['bad', 'Mulchie cannot write its action log',
      `${sentence(S.journal_error)} Actions still run, but the Activity view and the restore steps miss them until this is fixed.`, 'activity']);
  }
  if (S.scan_stale) {
    out.push(['warn', 'The disk figures are old',
      `The last completed disk scan was ${S.scan_age_secs != null ? ago(Date.now() / 1000 - S.scan_age_secs) + ' ago' : 'a while ago'}; scans may be failing or taking very long. Rescan from the Disk view.`, 'disk']);
  }
  return out;
}

export function noticesHtml(list, compact) {
  return list.map(([lvl, t, d, go]) =>
    `<div class="notice ${lvl}" ${go ? `role="button" tabindex="0" data-go="${esc(go)}"` : ''}><span class="ic"></span><div class="t"><div><b>${esc(t)}</b></div>${compact ? '' : `<div>${esc(d)}</div>`}</div></div>`).join('');
}

// The connection badge: engine here, client of another engine, or offline.
export function modeHtml() {
  const c = st.conn;
  if (!c) return '';
  if (c.mode === 'engine') return `<span class="dot"></span>engine${c.port ? ` · :${c.port}` : ' · API off'}`;
  if (c.mode === 'client') return `<span class="dot busy"></span>client of pid ${c.engine_pid ?? '?'}${c.port ? ` · :${c.port}` : ''}`;
  return '<span class="dot off"></span>disconnected';
}
export const modeTitle = () => {
  const c = st.conn;
  if (!c) return '';
  if (c.mode === 'engine') return c.detail || 'This app runs Mulchie’s engine and serves the API.';
  if (c.mode === 'client') return 'Another Mulchie engine (for example `mulchie serve`) runs; this app shows it and acts through it.';
  return c.detail || 'Disconnected';
};
