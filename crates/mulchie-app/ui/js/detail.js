// The detail drawer: every table row, finding, tile and chart legend opens
// one. It slides over the right of the view (tables never reflow), closes on
// Esc or ×, and re-renders on each poll without losing focus, scroll or the
// expanded rows. Every value that reaches innerHTML goes through esc().
import { st, hooks } from './store.js';
import { invoke } from './ipc.js';
import { $, esc, B, BS, P, ago, dur, tilde, toast, kindChip } from './util.js';
import { planThenExecute, confirmBox, reasonText } from './dialogs.js';
import { GROUP_ACTIONS, groupAction, signalPids, reniceProc, planAndRun, canLower } from './signals.js';
import { allProcs, findConsumer, ranked, cores, cpuSummary, memSummary, KIND_TEXT } from './consumers.js';
import { SEV_LVL, SEV_TEXT, suggestionLabel, watchFindings } from './findings.js';
import { act } from './act.js';

// ------------------------------------------------------------------ helpers

const kv = rows => `<dl class="kv">${rows.filter(Boolean).map(([k, v]) => `<dt>${esc(k)}</dt><dd>${v}</dd>`).join('')}</dl>`;
const sec = (title, html, extra = '') => `<section class="ds"><h3>${esc(title)}${extra}</h3>${html}</section>`;
const btn = (label, attrs, cls = '') => `<button class="btn ${cls}" ${attrs}>${esc(label)}</button>`;
const da = (name, arg = '') => `data-da="${esc(name)}" data-arg="${esc(arg)}"`;
const dl = (type, key) => `data-d="${esc(type)}" data-dk="${esc(key)}"`;
const link = (type, key, text) => `<a class="lnk" role="button" tabindex="0" ${dl(type, key)}>${esc(text)}</a>`;
const actions = list => `<div class="actions">${list.filter(Boolean).join('')}</div>`;
const S = () => st.S;
const wsAt = path => S().snapshot.workspaces.find(w => w.path === path);
const wtAt = path => S().diskmap?.worktrees?.find(w => w.path === path);
const procBy = pid => allProcs(S()).find(p => p.pid === pid);
const short = h => (h || '').slice(0, 10);
const when = t => t ? `${esc(ago(t))} ago` : '—';

function procRows(procs, max = 40) {
  if (!procs.length) return '<div class="dim">None.</div>';
  return `<table class="mini-t"><colgroup><col><col style="width:78px"><col style="width:70px"></colgroup>${procs.slice(0, max).map(p => `<tr class="click" role="button" tabindex="0" ${dl('proc', p.pid)}>
    <td><span class="cell">${esc(p.name)} <span class="dim mono">${p.pid}</span> ${p.kind !== 'other' ? kindChip(p.kind) : ''}${p.stopped ? ' <span class="chip warn">paused</span>' : ''}</span></td>
    <td class="n">${esc(cores(p.cpu_pct))}</td><td class="n">${B(p.mem_bytes)}</td></tr>`).join('')}</table>${procs.length > max ? `<div class="dim">and ${procs.length - max} more</div>` : ''}`;
}

const pick = (o, keys) => Object.fromEntries(keys.filter(k => o[k] !== undefined).map(k => [k, o[k]]));
const procCopy = p => pick(p, ['pid', 'name', 'kind', 'cpu_pct', 'mem_bytes', 'nice', 'stopped', 'run_secs', 'start_time', 'workspace', 'cmd']);

// The actions a group's plan can be asked for, as buttons. `g` is a
// consumer group from the engine (`actions` and `protected` come from it);
// a workspace outside the ranked lists gets the workspace actions, and its
// plan says what applies. Every button plans first.
const WS_ACTIONS = ['quit', 'force_quit', 'pause', 'resume', 'lower_priority', 'stop_builds'];
const DANGER = new Set(['quit', 'force_quit', 'stop_builds', 'restart']);
function groupButtons(g, { path = null } = {}) {
  if (g.protected) return `<p class="why-s warn">Mulchie will not act on this: ${esc(g.protected)}.</p>${path ? actions([btn('Reveal in Finder', `data-reveal="${esc(path)}"`)]) : ''}`;
  const list = g.actions || WS_ACTIONS;
  const arg = encodeURIComponent(JSON.stringify({ key: g.key, name: g.label || g.name }));
  const hasBuilds = g.builds == null || g.builds > 0;
  return actions([
    ...Object.keys(GROUP_ACTIONS).filter(a => list.includes(a) && (a !== 'stop_builds' || hasBuilds))
      .map(a => btn(GROUP_ACTIONS[a].verb + '…', da('g-' + a, arg), DANGER.has(a) ? 'danger' : '')),
    g.kind === 'app' && !list.includes('restart') ? '<button class="btn" disabled title="The engine offers a restart only for an app with a bundle id, on macOS">Restart</button>' : '',
    path ? btn('Reveal in Finder', `data-reveal="${esc(path)}"`) : '',
  ]);
}

// A workspace as a group (its key is `workspace:<path>`).
const wsGroup = w => findConsumer(S(), 'workspace:' + w.path) || { key: 'workspace:' + w.path, kind: 'workspace', label: w.label, builds: w.builds };

// Members the engine listed for a group (its biggest three), as rows.
const memberRows = g => procRows(g.top || []) + (g.count > (g.top || []).length ? `<div class="dim">${g.count - g.top.length} more; the plan lists every process an action affects.</div>` : '');

// ------------------------------------------------------------- renderers

