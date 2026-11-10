// Entry point: polling, routing and event wiring for the panel and dashboard.
import { invoke, listen } from './ipc.js';
import { st, hooks, MODE, HISTORY_MAX } from './store.js';
import { $, $$, toast, setHome } from './util.js';
import { modalOpen, dismissModal } from './dialogs.js';
import { act } from './act.js';
import { renderTop, renderOverview } from './views/overview.js';
import { renderProcs, procBulk } from './views/processes.js';
import { renderWss } from './views/workspaces.js';
import { renderDisk, removeSelectedWorktrees, sweep, clearSelectedCaches, clearStale } from './views/disk.js';
import { renderActs } from './views/activity.js';
import { renderAgents, loadIntegration } from './views/agents.js';
import { renderSettings, saveCfg } from './views/settings.js';
import { renderPanel } from './views/panel.js';
import { openDetail, closeDetail, detailOpen, renderDetail, detailAction, copyText, runSuggestion, NEEDS_ACTIONS } from './detail.js';
import { findings, watchFindings } from './findings.js';
import { cpuSummary, memSummary, ranked } from './consumers.js';
import { groupAction } from './signals.js';

const POLL_MS = 3000;
const VIEWS = ['overview', 'processes', 'workspaces', 'disk', 'activity', 'agents', 'settings'];
const TEXT_KEYS = ['name', 'group', 'label', 'branch', 'repo_name', 'kind'];

document.body.classList.add(MODE === 'panel' ? 'panel' : 'dashboard');

function render() {
  if (MODE === 'panel') return renderPanel();
  renderTop();
  if (!st.S) return;
  const v = st.view;
  if (v === 'overview') renderOverview();
  else if (v === 'processes') renderProcs();
  else if (v === 'workspaces') renderWss();
  else if (v === 'disk') renderDisk();
  else if (v === 'activity') renderActs();
  else if (v === 'agents') renderAgents();
  else if (v === 'settings') renderSettings();
  renderDetail();
}

// Whether this window is on screen. Rust also tells us on show and hide
// (mulchie://visible), so a newly shown window refreshes at once.
let visible = true;
async function onScreen() {
  if (document.hidden) return false;
  try { visible = await invoke('window_visible'); } catch (e) { visible = true; }
  return visible;
}

// One poll: the slim state, then config when it or the connection mode
// changed, actions only when shown (Activity, or a detail that lists them),
// then new history samples. Any failure is kept in `st.error`, which shows as
// a notice and marks the figures stale until a poll succeeds.
let busy = false;
async function refresh(force) {
  if (busy && !force) return;
  if (!force && !(await onScreen())) return;
  busy = true;
  try {
    const r = await invoke('get_state');
    const modeChanged = st.conn && st.conn.mode !== r.conn.mode;
    st.conn = r.conn;
    let error = r.error || null;
    const S = r.state;
    if (modeChanged) {
      // A takeover or a reconnect: settings ownership and the API details
      // (port, token, who serves it) changed with the engine.
      st.cfgRev = null;
      st.integ = null;
      if (MODE !== 'panel') loadIntegration(true).then(() => { if (st.view === 'agents') renderAgents(); });
    }
    if (S) {
      try {
        if (!st.cfg || S.config_rev !== st.cfgRev) {
          st.cfg = await invoke('get_config');
          st.cfgRev = st.cfg.rev;
          if (st.view === 'settings') renderSettings(true);
        }
        S.config = st.cfg.config;
        const wantActs = MODE !== 'panel' && (st.view === 'activity' || NEEDS_ACTIONS.has(st.ui.detail?.type));
        if (wantActs && S.actions_rev !== st.actionsRev) {
          const a = await invoke('get_actions');
          st.actions = a.actions;
          st.actionsRev = a.rev;
        }
        const last = st.H.length ? st.H[st.H.length - 1].t : 0;
        // A different engine (after reconnecting) has its own history.
        const since = st.S && st.S.version === S.version && st.connMode === r.conn.mode ? last : 0;
        const fresh = await invoke('get_history', { since });
        st.H = (since ? st.H.concat(fresh) : fresh).slice(-HISTORY_MAX);
        st.connMode = r.conn.mode;
        st.S = S;
      } catch (e) {
        error = String(e);
      }
    }
    st.error = r.conn.mode === 'disconnected' ? null : error;
  } catch (e) {
    // get_state itself failed (the backend, not the engine).
    st.error = String(e);
  }
  busy = false;
  if (!modalOpen()) {
    try { render(); } catch (e) { console.error('render failed', e); }
  }
}

