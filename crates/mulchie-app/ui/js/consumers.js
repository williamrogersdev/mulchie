// Who is using the machine, and what the numbers mean.
//
// This adapter derives, from today's state, the shape the engine will soon
// send itself (`consumers`, `cpu_summary`, `mem_summary`): one ranked list per
// resource of workspaces and apps, CPU in cores and share of the machine, and
// a ready headline. When the engine sends those fields, `consumers()` and the
// two summaries return them instead; nothing else changes.
import { B, esc } from './util.js';

// Every process the engine reports, once: the top list and each workspace's.
export function allProcs(S) {
  const s = S.snapshot, seen = new Map();
  for (const p of [...s.top, ...s.workspaces.flatMap(w => w.procs)]) if (!seen.has(p.pid)) seen.set(p.pid, p);
  return [...seen.values()];
}

// "game-music-voice-lines › table-design-cleanup-3a89ab" → keeps the repo,
// drops the hash suffix agents add to worktree names, and shortens the repo
// only when the whole is still long. CSS ellipsis is the last resort.
export function shortLabel(label, max = 34) {
  const parts = String(label || '').split(' › ');
  if (parts.length < 2) return parts[0];
  let [repo, wt] = [parts[0], parts.slice(1).join(' › ').replace(/-[0-9a-f]{6,}$/, '')];
  if (repo.length + wt.length + 3 > max && repo.length > 14) repo = repo.slice(0, 13) + '…';
  return `${repo} › ${wt}`;
}

export const cores = pct => {
  const c = (Number(pct) || 0) / 100;
  return c >= 10 ? `${c.toFixed(0)} cores` : c >= 1 ? `${c.toFixed(1)} cores` : `${c.toFixed(1)} core`;
};

// [{key, label, short, kind: 'workspace'|'app', path, cpu_pct, cores, share,
//   mem_bytes, count, tracked, procs, main: {pid, start_time}}]
export function consumers(S) {
  if (S.consumers) return S.consumers;
  const s = S.snapshot, n = s.cpu_count || 1, procs = allProcs(S);
  const wsByLabel = new Map(s.workspaces.map(w => [w.label, w]));
  return s.groups.map(g => {
    const w = g.is_workspace ? wsByLabel.get(g.group) : null;
    const ps = (w ? w.procs : procs.filter(p => p.group === g.group)).slice().sort((a, b) => b.cpu_pct - a.cpu_pct);
    const main = ps[0];
    return {
      key: w ? 'ws:' + w.path : 'app:' + g.group,
      label: g.group,
      short: shortLabel(g.group),
      kind: w ? 'workspace' : 'app',
      path: w?.path || null,
      cpu_pct: g.cpu_pct,
      cores: g.cpu_pct / 100,
      share: g.cpu_pct / (n * 100),
      mem_bytes: g.mem_bytes,
      count: g.procs,
      tracked: ps.length,
      procs: ps,
      main: main ? { pid: main.pid, start_time: main.start_time } : null,
    };
  });
}

export const ranked = (S, by) => consumers(S).slice().sort((a, b) => b[by] - a[by]);
export const findConsumer = (S, key) => consumers(S).find(c => c.key === key);

// {level: ok|warn|bad, word, headline, detail}
export function cpuSummary(S) {
  if (S.cpu_summary) return S.cpu_summary;
  const s = S.snapshot, n = s.cpu_count || 1, load = s.load[0] || 0, pct = s.cpu_pct || 0;
  const waiting = Math.max(0, Math.round(load - n));
  const top = ranked(S, 'cpu_pct')[0];
  const busy = Math.round(pct / 100 * n);
  let level, word, headline;
  if (pct >= 90) {
    level = load > n * 4 ? 'bad' : 'warn'; word = 'maxed out';
    headline = `All ${n} cores are busy${waiting ? ` and about ${waiting} more tasks are waiting for one, so everything is slower` : ''}.`;
  } else if (waiting) {
    level = load > n * 3 ? 'bad' : 'warn'; word = 'backed up';
    headline = `${busy} of ${n} cores are busy, yet about ${waiting} more tasks are waiting (often on the disk), so things feel slower.`;
  } else if (pct >= 60) {
    level = 'ok'; word = 'busy';
    headline = `${busy} of ${n} cores are busy; there is still some headroom.`;
  } else {
    level = 'ok'; word = 'fine';
    headline = `${busy} of ${n} cores in use; plenty of headroom.`;
  }
  if (top && top.cpu_pct >= 50) headline += ` Most of it is ${top.short} (${cores(top.cpu_pct)}).`;
  const detail = `CPU ${pct.toFixed(0)}% of the machine. Load ${s.load.map(x => x.toFixed(1)).join(' · ')} (1, 5, 15 min): the number of tasks running or waiting for a core; above ${n} means tasks queue.`;
  return { level, word, headline, detail };
}