function explainWorktree(w) {
  const N = S().config?.worktree_idle_days ?? 7, dirty = w.dirty_files ? ` Its ${w.dirty_files} uncommitted file(s) are saved as a patch first.` : '';
  if (w.missing) return 'The folder is gone, but git still lists it. Removing it only prunes git’s record.';
  if (w.in_use) return 'Something is running in it (an agent, a build or a shell), so Mulchie will not remove it. Stop those processes first.';
  if (w.locked) return 'It is locked in git (git worktree lock), so Mulchie leaves it alone.';
  if (w.sweepable) return `An agent worktree idle ${w.idle_days} days (the sweep threshold is ${N}) with nothing running in it, so a sweep removes it. The branch is kept.${dirty}`;
  if (w.removable) {
    const why = w.agent ? `It is not swept yet because it was active ${w.idle_days} day(s) ago (threshold ${N}).` : 'It is not an agent worktree, so sweeps skip it.';
    return `Nothing runs in it, so you can remove it; the branch is kept.${dirty} ${why}`;
  }
  return `Mulchie leaves it: ${w.verdict}.`;
}

function rWorktree(path) {
  const w = wtAt(path);
  if (!w) return gone('Worktree', path);
  const ws = wsAt(path), procs = ws?.procs || [];
  const backups = st.actions.filter(a => a.target === path && a.backups?.length);
  const flags = [w.agent && '<span class="chip agent">agent</span>', w.merged && '<span class="chip ok">merged</span>', w.locked && '<span class="chip warn">locked</span>', w.missing && '<span class="chip bad">missing</span>', w.in_use && '<span class="chip ok">in use</span>'].filter(Boolean).join(' ');
  const html = `<p class="why-s ${w.sweepable ? 'warn' : ''}">${esc(explainWorktree(w))}</p>
    ${kv([['path', esc(tilde(w.path))], ['repository', esc(tilde(w.repo))], ['branch', esc(w.detached ? '(detached HEAD)' : w.branch || '—')], ['HEAD', `<span class="mono">${esc(short(w.head))}</span>`],
      ['size', B(w.bytes)], ['last activity', when(w.last_activity_unix)], ['last commit', when(w.last_commit_unix)], ['idle', `${w.idle_days} day(s)`], ['uncommitted', `${w.dirty_files} file(s)`], flags && ['flags', flags]])}
    ${actions([btn('Reveal in Finder', `data-reveal="${esc(w.path)}"` + (w.missing ? ' disabled' : '')), btn('Open in Terminal', `data-term="${esc(w.path)}"` + (w.missing ? ' disabled' : '')),
      btn('Stop processes here…', da('stop-here', w.path) + (procs.length ? '' : ' disabled title="Nothing runs here"'), 'danger'),
      btn('Remove…', da('remove-wt', w.path) + (w.in_use || w.locked ? ' disabled' : ''), 'danger'), ws ? btn('Workspace →', dl('ws', ws.path)) : ''])}
    ${sec(`Running here (${procs.length})`, procRows(procs))}
    ${sec('Backups', backups.length ? backups.map(a => `<div class="bkrow">${esc(new Date(a.at_unix * 1000).toLocaleString())} · ${(a.backups || []).map(b => `${esc(b.kind)} <button class="btn sm" data-reveal="${esc(b.location)}">Show</button>`).join(' ')}</div>`).join('') : '<div class="dim">None for this worktree.</div>')}`;
  return { title: `${w.repo_name} › ${w.name}`, sub: w.sweepable ? '<span class="chip warn">sweepable</span>' : '', html,
    copy: { kind: 'worktree', ...pick(w, ['path', 'repo', 'branch', 'head', 'detached', 'agent', 'bytes', 'idle_days', 'dirty_files', 'merged', 'locked', 'missing', 'in_use', 'removable', 'sweepable', 'verdict']), explanation: explainWorktree(w), processes: procs.map(procCopy), hint: 'Remove with the MCP tool remove_worktrees or POST /v1/worktree/plan then /v1/worktree/remove.' } };
}

function rWorkspace(path) {
  const w = wsAt(path);
  if (!w) return gone('Workspace', path, 'No processes run here any more.');
  const procs = w.procs.slice().sort((a, b) => b.cpu_pct - a.cpu_pct);
  const caches = (S().scan?.items || []).filter(i => i.path.startsWith(w.path + '/'));
  const wt = wtAt(w.path);
  const state = [w.agents && `${w.agents} agent(s)`, w.builds && `${w.builds} build(s)`, w.paused && 'paused', w.throttled && `${w.throttled} at lower priority`, w.orphan && `orphan: ${w.orphan}`, !w.exists && 'folder deleted'].filter(Boolean).join(' · ') || 'running';
  const html = `${kv([['path', esc(tilde(w.path))], ['branch', esc(w.branch || '—')], ['state', esc(state)], ['cpu', `${esc(cores(w.cpu_pct))} <span class="dim">(${P(w.cpu_pct / (S().snapshot.cpu_count || 1))} of the machine)</span>`], ['memory', B(w.mem_bytes)], ['disk i/o', `${BS(w.read_bps)} r · ${BS(w.write_bps)} w`], wt && ['on disk', `${B(wt.bytes)} · ${wt.dirty_files} uncommitted`]])}
    ${groupButtons(wsGroup(w), { path: w.exists ? w.path : null })}
    <div class="actions">${btn('Open in Terminal', `data-term="${esc(w.path)}"` + (w.exists ? '' : ' disabled'))}${caches.length ? btn(`Clean caches (${B(caches.reduce((a, i) => a + i.bytes, 0))})…`, da('clean-caches', w.path)) : ''}${wt ? btn('Worktree →', dl('wt', wt.path)) : ''}</div>
    ${sec(`Processes (${procs.length})`, procRows(procs))}
    ${sec(`Build caches (${caches.length})`, caches.length ? `<table class="mini-t"><colgroup><col><col style="width:80px"><col style="width:80px"></colgroup>${caches.map(i => `<tr class="click" role="button" tabindex="0" ${dl('cache', i.path)}><td><span class="cell" title="${esc(i.path)}">${esc(tilde(i.path).replace(tilde(w.path), '…'))}</span></td><td class="n">${B(i.bytes)}</td><td class="n ${i.in_use ? 'ok' : i.stale ? 'warn' : 'dim'}">${i.in_use ? 'in use' : i.stale ? 'stale' : 'recent'}</td></tr>`).join('')}</table>` : '<div class="dim">None found under this folder.</div>')}`;
  return { title: w.label, sub: w.paused ? '<span class="chip warn">paused</span>' : '', html,
    copy: { kind: 'workspace', ...pick(w, ['path', 'label', 'branch', 'agents', 'builds', 'paused', 'throttled', 'orphan', 'cpu_pct', 'mem_bytes']), processes: procs.map(procCopy), caches: caches.map(i => pick(i, ['path', 'kind', 'bytes', 'stale', 'in_use'])) } };
}

