// Findings ("needs attention") and status notices about Mulchie itself.
import { st } from './store.js';
import { esc, B, P, ago, clock } from './util.js';
import { cpuSummary, memSummary, ranked } from './consumers.js';

// What needs attention, each with where it leads (`open`: a detail, or `go`:
// a view) and one primary action (`act`: a runner name main.js knows).
export function findings() {
  const S = st.S, out = [], s = S.snapshot, c = S.config;
  if (!s.disk_total || !c) return out;
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
  for (const w of s.workspaces.filter(w => w.orphan)) out.push({ lvl: 'warn', t: `Orphaned: ${w.label}`, d: `${w.orphan} · ${P(w.cpu_pct)} CPU, ${B(w.mem_bytes)}`, open: ['ws', w.path], act: { label: 'Stop…', run: `stop:${w.path}` } });
  for (const w of s.workspaces.filter(w => w.paused)) out.push({ lvl: 'warn', t: `Paused: ${w.label}`, d: 'Its processes are stopped (SIGSTOP). Resume or close it out.', open: ['ws', w.path], act: { label: 'Show', run: `d:ws:${w.path}` } });
  if (n) out.push({ lvl: 'warn', t: `${n} idle agent worktree(s), ${B(S.diskmap.sweepable_bytes)}`, d: `Idle ${c.worktree_idle_days}+ days. A sweep keeps branches and saves patches.`, go: 'disk', act: { label: 'Sweep…', run: 'sweep' } });
  if (stale > 1e9) out.push({ lvl: 'warn', t: `${B(stale)} of stale build caches`, d: 'Unused within the stale window, with no agent running in the project.', go: 'disk', act: { label: 'Clean…', run: 'stale' } });
  if (S.throttled) out.push({ lvl: 'ok', t: `${S.throttled} build process(es) at lower priority`, d: 'The optimizer reniced them under pressure.', open: ['agents', ''], act: { label: 'Show', run: 'd:agents:' } });
  if (S.slots?.waiting?.length) out.push({ lvl: 'ok', t: `${S.slots.waiting.length} job(s) queued for a build slot`, d: S.slots.waiting.map(w => w.label).join(', '), open: ['agents', ''], act: { label: 'Show', run: 'd:agents:' } });
  return out;
}

// Where a finding leads when its text is clicked: a detail or a view.
const target = f => f.open ? `data-d="${esc(f.open[0])}" data-dk="${esc(f.open[1])}"` : `data-go="${esc(f.go || 'overview')}"`;

export function findingsHtml(list, compact) {
  if (!list.length) return '<div class="empty" style="padding:12px">Nothing needs attention.</div>';
  return list.slice(0, compact ? 5 : 99).map(f =>
    `<div class="finding"><span class="ic" style="background:var(--${f.lvl})"></span><div class="t" role="button" tabindex="0" ${target(f)}><div>${esc(f.t)}</div>${compact ? '' : `<div class="muted">${esc(f.d)}</div>`}</div>${f.act ? `<button class="btn sm" data-run="${esc(f.act.run)}">${esc(f.act.label)}</button>` : ''}</div>`).join('');
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
      `${S.config_error}. The file is left as it is and not overwritten; automatic cleanup and sweeping are off until it is fixed. ${conn?.mode === 'client' ? 'Fix the file by hand and restart the engine.' : 'Fix the file by hand and restart Mulchie, or save Settings here (the invalid file is kept beside the new one).'}`, 'settings']);
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
    out.push(['warn', 'Some builds are still at lower priority',
      'Mulchie lowered their priority under pressure and macOS would not let it raise it back without admin rights. They return to normal when they exit; restart them to restore it sooner.', 'activity']);
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
