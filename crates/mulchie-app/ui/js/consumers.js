// Who is using the machine, and what the numbers mean, as the engine sends
// them: `snapshot.consumers` (ranked groups: workspaces, apps, processes,
// the system) and `snapshot.cpu_summary` / `snapshot.mem_summary` (the
// headline in words). This module only shapes them for display.
import { B, esc } from './util.js';

// Every process the engine lists individually, once: the top list and each
// workspace's.
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

export const KIND_ICON = { workspace: '⌂', app: '◆', process: '●', system: '⚙' };
export const KIND_TEXT = { workspace: 'workspace (a project folder)', app: 'app', process: 'program (all its processes)', system: 'operating system' };

// One group, with the names the UI uses next to the engine's own fields.
const shape = g => ({
  ...g,
  label: g.name,
  short: g.kind === 'workspace' ? shortLabel(g.name) : g.name,
  path: g.workspace || g.bundle_path || null,
  share: (g.cpu_share_pct || 0) / 100,
  count: g.procs,
});

// The groups the engine ranked, by CPU and by memory, each once.
export function consumers(S) {
  const c = S.snapshot.consumers;
  if (!c) return [];
  const seen = new Map();
  for (const g of [...c.by_cpu, ...c.by_mem]) if (!seen.has(g.key)) seen.set(g.key, shape(g));
  return [...seen.values()];
}

export const ranked = (S, by) => {
  const c = S.snapshot.consumers;
  if (!c) return [];
  return (by === 'cpu_pct' ? c.by_cpu : c.by_mem).map(shape);
};
export const findConsumer = (S, key) => consumers(S).find(c => c.key === key);

// The engine's severity as the UI's three colours.
const LEVEL = { high: 'bad', medium: 'warn' };
const level = sev => LEVEL[sev] || 'ok';

// {level, word, headline, detail} from the engine's summary.
export function cpuSummary(S) {
  const c = S.snapshot.cpu_summary || {};
  const lv = level(c.severity);
  const word = c.waiting >= 1 ? 'backed up' : c.pct >= 90 ? 'maxed out' : c.pct >= 60 ? 'busy' : 'fine';
  return { level: lv, word, headline: c.headline || '', detail: c.detail || '', waiting: c.waiting || 0 };
}

export function memSummary(S) {
  const m = S.snapshot.mem_summary || {};
  const lv = level(m.severity);
  const word = m.pressure && m.pressure !== 'normal' ? `pressure ${m.pressure}` : lv === 'ok' ? 'fine' : lv === 'warn' ? 'tight' : 'very tight';
  return { level: lv, word, headline: m.headline || '', detail: m.detail || '' };
}

// The top `n` consumers of `by` ('cpu_pct' or 'mem_bytes') as bars; each
// opens that consumer's detail.
export function barsHtml(S, by, n = 5) {
  const list = ranked(S, by).filter(c => c[by] > (by === 'cpu_pct' ? 0.5 : 1e6)).slice(0, n);
  if (!list.length) return '<div class="dim">Nothing measurable.</div>';
  const max = Math.max(1e-9, ...list.map(c => c[by]));
  const cpu = by === 'cpu_pct';
  return list.map(c => `<div class="cb" role="button" tabindex="0" data-d="group" data-dk="${esc(c.key)}" title="${esc(c.label)} · ${esc(cores(c.cpu_pct))} (${(c.share * 100).toFixed(0)}% of the machine) · ${B(c.mem_bytes)} · ${c.count} process(es)">
    <span class="nm"><span class="ki ${esc(c.kind)}">${KIND_ICON[c.kind] || '●'}</span>${esc(c.short)}</span>
    <span class="v">${cpu ? esc(cores(c.cpu_pct)) : B(c.mem_bytes)}</span>
    <span class="bar"><i class="${cpu ? '' : 'mem'}" style="width:${(c[by] / max * 100).toFixed(1)}%"></i></span></div>`).join('');
}

// Tile values: "12 of 16 cores", "23 of 69 GB".
export function cpuTile(S) {
  const s = S.snapshot, c = s.cpu_summary || {}, n = c.cores || s.cpu_count || 1, sum = cpuSummary(S);
  const busy = Math.min(n, Math.round((c.pct ?? s.cpu_pct) / 100 * n));
  const waiting = Math.round(c.waiting || 0);
  return { v: `${busy} of ${n} cores`, s: sum.word + (waiting ? ` · ${waiting} waiting` : ` · load ${(c.load1 ?? s.load[0]).toFixed(1)}`), level: sum.level, title: `${sum.headline}\n${sum.detail}` };
}
export function memTile(S) {
  const s = S.snapshot, m = s.mem_summary || {}, sum = memSummary(S);
  const total = m.total || s.mem_total, used = m.used ?? (s.mem_total - s.mem_available), swap = m.swap_used ?? s.swap_used;
  return { v: `${B(used, 0)} of ${B(total, 0)}`, s: sum.word + (swap > 1e9 ? ` · ${B(swap, 0)} swap` : ''), level: sum.level, title: `${sum.headline}\n${sum.detail}` };
}