// The engine's full record of one process (`GET /v1/process/<pid>`), read
// when the drawer opens and again every few seconds while it stays open.
const pd = { pid: null, data: undefined, err: null, at: 0, busy: false };
export function loadProc(pid, force) {
  pid = +pid;
  if (pd.pid !== pid) Object.assign(pd, { pid, data: undefined, err: null, at: 0 });
  if (pd.busy || (!force && Date.now() - pd.at < 2500)) return;
  pd.busy = true;
  invoke('process_detail', { pid })
    // A reply for a process the drawer no longer shows is dropped.
    .then(d => { if (pd.pid === pid) { pd.data = d; pd.err = null; } }, e => { if (pd.pid === pid) pd.err = String(e); })
    .finally(() => {
      pd.busy = false;
      if (pd.pid === pid) pd.at = Date.now();
      if (st.ui.detail?.type === 'proc') renderDetail();
    });
}

const SIG = { apple: 'Apple', app_store: 'App Store', developer_id: 'Developer ID', development: 'a developer’s own build', other_signed: 'signed by another certificate', ad_hoc: 'ad-hoc (no identity)', unsigned: 'not signed', unknown: 'could not be checked' };
const procName = (pid, name) => link('proc', pid, `${name} ${pid}`);

function rProc(pid) {
  pid = +pid;
  loadProc(pid);
  const p = procBy(pid), d = pd.pid === pid ? pd.data : undefined;
  if (d === null || (!p && !d && pd.at)) return gone(`Process ${pid}`, '', pd.err ? `Could not read it: ${pd.err}` : 'It has exited.');
  if (!p && !d) return { title: `Process ${pid}`, html: '<p class="dim">Reading…</p>', copy: { kind: 'process', pid } };
  // The engine's record wins; the tracked row adds I/O and nice.
  const x = { ...(p || {}), ...(d || {}) }, own = d ? d.own : p.own;
  const cmd = Array.isArray(d?.cmd) ? d.cmd.join(' ') : p?.cmd || '';
  const started = x.start_time ? new Date(x.start_time * 1000).toLocaleString() : '—';
  const age = d?.age_secs ?? p?.run_secs;
  const fs = d?.findings || [];
  const html = `<div>${x.kind ? kindChip(x.kind) : ''} ${own ? '' : '<span class="chip">another user</span>'} ${x.stopped ? '<span class="chip warn">paused</span>' : ''} ${p?.nice > 0 ? `<span class="chip warn">nice ${p.nice}</span>` : ''}</div>
    ${d?.protected ? `<p class="why-s warn">Mulchie will not signal it: ${esc(d.protected)}.</p>` : ''}
    ${fs.length ? sec(`Observations (${fs.length})`, fs.map(f => `<div class="finding" role="button" tabindex="0" ${dl('finding', f.id)}><span class="ic" style="background:var(--${SEV_LVL[f.severity] || 'info'})"></span><div class="t"><div>${esc(f.title)}</div></div></div>`).join('')) : ''}
    ${kv([['cpu', x.cpu_pct != null ? `${esc(cores(x.cpu_pct))} <span class="dim">(${P(x.cpu_pct, 1)} of one core)</span>` : '—'], ['memory', x.mem_bytes != null ? B(x.mem_bytes) : '—'], p && ['disk i/o', `${BS(p.read_bps)} r · ${BS(p.write_bps)} w`], p && ['nice', String(p.nice)],
      ['age', `${age != null ? esc(dur(age)) : '—'} <span class="dim">(started ${esc(started)})</span>`],
      ['working dir', d?.cwd ? `<span class="mono">${esc(tilde(d.cwd))}</span>` : `<span class="dim">${d ? 'not readable' : '…'}</span>`],
      ['program', d?.exe ? `<span class="mono">${esc(tilde(d.exe))}</span>` : '<span class="dim">—</span>'],
      d?.signature && ['signature', esc(SIG[d.signature] || d.signature) + (d.quarantined ? ' · <span class="warn">downloaded (quarantine flag)</span>' : '')],
      ['listening', d?.listening == null ? '<span class="dim">unknown</span>' : d.listening.length ? d.listening.map(l => `<span class="mono">${esc(l)}</span>`).join(', ') : '<span class="dim">no ports</span>'],
      ['parents', d?.parents?.length ? d.parents.slice(0, 6).map(q => procName(q.pid, q.name)).join(' ← ') : esc(String(x.ppid ?? '—'))],
      ['children', d?.children?.length ? d.children.slice(0, 12).map(k => procName(k.pid, k.name)).join(', ') + (d.children.length > 12 ? ` <span class="dim">+${d.children.length - 12}</span>` : '') : '—'],
      ['workspace', x.workspace ? link('ws', x.workspace, d?.workspace_label || tilde(x.workspace)) : '<span class="dim">none (its working directory is outside your projects)</span>'],
      d?.group && findConsumer(S(), d.group) && ['group', link('group', d.group, findConsumer(S(), d.group).label)]])}
    <h3 class="ds-h">Command line</h3><pre class="code">${esc(cmd) || '<span class="dim">not readable</span>'}</pre>
    ${own && !d?.protected ? actions([btn('Quit…', da('p-term', pid), 'danger'), btn('Force quit…', da('p-kill', pid), 'danger'), x.stopped ? btn('Resume…', da('p-cont', pid)) : btn('Pause…', da('p-stop', pid)), canLower(p) ? btn('Lower priority…', da('p-renice', pid)) : '', d?.children?.length ? btn('Quit with children…', da('p-tree', pid), 'danger') : ''])
      : own ? '' : '<div class="muted" style="margin-top:10px">Owned by another user; Mulchie will not signal it.</div>'}`;
  return { title: `${x.name}`, sub: `<span class="dim mono">pid ${pid}</span>`, html,
    copy: { kind: 'process', ...pick(x, ['pid', 'ppid', 'name', 'kind', 'cpu_pct', 'mem_bytes', 'start_time', 'workspace', 'cwd', 'exe', 'signature', 'listening', 'protected']), cmd, parents: d?.parents, children: d?.children, findings: fs.map(f => f.id) } };
}

