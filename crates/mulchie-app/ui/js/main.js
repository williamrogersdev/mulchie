// Entry point: polling, routing and event wiring for the panel and dashboard.
import { invoke, listen } from './ipc.js';
import { st, hooks, MODE, HISTORY_MAX } from './store.js';
import { $, $$, toast, setHome } from './util.js';
import { modalOpen, dismissModal } from './dialogs.js';
import { act } from './act.js';
import { renderTop, renderOverview } from './views/overview.js';
import { renderProcs, procRows, procAct } from './views/processes.js';
import { renderWss, wsAct } from './views/workspaces.js';
import { renderDisk, removeSelectedWorktrees, sweep, clearSelectedCaches, clearStale, prune } from './views/disk.js';
import { renderActs } from './views/activity.js';
import { renderAgents, loadIntegration } from './views/agents.js';
import { renderSettings, saveCfg } from './views/settings.js';
import { renderPanel } from './views/panel.js';

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
}

// Whether this window is on screen. Rust also tells us on show and hide
// (mulchie://visible), so a newly shown window refreshes at once.
let visible = true;
async function onScreen() {
  if (document.hidden) return false;
  try { visible = await invoke('window_visible'); } catch (e) { visible = true; }
  return visible;
}

// One poll: the slim state, then config and actions only if they changed
// (actions only while the Activity view is open), then new history samples.
let busy = false;
async function refresh(force) {
  if (busy && !force) return;
  if (!force && !(await onScreen())) return;
  busy = true;
  try {
    const r = await invoke('get_state');
    st.conn = r.conn;
    st.error = r.error;
    const S = r.state;
    if (S) {
      if (!st.cfg || S.config_rev !== st.cfgRev) {
        st.cfg = await invoke('get_config');
        st.cfgRev = st.cfg.rev;
        if (st.view === 'settings') renderSettings();
      }
      S.config = st.cfg.config;
      if (MODE !== 'panel' && st.view === 'activity' && S.actions_rev !== st.actionsRev) {
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
    }
    if (!modalOpen()) render();
  } catch (e) {
    console.error(e);
  }
  busy = false;
}

function go(v, opts = {}) {
  const ui = st.ui;
  if (v.startsWith('ws:')) { ui.wFocus = v.slice(3); v = 'workspaces'; }
  if (v.startsWith('proc:')) { opts.group = v.slice(5); v = 'processes'; }
  if (!VIEWS.includes(v)) v = 'overview';
  const changed = st.view !== v;
  st.view = v;
  $$('nav a').forEach(a => a.classList.toggle('on', a.dataset.v === v));
  $$('.view').forEach(s => s.classList.toggle('on', s.id === 'v-' + v));
  if (opts.group != null) { $('#pFilter').value = opts.group; ui.pKind = ''; }
  $$('#kindSeg button').forEach(b => b.classList.toggle('on', b.dataset.k === ui.pKind));
  if (v === 'agents') st.integ = null;
  render();
  if (changed && (v === 'activity' || v === 'settings')) refresh(true);
}

hooks.refresh = refresh;
hooks.render = render;
hooks.go = go;

const toDash = view => invoke('open_dashboard', { view });

const SEL = '[data-v],[data-sort],[data-pid],[data-psel],[data-pact],[data-pclose],[data-pfocus],[data-pjump],[data-bulk],[data-wpath],[data-wact],[data-wclose],[data-reveal],[data-term],[data-ws],[data-go],[data-gproc],[data-open],[data-prune],[data-copy],[data-k],[data-all],[data-wtsel],[data-bcsel],#wtSeg button,#bcSeg button,#aSeg button,#showTok';

document.addEventListener('click', async e => {
  const t = e.target.closest(SEL);
  if (!t) return;
  const d = t.dataset, ui = st.ui;
  if (t.id === 'showTok') { $('#tok').textContent = st.integ?.token || ''; t.remove(); return; }
  if (d.open) return MODE === 'panel' ? toDash(d.open) : go(d.open);
  if (d.gproc != null) return MODE === 'panel' ? toDash('proc:' + d.gproc) : go('processes', { group: d.gproc });
  if (d.go) { if (modalOpen()) dismissModal(); return MODE === 'panel' ? toDash(d.go) : go(d.go); }
  if (d.v) return go(d.v);
  if (d.reveal) return act(() => invoke('reveal', { path: d.reveal, terminal: false }));
  if (d.term) return act(() => invoke('reveal', { path: d.term, terminal: true }));
  if (d.copy) {
    const text = d.copy === 'token' ? st.integ?.token || '' : d.copy;
    await navigator.clipboard.writeText(text).catch(() => {});
    return toast('Copied');
  }
  if (!st.S) return;
  if (d.sort) {
    const [tbl, k] = d.sort.split(':'), key = { p: 'pSort', w: 'wSort', wt: 'wtSort' }[tbl];
    ui[key] = ui[key][0] === k ? [k, -ui[key][1]] : [k, TEXT_KEYS.includes(k) ? 1 : -1];
    return render();
  }
  if (d.all) {
    const on = t.checked;
    if (d.all === 'p') procRows().filter(p => p.own).forEach(p => on ? ui.pSel.add(p.pid) : ui.pSel.delete(p.pid));
    if (d.all === 'wt') $$('[data-wtsel]').filter(x => !x.disabled).forEach(x => on ? ui.wtSel.add(x.dataset.wtsel) : ui.wtSel.delete(x.dataset.wtsel));
    if (d.all === 'bc') $$('[data-bcsel]').filter(x => !x.disabled).forEach(x => on ? ui.bcSel.add(x.dataset.bcsel) : ui.bcSel.delete(x.dataset.bcsel));
    return render();
  }
  if (d.psel) { const pid = +d.psel; t.checked ? ui.pSel.add(pid) : ui.pSel.delete(pid); return render(); }
  if (d.wtsel) { t.checked ? ui.wtSel.add(d.wtsel) : ui.wtSel.delete(d.wtsel); return render(); }
  if (d.bcsel) { t.checked ? ui.bcSel.add(d.bcsel) : ui.bcSel.delete(d.bcsel); return render(); }
  if (d.k != null && t.closest('#kindSeg')) { ui.pKind = d.k; $$('#kindSeg button').forEach(b => b.classList.toggle('on', b === t)); return render(); }
  if (t.closest('#wtSeg')) { ui.wtF = d.f; return render(); }
  if (t.closest('#bcSeg')) { ui.bcF = d.f; return render(); }
  if (t.closest('#aSeg')) { ui.aSrc = d.s; return render(); }
  if (d.pclose != null) { ui.pFocus = null; return render(); }
  if (d.pfocus) { ui.pFocus = +d.pfocus; return render(); }
  if (d.pjump) { ui.pFocus = +d.pjump; $('#pFilter').value = ''; return go('processes'); }
  if (d.pid) { ui.pFocus = +d.pid === ui.pFocus ? null : +d.pid; return render(); }
  if (d.pact) return procAct([ui.pFocus], d.pact);
  if (d.bulk) return procAct([...ui.pSel], d.bulk);
  if (d.wclose != null) { ui.wFocus = null; return render(); }
  if (d.wpath) { ui.wFocus = d.wpath === ui.wFocus ? null : d.wpath; return render(); }
  if (d.wact) return wsAct(ui.wFocus, d.wact);
  if (d.ws) { ui.wFocus = d.ws; return go('workspaces'); }
  if (d.prune) return prune(d.prune);
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
  const ui = st.ui;
  // Nav items, findings and notices act as buttons from the keyboard too.
  if ((e.key === 'Enter' || e.key === ' ') && e.target.matches?.('[role="button"]')) {
    e.preventDefault();
    e.target.click();
    return;
  }
  if (e.key === 'Escape') {
    if (modalOpen()) dismissModal();
    else if (ui.pFocus) { ui.pFocus = null; render(); }
    else if (ui.wFocus) { ui.wFocus = null; render(); }
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