// Views, details ("d:type:key") and actions ("run:name") by name, so the
// panel and findings can point anywhere in the dashboard.
function go(v, opts = {}) {
  const ui = st.ui;
  if (v.startsWith('run:')) return run(v.slice(4));
  if (v.startsWith('d:')) {
    const [, type, ...key] = v.split(':');
    const k = key.join(':');
    const home = { wt: 'disk', cache: 'disk', cat: 'disk', seg: 'disk', act: 'activity', proc: 'processes', ws: 'workspaces' }[type];
    if (home && st.view !== home && !(type === 'proc' || type === 'ws')) go(home);
    return openDetail(type, k);
  }
  if (v.startsWith('ws:')) return openDetail('ws', v.slice(3));
  if (v.startsWith('proc:')) { opts.group = v.slice(5); v = 'processes'; }
  if (!VIEWS.includes(v)) v = 'overview';
  const changed = st.view !== v;
  st.view = v;
  $$('nav a').forEach(a => { a.classList.toggle('on', a.dataset.v === v); a.setAttribute('aria-current', a.dataset.v === v ? 'page' : 'false'); });
  $$('.view').forEach(s => s.classList.toggle('on', s.id === 'v-' + v));
  if (opts.group != null) { $('#pFilter').value = opts.group; ui.pKind = ''; }
  $$('#kindSeg button').forEach(b => b.classList.toggle('on', b.dataset.k === ui.pKind));
  if (v === 'agents') st.integ = null;
  render();
  if (changed && (v === 'activity' || v === 'settings')) refresh(true);
}

// Named actions for findings, the panel and the drawer.
function run(name) {
  if (name === 'sweep') return sweep();
  if (name === 'stale') return clearStale();
  if (name.startsWith('go:')) return go(name.slice(3));
  if (name.startsWith('d:')) return go(name);
  if (name.startsWith('stop:')) {
    const w = st.S?.snapshot.workspaces.find(x => x.path === name.slice(5));
    return groupAction('workspace:' + name.slice(5), 'quit', w?.label || name.slice(5));
  }
  if (name.startsWith('fx:')) {
    const [, i, ...id] = name.split(':');
    return runSuggestion(i, id.join(':'));
  }
}

hooks.refresh = refresh;
hooks.render = render;
hooks.go = go;
hooks.run = run;

const toDash = view => invoke('open_dashboard', { view });

async function copy(text, what = 'Copied') {
  try {
    await navigator.clipboard.writeText(text);
    toast(what);
  } catch (e) {
    toast(`Could not copy: ${e?.message || e}`, true);
  }
}