function rGroup(key) {
  const c = findConsumer(S(), key);
  if (!c) {
    const ws = key.startsWith('workspace:') && wsAt(key.slice(10));
    if (ws) return rWorkspace(ws.path);
    return gone('Group', key.replace(/^\w+:/, ''), 'It is not among the biggest users of CPU or memory right now.');
  }
  const html = `${kv([['kind', esc(KIND_TEXT[c.kind] || c.kind)], ['cpu', `${esc(cores(c.cpu_pct))} <span class="dim">(${P(c.share * 100)} of the machine)</span>`], ['memory', `${B(c.mem_bytes)} <span class="dim">(${P(c.mem_share_pct)})</span>`],
    ['processes', `${c.count}${c.agents ? ` · ${c.agents} agent(s)` : ''}${c.builds ? ` · ${c.builds} build(s)` : ''}`], ['main process', procName(c.main_pid, c.top?.find(t => t.pid === c.main_pid)?.name || c.label)],
    c.bundle_id && ['bundle id', `<span class="mono">${esc(c.bundle_id)}</span>`], c.path && ['path', esc(tilde(c.path))]])}
    <p class="dim">${esc(S().snapshot.consumers?.memory_note || '')}</p>
    ${groupButtons(c, { path: c.path })}
    ${c.kind === 'workspace' ? `<div class="actions">${btn('Workspace details →', dl('ws', c.path))}</div>` : ''}
    ${sec(`Biggest processes (${(c.top || []).length} of ${c.count})`, memberRows(c))}`;
  return { title: c.label, sub: `<span class="chip">${esc(c.kind)}</span>`, html, copy: { kind: 'group', ...pick(c, ['key', 'label', 'kind', 'path', 'bundle_id', 'cpu_pct', 'cpu_cores', 'cpu_share_pct', 'mem_bytes', 'count', 'main_pid', 'main_start_time', 'actions', 'protected']), top: c.top } };
}

const fmtEvidence = e => [
  e.pid != null && ['process', procName(e.pid, e.name || 'pid')],
  e.start_time && ['started', esc(new Date(e.start_time * 1000).toLocaleString())],
  e.age_secs != null && ['age', esc(dur(e.age_secs))],
  e.exe && ['program', `<span class="mono">${esc(tilde(e.exe))}</span>`],
  e.cwd && ['working dir', `<span class="mono">${esc(tilde(e.cwd))}</span>`],
  e.workspace && ['workspace', link('ws', e.workspace, tilde(e.workspace))],
  e.parent && ['parent', procName(e.parent.pid, e.parent.name)],
  e.listen?.length && ['listening', e.listen.map(l => `<span class="mono">${esc(l)}</span>`).join(', ')],
  e.signature && ['signature', esc(SIG[e.signature] || e.signature)],
  e.quarantined != null && ['quarantine flag', e.quarantined ? 'yes (downloaded)' : 'no'],
  e.cpu_pct != null && ['cpu', esc(cores(e.cpu_pct))],
  e.mem_bytes != null && ['memory', B(e.mem_bytes)],
  e.path && ['path', `<span class="mono">${esc(tilde(e.path))}</span>`],
  e.mode && ['mode', `<span class="mono">${esc(e.mode)}</span>`],
  e.detail && ['detail', esc(e.detail)],
];

const findingBy = id => (S().findings || []).find(f => f.id === id);
const until = t => new Date(t * 1000).toLocaleDateString([], { month: 'short', day: 'numeric', year: 'numeric' });

