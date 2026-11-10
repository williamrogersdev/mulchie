// The top bar, nav counters and the Overview view.
import { st } from '../store.js';
import { $, esc, B, BS, P, ago, clock, th, meter, legend, css } from '../util.js';
import { chart } from '../chart.js';
import { findings, findingsHtml, notices, noticesHtml, modeHtml, modeTitle } from '../findings.js';
import { barsHtml, consumers, cores as coresTxt, cpuSummary, memSummary, cpuTile, memTile, KIND_ICON } from '../consumers.js';

export function renderTop() {
  const S = st.S;
  const mode = $('#tMode');
  mode.innerHTML = modeHtml();
  mode.title = modeTitle();
  $('#dNotices').innerHTML = noticesHtml(notices());
  if (!S) return;
  const s = S.snapshot, hc = st.conn?.mode === 'disconnected' || st.error ? 'stale' : S.health;
  const el = $('#tHealth'); el.querySelector('.dot').className = 'dot ' + hc; el.querySelector('span:last-child').textContent = hc;
  $('#tCpu').textContent = P(s.cpu_pct);
  $('#tLoad').textContent = s.load.map(x => x.toFixed(0)).join(' ') + ` /${s.cpu_count}`;
  $('#tMem').textContent = `${B(s.mem_total - s.mem_available)} / ${B(s.mem_total, 0)}`;
  $('#tSwap').textContent = B(s.swap_used);
  $('#tDisk').textContent = `${B(s.disk_free)} free`;
  $('#tIo').textContent = `r ${BS(s.read_bps)} · w ${BS(s.write_bps)}`;
  $('#tSlots').textContent = `${S.slots.active.length}/${S.slots.capacity}` + (S.slots.waiting.length ? ` +${S.slots.waiting.length} queued` : '');
  $('#tTime').textContent = s.at_unix ? clock(s.at_unix) : '';
  $('#nProc').textContent = s.proc_count;
  $('#nWs').textContent = s.workspaces.length;
  $('#nSess').textContent = S.sessions?.running || '';
  $('#nDisk').textContent = S.diskmap ? B(S.diskmap.worktree_bytes, 0) : '';
  $('#nAct').textContent = S.actions_count || '';
  $('#ver').textContent = 'v' + S.version;
  $('#scanInfo').textContent = S.scanning ? 'scanning disk…' : S.diskmap ? `disk scanned ${ago(S.diskmap.at_unix)} ago` : '';
}

const WS_COLORS = ['#6fb0f0', '#b691e8', '#79c37d', '#e3b24f', '#5fc9c0', '#ec8a5a'];

