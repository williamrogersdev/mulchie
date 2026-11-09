// The tray panel.
import { st } from '../store.js';
import { $, esc, B, P, clock } from '../util.js';
import { drawMini } from '../chart.js';
import { findings, findingsHtml, notices, noticesHtml, modeHtml, modeTitle } from '../findings.js';

export function renderPanel() {
  const S = st.S;
  const mode = $('#pMode');
  mode.innerHTML = modeHtml();
  mode.title = modeTitle();
  $('#pNotices').innerHTML = noticesHtml(notices(), true);
  if (!S) return;
  const s = S.snapshot, hc = st.conn?.mode === 'disconnected' ? 'stale' : S.health;
  const el = $('#pHealth'); el.querySelector('.dot').className = 'dot ' + hc; el.querySelector('span:last-child').textContent = hc;
  $('#pTime').textContent = s.at_unix ? clock(s.at_unix) : '';
  $('#pCpu').textContent = `${P(s.cpu_pct)} · ${s.load[0].toFixed(0)}`;
  $('#pCores').textContent = `${s.cpu_count} cores`;
  $('#pMem').textContent = B(s.mem_total - s.mem_available);
  $('#pSwap').textContent = `swap ${B(s.swap_used, 0)}`;
  $('#pDisk').textContent = B(s.disk_free);
  $('#pFc').textContent = S.forecast?.hours_to_full != null ? `full ~${S.forecast.hours_to_full.toFixed(0)}h` : `of ${B(s.disk_total, 0)}`;
  $('#pAg').textContent = `${s.agent_count} · ${s.build_count}`;
  $('#pSlots').textContent = `${S.slots.active.length}/${S.slots.capacity} slots`;
  const tail = st.H.slice(-100), dv = tail.map(x => x.disk_free);
  drawMini($('#pCpuC'), tail.map(x => x.cpu), '#6fb0f0', { max: 100 });
  drawMini($('#pMemC'), tail.map(x => x.mem_used), '#b691e8', { max: s.mem_total });
  drawMini($('#pDiskC'), dv, '#79c37d', { min: Math.min(...dv) - 2e9, max: Math.max(...dv) + 2e9 });
  drawMini($('#pIoC'), tail.map(x => x.read_bps + x.write_bps), '#5fc9c0');
  $('#pFind').innerHTML = findingsHtml(findings(), true);
  $('#pTop').innerHTML = s.groups.slice(0, 6).map(g => `<div class="r" data-gproc="${esc(g.group)}"><span class="nm">${esc(g.group)}</span><span class="mono">${P(g.cpu_pct)} · ${B(g.mem_bytes, 0)}</span></div>`).join('');
}