function rFinding(id) {
  const f = findingBy(id);
  if (!f) return gone('Observation', '', 'The engine no longer reports it: the cause went away, or the watch has not run since.');
  const e = f.evidence || {};
  const sug = f.suggested.map((sg, i) => sg.request ? btn(sg.label + '…', da('fx', `${i}:${f.id}`), sg.reversible ? '' : 'danger') : '').filter(Boolean);
  const html = `<p class="why-s ${SEV_LVL[f.severity] === 'bad' ? 'warn' : ''}">${esc(f.why)}</p>
    ${kv(fmtEvidence(e))}
    ${e.cmd ? `<h3 class="ds-h">Command line</h3><pre class="code">${esc(e.cmd)}</pre>` : ''}
    ${kv([['first seen', when(f.first_seen_unix)], ['last seen', when(f.last_seen_unix)], f.dismissed && ['dismissed', f.dismissed_until ? `until ${esc(until(f.dismissed_until))}` : 'indefinitely']])}
    ${sug.length ? sec('What you can do', `<div class="actions">${sug.join('')}</div><div class="dim">Each action is planned first and shows the processes it affects before anything is sent.</div>`) : ''}
    ${sec(f.dismissed ? 'Dismissed' : 'Dismiss', f.dismissed
      ? `<div class="actions">${btn('Show again', da('undismiss', f.id))}</div>`
      : `<div class="actions"><select id="dDays" aria-label="Dismiss for">${[[1, '1 day'], [7, '7 days'], [30, '30 days'], [365, '1 year']].map(([d, l]) => `<option value="${d}" ${d === 30 ? 'selected' : ''}>${l}</option>`).join('')}</select>${btn('Dismiss', da('dismiss', f.id))}</div><div class="dim">Hidden until the date shown, then reported again if it still holds. Saved in the settings file.</div>`)}`;
  return { title: f.title, sub: `<span class="chip ${SEV_LVL[f.severity]}">${esc(SEV_TEXT[f.severity] || f.severity)}</span> <span class="dim mono">${esc(f.kind.replace(/_/g, ' '))}</span>`, html,
    copy: { kind: 'finding', ...pick(f, ['id', 'kind', 'severity', 'title', 'why', 'evidence', 'dismissed', 'dismissed_until']), suggested: f.suggested.map(sg => sg.request ? { label: sg.label, request: sg.request } : { label: sg.label, api: sg.api }) } };
}

function findingList(list) {
  if (!list.length) return '<div class="dim">None.</div>';
  return list.map(f => `<div class="finding" role="button" tabindex="0" ${dl('finding', f.id)}><span class="ic" style="background:var(--${SEV_LVL[f.severity] || 'info'})"></span><div class="t"><div>${esc(f.title)}</div><div class="muted">${esc(SEV_TEXT[f.severity])}${f.dismissed && f.dismissed_until ? ` · dismissed until ${esc(until(f.dismissed_until))}` : ''}</div></div></div>`).join('');
}

function rFindings() {
  const open = watchFindings(S()), dis = watchFindings(S(), true), c = S().finding_counts || {};
  const off = S().config?.watch && S().config.watch.enabled === false;
  const html = `<p class="dim">Things the engine noticed about running programs and Mulchie’s own setup. They are observations to check, not verdicts; nothing is acted on unless you choose an action.${S().findings_unix ? ` Last checked ${esc(ago(S().findings_unix))} ago.` : ''}</p>
    ${off ? '<p class="why-s warn">The watch is off in Settings, so nothing new is reported.</p>' : ''}
    ${kv([['high', String(c.high || 0)], ['medium', String(c.medium || 0)], ['low', String(c.low || 0)], ['info', String(c.info || 0)]])}
    ${sec(`Open (${open.length})`, findingList(open))}
    ${sec(`Dismissed (${dis.length})`, findingList(dis))}`;
  return { title: 'Observations', html, copy: { kind: 'findings', counts: c, open: open.map(f => pick(f, ['id', 'severity', 'title'])), dismissed: dis.map(f => pick(f, ['id', 'title', 'dismissed_until'])) } };
}

// CPU or memory: the ranked consumers, each expandable to its processes with
// the group actions.
function rResource(res) {
  const cpu = res === 'cpu', s = S().snapshot, sum = cpu ? cpuSummary(S()) : memSummary(S());
  const list = ranked(S(), cpu ? 'cpu_pct' : 'mem_bytes').slice(0, 15);
  const max = Math.max(1e-9, ...list.map(c => cpu ? c.cpu_pct : c.mem_bytes));
  const rows = list.map(c => `<details class="cons" data-key="${esc(c.key)}"><summary>
      <span class="bar"><i style="width:${(cpu ? c.cpu_pct : c.mem_bytes) / max * 100}%"></i></span>
      <span class="nm" title="${esc(c.label)}">${esc(c.short)}</span><span class="v">${cpu ? esc(cores(c.cpu_pct)) : B(c.mem_bytes)}</span></summary>
      <div class="cons-b">${memberRows(c)}${groupButtons(c, { path: c.path })}<div class="actions">${btn('Details →', dl('group', c.key))}</div></div></details>`).join('');
  const html = `<p class="why-s ${sum.level}">${esc(sum.headline)}</p><p class="dim">${esc(sum.detail)}</p>
    ${sec(cpu ? 'Using the CPU' : 'Using memory', rows || '<div class="dim">Nothing measurable.</div>')}`;
  return { title: cpu ? 'CPU' : 'Memory', sub: `<span class="chip ${sum.level === 'ok' ? 'ok' : sum.level}">${esc(sum.word)}</span>`, html,
    copy: { kind: cpu ? 'cpu' : 'memory', summary: cpu ? s.cpu_summary : s.mem_summary,
      top: list.slice(0, 8).map(c => ({ ...pick(c, ['key', 'label', 'kind', 'path', 'cpu_pct', 'mem_bytes', 'count', 'main_pid']), top_pids: (c.top || []).map(p => p.pid) })) } };
}

