// Findings ("needs attention") and status notices about Mulchie itself.
import { st } from './store.js';
import { esc, B, P, ago } from './util.js';

export function findings() {
  const S = st.S, out = [], s = S.snapshot, c = S.config;
  if (!s.disk_total || !c) return out;
  const free = s.disk_free / 1e9;
  if (free < c.low_disk_gb) out.push(['bad', `Disk nearly full: ${B(s.disk_free)} free`, 'Clear stale caches or sweep idle worktrees.', 'disk']);
  else if (S.low_disk) out.push(['warn', `Free space under ${c.alert_below_gb} GB`, `${B(s.disk_free)} free.`, 'disk']);
  if (S.forecast?.hours_to_low != null && S.forecast.hours_to_low < 24) out.push(['warn', `Disk filling at ${B(S.forecast.bytes_per_hour)}/h`, `Reaches the ${c.low_disk_gb} GB floor in about ${S.forecast.hours_to_low.toFixed(1)} h at this rate.`, 'disk']);
  if (s.cpu_count && s.load[0] > s.cpu_count * 2) {
    const top = [...s.workspaces].sort((a, b) => b.cpu_pct - a.cpu_pct).slice(0, 3).map(w => `${w.label} ${P(w.cpu_pct)}`).join(', ');
    out.push([s.load[0] > s.cpu_count * 6 ? 'bad' : 'warn', `Load ${s.load[0].toFixed(0)} on ${s.cpu_count} cores (${(s.load[0] / s.cpu_count).toFixed(1)}× oversubscribed)`, `Top: ${top}`, 'workspaces']);
  }
  const mp = s.mem_total ? s.mem_available / s.mem_total * 100 : 100;
  if (mp < c.throttle_mem_free_pct) out.push(['bad', `Memory pressure: ${mp.toFixed(0)}% available`, 'Builds are being deprioritised.', 'processes']);
  if (s.swap_used > 8e9) out.push(['warn', `${B(s.swap_used)} in swap`, 'Heavy swapping slows every agent. Sort processes by memory.', 'processes']);
  for (const w of s.workspaces.filter(w => w.orphan)) out.push(['warn', `Orphaned: ${w.label}`, `${w.orphan} · ${P(w.cpu_pct)} CPU, ${B(w.mem_bytes)}`, 'ws:' + w.path]);
  for (const w of s.workspaces.filter(w => w.paused)) out.push(['warn', `Paused: ${w.label}`, 'Its processes are stopped (SIGSTOP). Resume or close it out.', 'ws:' + w.path]);
  const n = S.diskmap?.worktrees?.filter(w => w.sweepable).length || 0;
  if (n) out.push(['warn', `${n} idle agent worktree(s), ${B(S.diskmap.sweepable_bytes)}`, `Idle ${c.worktree_idle_days}+ days. A sweep keeps branches and saves patches.`, 'disk']);
  const stale = S.scan?.stale_bytes || 0;
  if (stale > 1e9) out.push(['warn', `${B(stale)} of stale build caches`, 'Unused within the stale window, with no agent running in the project.', 'disk']);
  if (S.throttled) out.push(['ok', `${S.throttled} build process(es) at lower priority`, 'The optimizer reniced them under pressure.', 'activity']);
  if (S.slots?.waiting?.length) out.push(['ok', `${S.slots.waiting.length} job(s) queued for a build slot`, S.slots.waiting.map(w => w.label).join(', '), 'agents']);
  return out;
}

export function findingsHtml(list, compact) {
  if (!list.length) return '<div class="empty" style="padding:12px">Nothing needs attention.</div>';
  return list.slice(0, compact ? 5 : 99).map(([lvl, t, d, go]) =>
    `<div class="finding" data-go="${esc(go || '')}"><span class="ic" style="background:var(--${lvl})"></span><div class="t"><div>${esc(t)}</div>${compact ? '' : `<div class="muted">${esc(d)}</div>`}</div></div>`).join('');
}

// Problems with Mulchie itself, in words a user can act on. Each is
// [level, title, detail, go?].
export function notices() {
  const out = [], S = st.S, conn = st.conn;
  if (conn?.mode === 'disconnected') {
    out.push(['bad', 'Not connected to Mulchie’s engine',
      `${conn.detail || 'No engine answers.'} Retrying every few seconds${S ? '; the figures shown are from before the connection was lost' : ''}. If it stays gone, this app starts its own engine.`]);
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
    `<div class="notice ${lvl}" ${go ? `data-go="${esc(go)}"` : ''}><span class="ic"></span><div class="t"><div><b>${esc(t)}</b></div>${compact ? '' : `<div>${esc(d)}</div>`}</div></div>`).join('');
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