// A compact picture of the machine for an agent: health, the top consumers,
// disk, what needs attention, and how to reach Mulchie.
function overviewCopy() {
  const S = st.S, s = S.snapshot, cs = cpuSummary(S), ms = memSummary(S);
  const top = (by, n) => ranked(S, by).slice(0, n).map(c => ({ key: c.key, label: c.label, kind: c.kind, cpu_pct: Math.round(c.cpu_pct), mem_bytes: c.mem_bytes, path: c.path || undefined, actions: c.actions, protected: c.protected }));
  return copyText('machine overview', {
    health: S.health, engine: st.conn?.mode, api_port: st.conn?.port,
    cpu: { headline: cs.headline, cpu_pct: s.cpu_pct, cores: s.cpu_count, load: s.load, top: top('cpu_pct', 5) },
    memory: { headline: ms.headline, used: s.mem_total - s.mem_available, total: s.mem_total, swap_used: s.swap_used, top: top('mem_bytes', 5) },
    disk: { free: s.disk_free, total: s.disk_total, stale_cache_bytes: S.scan?.stale_bytes || 0, sweepable_worktree_bytes: S.diskmap?.sweepable_bytes || 0 },
    agents: s.agent_count, builds: s.build_count, build_slots: { active: S.slots.active.length, capacity: S.slots.capacity, waiting: S.slots.waiting.length },
    needs_attention: findings().map(f => f.t),
    observations: watchFindings(S).map(f => ({ id: f.id, severity: f.severity, title: f.title })),
    next: 'Call the mulchie MCP tool diagnose first; act on processes with a process plan, then execute_plan with the approved items.',
  });
}

const SEL = '[data-psel],[data-wtsel],[data-bcsel],[data-all],[data-da],[data-run],[data-reveal],[data-term],[data-copy],[data-copy-el],[data-bulk],[data-v],[data-sort],[data-open],[data-go],[data-gproc],[data-k],[data-d],#wtSeg button,#bcSeg button,#aSeg button,#showTok,#dClose,#dCopy,#copyOverview';

document.addEventListener('click', async e => {
  const t = e.target.closest(SEL);
  if (!t || t.disabled) return;
  const d = t.dataset, ui = st.ui;
  if (t.id === 'dClose') return closeDetail();
  if (t.id === 'dCopy') return st.ui.detailCopy && copy(copyText(st.ui.detailCopy.title, st.ui.detailCopy.data), 'Copied for an agent');
  if (t.id === 'copyOverview') return st.S && copy(overviewCopy(), 'Copied for an agent');
  if (t.id === 'showTok') { $('#tok').textContent = st.integ?.token || ''; t.remove(); return; }
  if (d.copy) return copy(d.copy === 'token' ? st.integ?.token || '' : d.copy);
  if (d.copyEl) return copy($('#' + d.copyEl).textContent);
  if (d.open) return MODE === 'panel' ? toDash(d.open) : go(d.open);
  if (d.run) {
    if (MODE === 'panel') return toDash('run:' + d.run);
    if (modalOpen()) dismissModal();
    return run(d.run);
  }
  if (d.gproc != null) return MODE === 'panel' ? toDash('proc:' + d.gproc) : go('processes', { group: d.gproc });
  if (d.go) { if (modalOpen()) dismissModal(); return MODE === 'panel' ? toDash(d.go) : go(d.go); }
  if (d.v) return go(d.v);
  if (d.reveal) return act(() => invoke('reveal', { path: d.reveal, terminal: false }));
  if (d.term) return act(() => invoke('reveal', { path: d.term, terminal: true }));
  if (d.da) return detailAction(t);
  if (!st.S) return;
  // Checkboxes inside clickable rows select; they do not open the row.
  if (d.all) {
    const on = t.checked;
    if (d.all === 'p') $$('[data-psel]').filter(x => !x.disabled).forEach(x => on ? ui.pSel.add(+x.dataset.psel) : ui.pSel.delete(+x.dataset.psel));
    if (d.all === 'wt') $$('[data-wtsel]').filter(x => !x.disabled).forEach(x => on ? ui.wtSel.add(x.dataset.wtsel) : ui.wtSel.delete(x.dataset.wtsel));
    if (d.all === 'bc') $$('[data-bcsel]').filter(x => !x.disabled).forEach(x => on ? ui.bcSel.add(x.dataset.bcsel) : ui.bcSel.delete(x.dataset.bcsel));
    return render();
  }
  if (d.psel) { const pid = +d.psel; t.checked ? ui.pSel.add(pid) : ui.pSel.delete(pid); return render(); }
  if (d.wtsel) { t.checked ? ui.wtSel.add(d.wtsel) : ui.wtSel.delete(d.wtsel); return render(); }
  if (d.bcsel) { t.checked ? ui.bcSel.add(d.bcsel) : ui.bcSel.delete(d.bcsel); return render(); }
  if (d.d) {
    if (MODE === 'panel') return toDash(`d:${d.d}:${d.dk ?? ''}`);
    if (modalOpen()) dismissModal();
    return openDetail(d.d, d.dk);
  }
  if (d.sort) {
    const [tbl, k] = d.sort.split(':'), key = { p: 'pSort', w: 'wSort', wt: 'wtSort' }[tbl];
    ui[key] = ui[key][0] === k ? [k, -ui[key][1]] : [k, TEXT_KEYS.includes(k) ? 1 : -1];
    return render();
  }
  if (d.k != null && t.closest('#kindSeg')) { ui.pKind = d.k; $$('#kindSeg button').forEach(b => b.classList.toggle('on', b === t)); return render(); }
  if (t.closest('#wtSeg')) { ui.wtF = d.f; return render(); }
  if (t.closest('#bcSeg')) { ui.bcF = d.f; return render(); }
  if (t.closest('#aSeg')) { ui.aSrc = d.s; return render(); }
  if (d.bulk) return procBulk(d.bulk);
});