function rAgents() {
  const sl = S().slots, s = S().snapshot;
  const agents = allProcs(S()).filter(p => p.kind === 'agent').sort((a, b) => b.cpu_pct - a.cpu_pct);
  const builds = allProcs(S()).filter(p => p.kind === 'build').sort((a, b) => b.cpu_pct - a.cpu_pct);
  const lease = l => `<div class="bkrow"><b>${esc(l.label || '?')}</b> <span class="dim">${l.pid ? 'pid ' + esc(l.pid) : ''} ${l.jobs ? `· ${esc(l.jobs)} jobs` : ''}</span></div>`;
  const html = `${kv([['agents', String(s.agent_count)], ['builds', String(s.build_count)], ['build slots', `${sl.active.length} of ${sl.capacity} in use`], ['queued', String(sl.waiting.length)]])}
    ${sec(`Build queue (${sl.active.length} running, ${sl.waiting.length} waiting)`, (sl.active.map(lease).join('') + sl.waiting.map(lease).join('')) || '<div class="dim">No builds hold a slot. Agents take one with `mulchie run` or the MCP tool.</div>')}
    ${sec(`Agent processes (${agents.length})`, procRows(agents))}
    ${sec(`Build processes (${builds.length})`, procRows(builds))}`;
  return { title: 'Agents and builds', html, copy: { kind: 'agents', slots: sl, agents: agents.map(procCopy), builds: builds.map(procCopy) } };
}

const KN = { cargo_target: 'Rust target', node_modules: 'node_modules', next_cache: 'Next.js .next', xcode_derived_data: 'Xcode DerivedData' };

function rCache(path) {
  const i = (S().scan?.items || []).find(x => x.path === path);
  if (!i) return gone('Build cache', path);
  const why = i.in_use ? 'A process is running in this project, so the cache is in use and is left alone.' : i.stale ? `Unused for longer than the stale window (${S().config?.stale_hours ?? '?'} h) with nothing running in the project, so clearing it is safe; the next build recreates it.` : 'Used recently; clearing it now would only cost the next build time.';
  const ws = wsAt(i.project);
  const html = `<p class="why-s ${i.stale ? 'warn' : ''}">${esc(why)}</p>
    ${kv([['path', esc(tilde(i.path))], ['project', esc(tilde(i.project))], ['kind', esc(KN[i.kind] || i.kind)], ['size', B(i.bytes)], ['last used', when(i.last_used_unix)], ['in a worktree', i.in_worktree ? 'yes' : 'no']])}
    ${actions([btn('Clear…', da('clean-one', i.path) + (i.in_use ? ' disabled' : ''), 'danger'), btn('Reveal in Finder', `data-reveal="${esc(i.path)}"`), ws ? btn('Workspace →', dl('ws', ws.path)) : '', wtAt(i.project) ? btn('Worktree →', dl('wt', i.project)) : ''])}`;
  return { title: `${KN[i.kind] || i.kind} · ${tilde(i.project).split('/').pop()}`, html, copy: { kind: 'build_cache', ...pick(i, ['path', 'project', 'kind', 'bytes', 'last_used_unix', 'stale', 'in_use']), explanation: why } };
}

function rCategory(key) {
  const c = S().diskmap?.categories?.find(x => x.key === key);
  if (!c) return gone('Store', key);
  const total = S().snapshot.disk_total;
  const html = `${kv([['path', esc(tilde(c.path))], ['size', `${B(c.bytes)} <span class="dim">(${P(c.bytes / total * 100, 1)} of the disk)</span>`], ['what is in it', esc(c.hint)], ['prune', c.prunable ? `<span class="mono">${esc(c.hint)}</span>` : '<span class="dim">no safe command; reclaim by hand</span>']])}
    <p class="dim">The engine reports this store’s total only; a breakdown of its biggest items is not available yet.</p>
    ${actions([c.prunable ? btn('Prune…', da('prune', c.key), 'danger') : '', btn('Reveal in Finder', `data-reveal="${esc(c.path)}"`)])}`;
  return { title: c.name, html, copy: { kind: 'disk_store', ...pick(c, ['key', 'name', 'path', 'bytes', 'hint', 'prunable']) } };
}

// A segment of the volume bar.
function rSegment(seg) {
  const m = S().diskmap, s = S().snapshot;
  if (seg.startsWith('cat:')) return rCategory(seg.slice(4));
  if (seg === 'worktrees') {
    const list = (m?.worktrees || []).slice().sort((a, b) => b.bytes - a.bytes).slice(0, 12);
    return { title: 'Worktrees', html: `${kv([['total', B(m?.worktree_bytes || 0)], ['sweepable', B(m?.sweepable_bytes || 0)], ['count', String(m?.worktrees?.length || 0)]])}
      ${sec('Biggest', `<table class="mini-t"><colgroup><col><col style="width:80px"></colgroup>${list.map(w => `<tr class="click" role="button" tabindex="0" ${dl('wt', w.path)}><td><span class="cell">${esc(w.repo_name)} › ${esc(w.name)}</span></td><td class="n">${B(w.bytes)}</td></tr>`).join('')}</table>`)}
      ${actions([btn('Sweep idle…', da('sweep'))])}`, copy: { kind: 'worktrees', total: m?.worktree_bytes, biggest: list.map(w => pick(w, ['path', 'bytes', 'idle_days', 'sweepable'])) } };
  }
  if (seg === 'caches') {
    const list = (S().scan?.items || []).filter(i => !i.in_worktree).sort((a, b) => b.bytes - a.bytes).slice(0, 12);
    return { title: 'Build caches in main repos', html: `${sec('Biggest', `<table class="mini-t"><colgroup><col><col style="width:80px"></colgroup>${list.map(i => `<tr class="click" role="button" tabindex="0" ${dl('cache', i.path)}><td><span class="cell">${esc(tilde(i.path))}</span></td><td class="n">${B(i.bytes)}</td></tr>`).join('')}</table>`)}
      ${actions([btn('Clear stale…', da('stale'))])}`, copy: { kind: 'build_caches', biggest: list.map(i => pick(i, ['path', 'bytes', 'stale', 'in_use'])) } };
  }
  const used = s.disk_total - s.disk_free;
  return seg === 'free'
    ? { title: 'Free space', html: kv([['free', B(s.disk_free)], ['of', B(s.disk_total)], ['alert below', `${S().config?.alert_below_gb ?? '?'} GB`]]), copy: { kind: 'free', disk_free: s.disk_free, disk_total: s.disk_total } }
    : { title: 'Everything else', html: `<p>${B(used)} is used on this volume. Mulchie measures worktrees, build caches and known package stores; the rest is apps, documents, system files and other data it does not track.</p>`, copy: { kind: 'other', used } };
}

