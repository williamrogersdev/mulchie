// What is behind a status message: the sessions, apps, workspaces and
// processes using the most CPU, memory or swap now, each with the detail it
// opens in the inspector. Sessions come from `state.sessions` (every running
// one), the rest from `snapshot.consumers`; a memory finding carries the
// engine's own list (`evidence.culprits`), which also counts swap per session.
import { B, esc, kindGlyph } from './util.js';
import { cores, shortLabel } from './consumers.js';

const base = p => String(p || '').replace(/\/+$/, '').split('/').pop();

// A running session as a culprit.
export function sessionCulprit(r) {
  const u = r.usage || r; // a brief carries the usage fields at its top level
  return {
    kind: 'session', key: r.id,
    name: `${r.agent || 'agent'}${r.workspace ? ' in ' + shortLabel(base(r.workspace)) : ''}`,
    cpu_pct: (u.cpu_cores || 0) * 100, mem_bytes: (u.memory_mb || 0) * 1048576, swap_bytes: null,
    workspace: r.workspace || null, processes: u.processes || 0,
  };
}

// A consumer group as a culprit.
export const groupCulprit = g => ({
  kind: g.kind, key: g.key, name: g.kind === 'workspace' ? shortLabel(g.name) : g.name,
  cpu_pct: g.cpu_pct || 0, mem_bytes: g.mem_bytes || 0, swap_bytes: g.swap_bytes ?? null,
  workspace: g.workspace || null, processes: g.procs || 0,
});

export const runningSessions = S => (S.sessions?.list || []).filter(r => r.status === 'running');

const METRIC = { cpu: 'cpu_pct', mem: 'mem_bytes', swap: 'swap_bytes' };

// The biggest `n` by `what` ('cpu', 'mem' or 'swap'), sessions and groups
// together. A workspace group is left out when a listed session works in that
// workspace (it would count the same processes twice).
export function behind(S, what, n = 6) {
  const m = METRIC[what], c = S.snapshot.consumers || {};
  if (what === 'swap') {
    const fromEngine = (S.findings || []).find(f => f.kind === 'memory_low' && f.evidence?.culprits?.length);
    if (fromEngine) return fromEngine.evidence.culprits.filter(x => x.swap_bytes > 0).slice(0, n);
    return (c.by_swap || []).map(groupCulprit).slice(0, n);
  }
  const sessions = runningSessions(S).map(sessionCulprit).filter(x => x[m] > 0);
  const groups = [...(what === 'cpu' ? c.by_cpu || [] : c.by_mem || [])].map(groupCulprit);
  const sessionWs = new Set(sessions.map(s => s.workspace).filter(Boolean));
  const all = [...sessions, ...groups.filter(g => !(g.kind === 'workspace' && sessionWs.has(g.workspace)))];
  return all.sort((a, b) => b[m] - a[m]).slice(0, n);
}

// The detail a culprit opens: a session, or a group (app, workspace, process).
export const openAttrs = x => x.kind === 'session' ? `data-d="session" data-dk="${esc(x.key)}"` : `data-d="group" data-dk="${esc(x.key)}"`;

export const metricText = (x, what) => what === 'cpu' ? cores(x.cpu_pct) : what === 'swap' ? B(x.swap_bytes || 0) : B(x.mem_bytes);

// "Behind it:" and a chip per culprit.
export function whoHtml(list, what, label = 'Behind it:') {
  if (!list?.length) return '';
  return `<div class="who"><span class="wl">${esc(label)}</span>${list.map(x => `<span class="wc" role="button" tabindex="0" ${openAttrs(x)} title="${esc(x.name)} · ${esc(metricText(x, what))}">${kindGlyph(x.kind)}<span class="n">${esc(x.name)}</span><span class="v">${esc(metricText(x, what))}</span></span>`).join('')}</div>`;
}

// A ranked table for the inspector: kind, name, the measure, and the other.
export function rankHtml(list, what) {
  if (!list.length) return '<div class="dim">Nothing measurable.</div>';
  const other = what === 'mem' ? 'cpu' : 'mem';
  return `<table class="rank"><colgroup><col><col style="width:72px"><col style="width:64px"></colgroup>${list.map(x => `<tr role="button" tabindex="0" ${openAttrs(x)} class="k-${esc(x.kind)}"><td><span class="nm">${kindGlyph(x.kind)}<span title="${esc(x.name)}">${esc(x.name)}</span></span></td><td class="v">${esc(metricText(x, what))}</td><td class="v dim">${esc(metricText(x, other))}</td></tr>`).join('')}</table>`;
}
