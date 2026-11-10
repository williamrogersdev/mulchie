// The tray panel: what the machine is doing in one sentence, who is using the
// CPU and memory, and what needs attention. Everything opens the dashboard at
// the matching detail; actions run there, through the plan dialogs.
import { st } from '../store.js';
import { invoke } from '../ipc.js';
import { $, esc, B, clock } from '../util.js';
import { findings, findingsHtml, notices, noticesHtml, modeHtml, modeTitle } from '../findings.js';
import { barsHtml, cpuSummary, memSummary, cpuTile, memTile } from '../consumers.js';

let lastH = 0;

// Size the panel window to its content, within the screen-friendly maximum.
function fit() {
  const want = Math.ceil($('#panel .ph').offsetHeight + $('#pBody').scrollHeight + $('#panel .pf').offsetHeight + 2);
  if (Math.abs(want - lastH) > 3) {
    lastH = want;
    invoke('fit_panel', { height: want }).catch(() => {});
  }
}

const tile = (id, t) => {
  $(`#${id}`).textContent = t.v;
  $(`#${id}S`).textContent = t.s;
  $(`#${id}S`).className = 's ' + (t.level === 'ok' ? '' : t.level);
  $(`#${id}T`).title = t.title;
};

export function renderPanel() {
  const S = st.S;
  const mode = $('#pMode');
  mode.innerHTML = modeHtml();
  mode.title = modeTitle();
  $('#pNotices').innerHTML = noticesHtml(notices(), true);
  if (S) {
    const s = S.snapshot, stale = st.conn?.mode === 'disconnected' || st.error ? 'stale' : S.health;
    const el = $('#pHealth'); el.querySelector('.dot').className = 'dot ' + stale; el.querySelector('span:last-child').textContent = stale;
    $('#pTime').textContent = s.at_unix ? clock(s.at_unix) : '';
    const cs = cpuSummary(S), ms = memSummary(S);
    $('#pHead').innerHTML = `${esc(cs.headline)}${ms.level !== 'ok' ? ` <span class="${ms.level}">${esc(ms.headline)}</span>` : ''}`;
    tile('pCpu', cpuTile(S));
    tile('pMem', memTile(S));
    $('#pDisk').textContent = B(s.disk_free);
    $('#pDiskS').textContent = S.forecast?.hours_to_full != null ? `full in ~${S.forecast.hours_to_full.toFixed(0)} h` : `of ${B(s.disk_total, 0)}`;
    $('#pAg').textContent = `${s.agent_count} · ${s.build_count}`;
    $('#pAgS').textContent = `slots ${S.slots.active.length}/${S.slots.capacity}${S.slots.waiting.length ? ` · ${S.slots.waiting.length} queued` : ''}`;
    $('#pCpuTop').innerHTML = barsHtml(S, 'cpu_pct', 3);
    $('#pMemTop').innerHTML = barsHtml(S, 'mem_bytes', 3);
    const f = findings(), c = S.finding_counts;
    $('#pFindW').style.display = f.length || c?.high || c?.medium || c?.low ? '' : 'none';
    $('#pFind').innerHTML = findingsHtml(f, true);
    $('#pWatch').innerHTML = c && (c.high + c.medium + c.low + c.info) ? `<a class="lnk" role="button" tabindex="0" data-d="findings" data-dk="" title="Open the list of observations">Observations: ${[['high', c.high, 'bad'], ['medium', c.medium, 'warn'], ['low', c.low, 'info'], ['info', c.info, 'info']].filter(x => x[1]).map(([l, n, k]) => `<span class="${k}">${n} ${l}</span>`).join(' · ')}</a>` : '';
  }
  fit();
}