export const actKey = a => `${a.at_unix}:${a.kind}:${a.target || a.detail.slice(0, 60)}`;

function rAction(key) {
  const a = st.actions.find(x => actKey(x) === key);
  if (!a) return gone('Activity', '', 'This entry is no longer in the journal.');
  const bk = a.backups || [], find = k => bk.find(b => b.kind === k)?.location;
  let restore = '';
  if (a.kind === 'worktree' && a.status !== 'skipped' && a.target) {
    const t = a.target, repo = t.includes('/.claude/worktrees/') ? t.split('/.claude/worktrees/')[0] : '<repo>';
    const branch = find('branch'), patch = find('patch'), arch = find('ignored_archive');
    restore = sec('Restore this worktree', `<pre class="code">${esc([
      `git -C ${tilde(repo)} worktree add ${tilde(t)} ${branch || '<branch>'}`,
      patch && `git -C ${tilde(t)} apply ${tilde(patch)}`,
      arch && `tar -xzf ${tilde(arch)} -C ${tilde(t)}`,
    ].filter(Boolean).join('\n'))}</pre>${branch ? '' : '<div class="dim">The branch was kept. <span class="mono">git branch</span> in the repository lists it; agent branches are usually named like the worktree.</div>'}`);
  }
  const html = `${kv([['time', esc(new Date(a.at_unix * 1000).toLocaleString())], ['source', esc(a.source)], ['kind', esc(a.kind)], a.status && ['status', esc(a.status)], a.reason && ['reason', `<span class="warn">${esc(reasonText(a.reason))}</span>`], a.workspace && ['workspace', esc(a.workspace)], a.target && ['target', esc(tilde(a.target))], a.bytes && a.kind !== 'alert' && ['freed', B(a.bytes)]])}
    <h3 class="ds-h">Detail</h3><p class="wrap-t">${esc(a.detail)}</p>
    ${bk.length ? sec('Backups', bk.map(b => b.kind === 'branch' ? `<div class="bkrow">branch <span class="mono">${esc(b.location)}</span></div>` : `<div class="bkrow">${esc(b.kind === 'patch' ? 'patch' : 'ignored files')} <span class="mono">${esc(tilde(b.location))}</span> <button class="btn sm" data-reveal="${esc(b.location)}">Reveal</button></div>`).join('')) : ''}
    ${restore}`;
  return { title: `${a.kind}${a.status === 'skipped' ? ' · skipped' : ''}`, sub: `<span class="dim">${esc(ago(a.at_unix))} ago</span>`, html, copy: { kind: 'activity', ...a } };
}

function gone(title, path, why = 'It is no longer reported by the engine.') {
  return { title, html: `<p class="muted">${path ? esc(tilde(path)) + '<br><br>' : ''}${esc(why)}</p>`, copy: { kind: 'gone', title, path } };
}

const RENDER = { wt: rWorktree, ws: rWorkspace, proc: rProc, group: rGroup, cpu: () => rResource('cpu'), mem: () => rResource('mem'), agents: rAgents, cache: rCache, cat: rCategory, seg: rSegment, act: rAction, finding: rFinding, findings: rFindings };
export const NEEDS_ACTIONS = new Set(['wt', 'act']);

// ------------------------------------------------------------- the drawer

let last = '', opener = null;

export const detailOpen = () => !!st.ui.detail;

export function openDetail(type, key) {
  if (!RENDER[type]) return;
  if (!st.ui.detail) opener = document.activeElement;
  st.ui.detail = { type, key: String(key ?? '') };
  if (type === 'proc') loadProc(key, true);
  last = '';
  renderDetail();
  $('#dBody').scrollTop = 0;
  $('#dClose').focus({ preventScroll: true });
  if (NEEDS_ACTIONS.has(type)) hooks.refresh(true);
}

export function closeDetail() {
  st.ui.detail = null;
  renderDetail();
  if (opener?.isConnected) opener.focus();
  opener = null;
}

// A selector that finds the same control after a re-render.
function focusSel(el) {
  if (!el || !el.closest?.('#drawer')) return null;
  for (const a of ['data-da', 'data-d', 'data-reveal', 'data-term']) {
    if (el.hasAttribute(a)) {
      const extra = a === 'data-da' ? `[data-arg="${CSS.escape(el.dataset.arg || '')}"]` : a === 'data-d' ? `[data-dk="${CSS.escape(el.dataset.dk || '')}"]` : '';
      return `#drawer [${a}="${CSS.escape(el.getAttribute(a))}"]${extra}`;
    }
  }
  if (el.tagName === 'SUMMARY') return `#drawer details[data-key="${CSS.escape(el.parentElement.dataset.key)}"] > summary`;
  return null;
}

