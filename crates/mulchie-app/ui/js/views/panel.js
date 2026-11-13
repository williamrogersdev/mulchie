// The tray panel: how the machine is doing in one sentence and three
// figures, who is using it most, and what there is to resolve. Every row
// carries its own action; the action opens the dashboard at its review
// dialog, where the plan is shown before anything runs.
import { st } from '../store.js';
import { invoke } from '../ipc.js';
import { $, esc, B } from '../util.js';
import { findings, findingsHtml, cleanup, notices, noticesHtml, modeHtml, modeTitle } from '../findings.js';
import { cpuSummary, memSummary, cpuTile, memTile, ranked, cores, KIND_ICON } from '../consumers.js';

let lastH = 0;

// Size the panel window to its content, within the screen-friendly maximum.
function fit() {
  const want = Math.ceil($('#panel .ph').offsetHeight + $('#pBody').scrollHeight + $('#panel .pf').offsetHeight + 2);
  if (Math.abs(want - lastH) > 3) {
    lastH = want;
    invoke('fit_panel', { height: want }).catch(() => {});
  }
}

const stat = (label, v, sub, level, open, title) =>
  `<div class="pstat" role="button" tabindex="0" data-open="${esc(open)}" title="${esc(title)}"><div class="l">${label}</div><div class="v">${esc(v)}</div><div class="s ${level === 'ok' ? '' : level}">${esc(sub)}</div></div>`;

// The first part of a tile's note; the sentence above says the rest.
const short = t => String(t).split(' · ')[0];

// The biggest users of CPU, then of memory, each once: at most `n` rows.
function top(S, n) {
  const out = new Map();
  const add = c => { if (out.size < n && !out.has(c.key)) out.set(c.key, c); };
  ranked(S, 'cpu_pct').filter(c => c.cpu_pct >= 20).slice(0, 3).forEach(add);
  ranked(S, 'mem_bytes').filter(c => c.mem_bytes >= 5e8).forEach(add);
  ranked(S, 'cpu_pct').forEach(add);
  return [...out.values()];
}

// What a row can do: stop a workspace, quit an app or a program. Nothing
// for the system or a group the engine protects.
function rowAction(c) {
  if (c.protected || c.kind === 'system' || (c.actions && !c.actions.includes('quit'))) return '';
  const [label, run] = c.kind === 'workspace' && c.path ? ['Stop…', `stop:${c.path}`] : ['Quit…', `quit:${c.key}`];
  return `<button class="btn sm" data-run="${esc(run)}" title="Review what this would quit, then confirm">${label}</button>`;
}

function topHtml(S) {
  const list = top(S, 4);
  if (!list.length) return '<div class="dim">Nothing measurable.</div>';
  const maxCpu = Math.max(...list.map(c => c.cpu_pct)), maxMem = Math.max(...list.map(c => c.mem_bytes));
  return list.map(c => `<div class="prow" role="button" tabindex="0" data-d="group" data-dk="${esc(c.key)}" title="${esc(c.label)} · ${c.count} process(es)">
    <span class="nm"><span class="ki ${esc(c.kind)}">${KIND_ICON[c.kind] || '●'}</span>${esc(c.short)}</span>
    <span class="n ${c.cpu_pct === maxCpu && c.cpu_pct >= 100 ? 'hot' : ''}">${esc(cores(c.cpu_pct).replace(/ cores?$/, ''))} cores</span>
    <span class="n ${c.mem_bytes === maxMem && c.mem_bytes >= 4e9 ? 'hot' : ''}">${B(c.mem_bytes, c.mem_bytes >= 1e10 ? 0 : 1)}</span>
    ${rowAction(c) || '<span></span>'}</div>`).join('');
}

export function renderPanel() {
  const S = st.S;
  // The engine badge only when it says something: this app is a client of
  // another engine, or it is disconnected.
  const mode = $('#pMode'), odd = st.conn && st.conn.mode !== 'engine';
  mode.innerHTML = odd ? modeHtml() : '';
  mode.title = odd ? modeTitle() : '';
  // Only what stops Mulchie from working; the rest is in the dashboard.
  $('#pNotices').innerHTML = noticesHtml(notices().filter(n => n[0] === 'bad'), true);
  if (S) {
    const s = S.snapshot, stale = st.conn?.mode === 'disconnected' || st.error ? 'stale' : S.health;
    const el = $('#pHealth'); el.querySelector('.dot').className = 'dot ' + stale; el.querySelector('span:last-child').textContent = stale;
    const cs = cpuSummary(S), ms = memSummary(S), ct = cpuTile(S), mt = memTile(S);
    $('#pHead').innerHTML = `${esc(cs.headline)}${ms.level !== 'ok' ? ` <span class="${ms.level}">${esc(ms.headline)}</span>` : ''}`;
    const full = S.forecast?.hours_to_full, low = s.disk_free / 1e9 < (S.config?.low_disk_gb ?? 0);
    $('#pStats').innerHTML = [
      stat('CPU', ct.v.replace(' cores', ''), short(ct.s), cs.level, 'd:cpu:', ct.title),
      stat('Memory', mt.v.replace(/^(\S+) (\S+) of (\S+) \2$/, '$1 of $3 $2'), ms.level === 'ok' ? 'fine' : ms.level === 'warn' ? 'tight' : 'very tight', ms.level, 'd:mem:', mt.title),
      stat('Disk free', B(s.disk_free), full != null ? `full in ~${full.toFixed(0)} h` : `of ${B(s.disk_total, 0)}`, low ? 'bad' : full != null && full < 24 ? 'warn' : 'ok', 'disk', 'Open the Disk view'),
    ].join('');
    $('#pTop').innerHTML = topHtml(S);
    // What needs attention now (not the cleanup, which has its own list,
    // and not the notes that nothing can be done about).
    const f = findings().filter(x => x.kind !== 'cleanup' && x.lvl !== 'ok'), c = cleanup(S);
    $('#pFindW').style.display = f.length ? '' : 'none';
    $('#pFind').innerHTML = findingsHtml(f, 3);
    $('#pCleanW').style.display = c.length ? '' : 'none';
    $('#pClean').innerHTML = findingsHtml(c, 3);
    // One line for the rest, each a link to where it is listed. Sessions,
    // not breaches: one session can be over several caps.
    const over = new Set((S.sessions?.breaches || []).filter(b => b.cleared_unix == null).map(b => b.session_id));
    const nb = over.size, nu = S.updates?.available || 0, more = Math.max(0, f.length - 3);
    const lnk = (attrs, cls, text) => `<a class="lnk" role="button" tabindex="0" ${attrs}><span class="${cls}">${text}</span></a>`;
    $('#pLim').innerHTML = [
      more && lnk('data-open="overview"', 'warn', `${more} more to look at`),
      nb && lnk('data-v="sessions"', 'bad', `${nb === 1 ? '1 session' : `${nb} sessions`} over a limit`),
      nu && lnk('data-d="findings" data-dk="update"', 'info', `${nu === 1 ? '1 update' : `${nu} updates`}`),
    ].filter(Boolean).join(' · ');
  }
  fit();
}