export function memSummary(S) {
  if (S.mem_summary) return S.mem_summary;
  const s = S.snapshot, c = S.config || {};
  const used = s.mem_total - s.mem_available, freePct = s.mem_total ? s.mem_available / s.mem_total * 100 : 100;
  const top = ranked(S, 'mem_bytes')[0];
  let level, word, headline;
  if (freePct < (c.throttle_mem_free_pct || 10)) {
    level = 'bad'; word = 'tight';
    headline = `Memory is tight: ${B(s.mem_available)} free${s.swap_used > 1e9 ? ` and ${B(s.swap_used)} swapped to disk` : ''}, so apps slow down.`;
  } else if (s.swap_used > 8e9 && freePct < 25) {
    level = 'warn'; word = 'swapping';
    headline = `Memory is getting tight: ${B(s.swap_used)} is swapped to disk.`;
  } else {
    level = 'ok'; word = 'fine';
    headline = `Memory is fine: ${B(used)} of ${B(s.mem_total, 0)} in use.`;
  }
  if (top && top.mem_bytes > 1e9) headline += ` The most is ${top.short} (${B(top.mem_bytes)}).`;
  const detail = `${B(s.mem_available)} available · ${B(s.swap_used)} of ${B(s.swap_total)} swap in use.`;
  return { level, word, headline, detail };
}

// The top `n` consumers of `by` ('cpu_pct' or 'mem_bytes') as bars; each
// opens that consumer's detail.
export function barsHtml(S, by, n = 5) {
  const list = ranked(S, by).filter(c => c[by] > (by === 'cpu_pct' ? 0.5 : 1e6)).slice(0, n);
  if (!list.length) return '<div class="dim">Nothing measurable.</div>';
  const max = Math.max(1e-9, ...list.map(c => c[by]));
  const cpu = by === 'cpu_pct';
  return list.map(c => `<div class="cb" role="button" tabindex="0" data-d="group" data-dk="${esc(c.key)}" title="${esc(c.label)} · ${esc(cores(c.cpu_pct))} (${(c.share * 100).toFixed(0)}% of the machine) · ${B(c.mem_bytes)} · ${c.count} process(es)">
    <span class="nm"><span class="ki ${c.kind}">${c.kind === 'workspace' ? '⌂' : '◆'}</span>${esc(c.short)}</span>
    <span class="v">${cpu ? esc(cores(c.cpu_pct)) : B(c.mem_bytes)}</span>
    <span class="bar"><i class="${cpu ? '' : 'mem'}" style="width:${(c[by] / max * 100).toFixed(1)}%"></i></span></div>`).join('');
}

// Tile values: "12 of 16 cores", "23 of 69 GB".
export function cpuTile(S) {
  const s = S.snapshot, n = s.cpu_count || 1, sum = cpuSummary(S), busy = Math.min(n, Math.round(s.cpu_pct / 100 * n));
  const waiting = Math.max(0, Math.round(s.load[0] - n));
  return { v: `${busy} of ${n} cores`, s: sum.word + (waiting ? ` · ${waiting} waiting` : ''), level: sum.level, title: sum.detail };
}
export function memTile(S) {
  const s = S.snapshot, sum = memSummary(S);
  return { v: `${B(s.mem_total - s.mem_available, 0)} of ${B(s.mem_total, 0)}`, s: sum.word + (s.swap_used > 1e9 ? ` · ${B(s.swap_used, 0)} swap` : ''), level: sum.level, title: sum.detail };
}