const on = (sel, ev, fn) => { const el = $(sel); if (el) el.addEventListener(ev, fn); };
on('#pFilter', 'input', render);
on('#wFilter', 'input', render);
on('#pMine', 'change', render);
on('#pClear', 'click', () => { st.ui.pSel.clear(); render(); });
on('#wtClear', 'click', () => { st.ui.wtSel.clear(); render(); });
on('#bcClear', 'click', () => { st.ui.bcSel.clear(); render(); });
on('#rescan', 'click', () => act(() => invoke('rescan'), 'Rescan started'));
on('#saveCfg', 'click', saveCfg);
on('#form', 'input', () => { st.ui.cfgDirty = true; });
on('#form', 'change', () => { st.ui.cfgDirty = true; });
on('#pQuit', 'click', () => invoke('quit'));
on('#installCli', 'click', () => act(() => invoke('install_cli'), p => { st.integ = null; return `Linked ${p}\nMake sure ~/.local/bin is on your PATH.`; }));
on('#wtRemove', 'click', removeSelectedWorktrees);
on('#wtSweep', 'click', sweep);
on('#bcClean', 'click', clearSelectedCaches);
on('#bcStale', 'click', clearStale);

document.addEventListener('keydown', e => {
  // Rows, tiles, findings and nav items act as buttons from the keyboard.
  if ((e.key === 'Enter' || e.key === ' ') && e.target.matches?.('[role="button"]')) {
    e.preventDefault();
    e.target.click();
    return;
  }
  if (e.key === 'Escape') {
    if (modalOpen()) dismissModal();
    else if (detailOpen()) closeDetail();
    return;
  }
  if (MODE === 'panel') return;
  if ((e.metaKey || e.ctrlKey) && e.key === 'f' && st.view === 'processes') { e.preventDefault(); $('#pFilter').focus(); }
  if ((e.metaKey || e.ctrlKey) && /^[1-7]$/.test(e.key)) { e.preventDefault(); go(VIEWS[+e.key - 1]); }
});
window.addEventListener('resize', () => { if (!modalOpen()) render(); });
document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(true); });
listen('mulchie://view', ev => go(ev.payload));
listen('mulchie://visible', ev => { visible = !!ev.payload; if (visible) refresh(true); });

loadIntegration().then(I => { if (I?.home) setHome(I.home); render(); });
go(st.view);
refresh(true);
setInterval(refresh, POLL_MS);