export function renderDetail() {
  const d = st.ui.detail, el = $('#drawer');
  if (!el) return;
  el.classList.toggle('on', !!(d && st.S));
  el.setAttribute('aria-hidden', d && st.S ? 'false' : 'true');
  document.body.classList.toggle('drawer-on', !!(d && st.S));
  if (!d || !st.S) { last = ''; return; }
  const r = RENDER[d.type](d.key);
  st.ui.detailCopy = { title: r.title, data: r.copy };
  $('#dTitle').textContent = r.title;
  $('#dSub').innerHTML = r.sub || '';
  if (r.html === last) return;
  const body = $('#dBody'), top = body.scrollTop, sel = focusSel(document.activeElement);
  const open = [...body.querySelectorAll('details[open]')].map(x => x.dataset.key);
  body.innerHTML = r.html;
  last = r.html;
  for (const k of open) body.querySelector(`details[data-key="${CSS.escape(k)}"]`)?.setAttribute('open', '');
  body.scrollTop = top;
  if (sel) body.querySelector(sel.replace('#drawer ', ''))?.focus({ preventScroll: true });
}

// Text an agent can act on: a heading and the facts as JSON.
export function copyText(title, data) {
  const at = new Date().toISOString().slice(0, 16).replace('T', ' ');
  return `# Mulchie: ${title} (${at})\n${JSON.stringify(data)}`;
}

// ------------------------------------------------------------- actions

const decode = s => JSON.parse(decodeURIComponent(s));

// Run suggestion `i` of finding `id`: a signal is planned again (and must
// still name the observed process); a renice goes to the gate with the
// observed start time.
export async function runSuggestion(i, id) {
  const f = findingBy(id), sg = f?.suggested[+i], r = sg?.request;
  if (!r) return toast('That observation is gone.', true);
  if (r.action === 'signal') {
    return planAndRun([{ pid: r.pid, signal: r.signal }], { title: `${sg.label}?`, verb: suggestionLabel(sg).replace('…', ''), danger: !sg.reversible, expect: { pid: r.pid, start_time: r.expected_start_time } });
  }
  if (r.action === 'renice') {
    const row = procBy(r.pid);
    return reniceProc({ pid: r.pid, name: f.evidence?.name || `pid ${r.pid}`, start_time: r.expected_start_time, nice: row?.start_time === r.expected_start_time ? row.nice : undefined }, r.nice);
  }
  return toast('This suggestion has no action the app can run.', true);
}

export async function detailAction(t) {
  const name = t.dataset.da, arg = t.dataset.arg;
  if (name.startsWith('g-')) {
    const { key, name: label } = decode(arg);
    return groupAction(key, name.slice(2), label);
  }
  if (name.startsWith('p-')) {
    const pid = +arg, p = procBy(pid) || (pd.pid === pid && pd.data);
    if (!p) return toast('That process is gone.', true);
    if (name === 'p-renice') return reniceProc(p);
    if (name === 'p-tree') return planAndRun([{ pid, signal: 'term', tree: true }], { title: `Quit ${p.name} (${pid}) and its children?`, verb: 'Quit', note: 'Sends SIGTERM to the process and every descendant, children first.' });
    return signalPids([pid], name.slice(2), `${p.name} (${pid})`);
  }
  switch (name) {
    case 'fx': {
      const [i, ...id] = arg.split(':');
      return runSuggestion(i, id.join(':'));
    }
    case 'dismiss': {
      const days = +($('#dDays')?.value || 30);
      return act(() => invoke('dismiss_finding', { id: arg, days }), u => `Dismissed until ${until(u)}`);
    }
    case 'undismiss': return act(() => invoke('undismiss_finding', { id: arg }), 'Shown again');
    case 'stop-here': {
      const w = wsAt(arg);
      return groupAction('workspace:' + arg, 'quit', `everything running in ${w?.label || tilde(arg).split('/').pop()}`);
    }
    case 'remove-wt': return planThenExecute({ request: { kind: 'worktrees', paths: [arg], allow_dirty: true }, batch: 'worktree', title: 'Remove this worktree?', verb: 'Remove', intro: 'Runs <span class="mono">git worktree remove</span>. The branch is kept; uncommitted changes and ignored files that are not caches are saved first.' });
    case 'clean-caches': {
      const paths = (S().scan?.items || []).filter(i => i.path.startsWith(arg + '/')).map(i => i.path);
      return planThenExecute({ request: { kind: 'clean', paths }, batch: 'clean', title: 'Clear the build caches here?', verb: 'Clear', intro: 'Deletes these folders. They are rebuilt on the next build.' });
    }
    case 'clean-one': return planThenExecute({ request: { kind: 'clean', paths: [arg] }, batch: 'clean', title: 'Clear this build cache?', verb: 'Clear', intro: 'Deletes the folder. It is rebuilt on the next build.' });
    case 'prune': {
      const c = S().diskmap.categories.find(x => x.key === arg);
      if (!(await confirmBox(`Prune ${c.name}?`, `Runs <span class="mono">${esc(c.hint)}</span>. Packages are downloaded again when next needed.`, 'Prune', false))) return;
      toast('Pruning…');
      return act(() => invoke('prune', { key: arg }), r => r.ok ? `Freed ${B(r.freed)}${r.output ? '\n' + r.output : ''}` : `Not pruned: ${r.output || r.reason || 'failed'}`);
    }
    default: return hooks.run?.(name);
  }
}