export function renderOverview() {
  const S = st.S, H = st.H, s = S.snapshot, mu = s.mem_total - s.mem_available, used = s.disk_total - s.disk_free;
  const fc = S.forecast?.hours_to_full != null ? `full in ~${S.forecast.hours_to_full.toFixed(0)} h` : 'stable over 30 min';
  const cs = cpuSummary(S), ms = memSummary(S), ct = cpuTile(S), mt = memTile(S);
  $('#oHead').innerHTML = `<span class="${cs.level}">${esc(cs.headline)}</span> <span class="${ms.level}">${esc(ms.headline)}</span>`;
  $('#kpis').innerHTML = [
    ['CPU', ct.v, ct.s + ` · ${P(s.cpu_pct)}`, s.cpu_pct, s.cpu_pct > 90 ? 'var(--warn)' : 'var(--c1)', 'data-d="cpu" data-dk=""', ct.title],
    ['Memory', mt.v, mt.s, s.mem_total ? mu / s.mem_total * 100 : 0, mu / s.mem_total > .9 ? 'var(--bad)' : 'var(--c2)', 'data-d="mem" data-dk=""', mt.title],
    ['Disk', B(s.disk_free) + ' free', `${B(used)} used of ${B(s.disk_total)} · ${fc}`, s.disk_total ? used / s.disk_total * 100 : 0, s.disk_free / 1e9 < S.config.low_disk_gb ? 'var(--bad)' : 'var(--c3)', 'data-go="disk"', 'Open the Disk view'],
    ['Agents · builds', `${s.agent_count} · ${s.build_count}`, `${s.workspaces.length} workspaces · slots ${S.slots.active.length}/${S.slots.capacity}${S.slots.waiting.length ? ` · ${S.slots.waiting.length} queued` : ''}`, S.slots.capacity ? S.slots.active.length / S.slots.capacity * 100 : 0, 'var(--c4)', 'data-d="agents" data-dk=""', 'Agents, builds and the build queue'],
  ].map(([l, v, sub, pct, col, to, tip]) => `<div class="card kpi" role="button" tabindex="0" ${to} title="${esc(tip)}"><h3>${l}</h3><div class="v">${esc(v)}</div><div class="s">${esc(sub)}</div><div class="bar"><i style="width:${Math.min(100, pct || 0)}%;background:${col}"></i></div></div>`).join('');
  $('#oCpuTop').innerHTML = barsHtml(S, 'cpu_pct', 5);
  $('#oMemTop').innerHTML = barsHtml(S, 'mem_bytes', 5);

  const cores = s.cpu_count || 1;
  chart($('#chCpu'), [
    { l: 'CPU', c: '#6fb0f0', v: H.map(x => x.cpu), fmt: v => P(v, 1) },
    { l: 'load/core', c: '#ec8a5a', v: H.map(x => x.load1 / cores), r: true, fmt: v => v.toFixed(2) + '×' },
  ], { fmt: v => v.toFixed(0) + '%', max: 100, rmax: Math.max(2, ...H.map(x => x.load1 / cores)) * 1.1, rfmt: v => v.toFixed(v < 10 ? 1 : 0) + '×' });
  $('#lgCpu').innerHTML = legend([['CPU % (left)', '#6fb0f0'], ['load per core (right)', '#ec8a5a']]);
  chart($('#chMem'), [{ l: 'used', c: '#b691e8', v: H.map(x => x.mem_used) }, { l: 'swap', c: '#ec8a5a', v: H.map(x => x.swap) }],
    { fmt: v => B(v, 0), max: Math.max(s.mem_total, ...H.map(x => x.swap)) * 1.05, lines: [{ v: s.mem_total, c: css('--chart-cursor'), l: 'physical' }] });
  $('#lgMem').innerHTML = legend([['used', '#b691e8'], ['swap', '#ec8a5a']]);
  chart($('#chIo'), [{ l: 'read', c: '#5fc9c0', v: H.map(x => x.read_bps) }, { l: 'write', c: '#e3b24f', v: H.map(x => x.write_bps) }], { fmt: v => B(v, 0), tipFmt: v => BS(v) });
  $('#lgIo').innerHTML = legend([['read', '#5fc9c0'], ['write', '#e3b24f']]);
  const dv = H.map(x => x.disk_free), dmin = Math.min(...dv), dmax = Math.max(...dv), pad = Math.max((dmax - dmin) * .5, 2e9);
  chart($('#chDisk'), [{ l: 'free', c: '#79c37d', v: dv }], { fmt: v => B(v, 0), tipFmt: v => B(v, 2), min: Math.max(0, dmin - pad), max: dmax + pad });
  const bph = S.forecast?.bytes_per_hour || 0;
  $('#lgDisk').innerHTML = `<span class="mono ${bph > 1e9 ? 'warn' : ''}">${bph > 0 ? '−' : '+'}${B(Math.abs(bph))}/h</span>`;

  const tot = {};
  H.forEach(x => x.ws.forEach(([l, c]) => { tot[l] = (tot[l] || 0) + c; }));
  const top = Object.entries(tot).sort((a, b) => b[1] - a[1]).slice(0, 6).map(e => e[0]);
  chart($('#chWs'), top.map((l, i) => ({ l, c: WS_COLORS[i], v: H.map(x => (x.ws.find(w => w[0] === l) || [0, 0])[1]), fill: false })), { fmt: v => v.toFixed(0) + '%' });
  $('#lgWs').innerHTML = legend(top.map((l, i) => [l.length > 30 ? l.slice(0, 29) + '…' : l, WS_COLORS[i]]));

  $('#findings').innerHTML = findingsHtml(findings()) + watchLink(S);
  const gs = consumers(S).sort((a, b) => b.cpu_pct - a.cpu_pct);
  const gm = Math.max(1, ...gs.map(g => g.cpu_pct)), mm = Math.max(1, ...gs.map(g => g.mem_bytes));
  $('#groups').innerHTML = th([{ l: 'Group' }, { l: 'Kind', c: 'c-lg', w: '92px' }, { l: 'Procs', n: 1, c: 'c-md', w: '60px' }, { l: 'CPU', n: 1, w: '140px' }, { l: 'Memory', n: 1, w: '140px' }]) +
    gs.map(g => `<tr class="click" role="button" tabindex="0" data-d="group" data-dk="${esc(g.key)}"><td title="${esc(g.label)}">${KIND_ICON[g.kind] || ''} ${esc(g.short)}${g.protected ? ' <span class="chip">protected</span>' : ''}</td><td class="c-lg dim">${esc(g.kind)}</td><td class="n c-md">${g.count}</td><td class="n">${meter(g.cpu_pct, gm)}${esc(coresTxt(g.cpu_pct))}</td><td class="n">${meter(g.mem_bytes, mm, 'var(--c2)')}${B(g.mem_bytes)}</td></tr>`).join('');
}

// "All observations (11 · 2 dismissed) →": the engine's findings, with the
// dismissed ones, in one drawer.
export function watchLink(S) {
  const c = S.finding_counts;
  if (!c) return '';
  const open = c.high + c.medium + c.low + c.info;
  if (!open && !c.dismissed) return '';
  return `<div class="more"><a class="lnk" role="button" tabindex="0" data-d="findings" data-dk="">All observations (${open}${c.dismissed ? ` · ${c.dismissed} dismissed` : ''}) →</a></div>`;
}
