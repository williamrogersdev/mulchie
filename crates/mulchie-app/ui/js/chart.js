// Canvas line charts, sparklines and the panel's mini charts.
import { st } from './store.js';
import { esc, clock, css } from './util.js';

export function chart(host, series, opt = {}) {
  if (!host.querySelector('canvas')) {
    host.innerHTML = '<canvas></canvas><div class="tip"></div>';
    const c = host.querySelector('canvas');
    c.addEventListener('mousemove', e => { host._hx = e.offsetX; drawChart(host); });
    c.addEventListener('mouseleave', () => { host._hx = null; drawChart(host); });
  }
  host._series = series; host._opt = opt;
  drawChart(host);
}

function drawChart(host) {
  const H = st.H;
  const c = host.querySelector('canvas'), tip = host.querySelector('.tip');
  const series = host._series || [], opt = host._opt || {};
  const dpr = devicePixelRatio || 1, W = c.clientWidth, Hh = c.clientHeight;
  if (!W || !Hh) return;
  c.width = W * dpr; c.height = Hh * dpr;
  const g = c.getContext('2d');
  g.scale(dpr, dpr);
  const hasR = series.some(s => s.r);
  const padL = 46, padR = hasR ? 40 : 0, padB = 14, padT = 4;
  const w = W - padL - padR, h = Hh - padB - padT;
  const n = Math.max(2, ...series.map(s => s.v.length));
  const vals = series.filter(s => !s.r).flatMap(s => s.v.filter(Number.isFinite));
  const rvals = series.filter(s => s.r).flatMap(s => s.v.filter(Number.isFinite));
  const max = opt.max ?? Math.max(1e-9, ...vals) * 1.12;
  const rmax = opt.rmax ?? Math.max(1e-9, ...rvals) * 1.12;
  const min = opt.min ?? 0;
  const X = i => padL + i / (n - 1) * w, Y = v => padT + h - (v - min) / (max - min || 1) * h;
  const YR = v => padT + h - v / rmax * h;
  g.strokeStyle = css('--chart-grid'); g.fillStyle = css('--chart-tx'); g.font = '10px ui-monospace, Menlo'; g.lineWidth = 1;
  const fmt = opt.fmt || (x => x.toFixed(0));
  for (let i = 0; i <= 3; i++) {
    const v = min + (max - min) * i / 3, y = Math.round(Y(v)) + .5;
    g.beginPath(); g.moveTo(padL, y); g.lineTo(W - padR, y); g.stroke();
    g.textAlign = 'right'; g.fillText(fmt(v), padL - 5, y + 3);
    if (hasR) { g.textAlign = 'left'; g.fillText((opt.rfmt || (x => x.toFixed(0)))(rmax * i / 3), W - padR + 5, y + 3); }
  }
  if (H.length > 1) {
    g.textAlign = 'left'; g.fillText('−' + Math.max(1, Math.round((H[H.length - 1].t - H[0].t) / 60)) + 'm', padL, Hh - 2);
    g.textAlign = 'right'; g.fillText('now', W - padR, Hh - 2);
  }
  for (const l of opt.lines || []) {
    if (l.v > max || l.v < min) continue;
    g.setLineDash([3, 3]); g.strokeStyle = l.c; g.beginPath(); g.moveTo(padL, Y(l.v)); g.lineTo(W - padR, Y(l.v)); g.stroke(); g.setLineDash([]);
    if (l.l) { g.fillStyle = l.c; g.textAlign = 'left'; g.fillText(l.l, padL + 4, Y(l.v) - 3); }
  }
  for (const s of series) {
    if (!s.v.length) continue;
    const off = n - s.v.length;
    g.beginPath();
    const YY = s.r ? YR : Y;
    s.v.forEach((v, i) => { const x = X(i + off), y = YY(Number.isFinite(v) ? v : 0); i ? g.lineTo(x, y) : g.moveTo(x, y); });
    g.strokeStyle = s.c; g.lineWidth = 1.4; g.stroke();
    if (s.fill !== false && !s.r) {
      g.lineTo(X(n - 1), Y(min)); g.lineTo(X(off), Y(min)); g.closePath();
      g.globalAlpha = .1; g.fillStyle = s.c; g.fill(); g.globalAlpha = 1;
    }
  }
  if (host._hx != null && tip) {
    const i = Math.round((host._hx - padL) / w * (n - 1));
    if (i >= 0 && i < n) {
      g.strokeStyle = css('--chart-cursor'); g.beginPath(); g.moveTo(Math.round(X(i)) + .5, padT); g.lineTo(Math.round(X(i)) + .5, padT + h); g.stroke();
      const t = H[H.length - n + i]?.t;
      tip.innerHTML = (t ? `<div class="dim">${clock(t)}</div>` : '') + series.map(s => {
        const v = s.v[i - (n - s.v.length)];
        return `<div><span style="color:${s.c}">■</span> ${esc(s.l)} ${v == null ? '—' : (s.fmt || opt.tipFmt || fmt)(v)}</div>`;
      }).join('');
      tip.style.display = 'block';
      const tw = tip.offsetWidth, tx = X(i) + 10;
      tip.style.left = (tx + tw > W - padR ? X(i) - tw - 10 : tx) + 'px';
    }
  } else if (tip) tip.style.display = 'none';
}

export function sparkline(c, v) {
  const dpr = devicePixelRatio || 1, W = 84, Hh = 18;
  c.width = W * dpr; c.height = Hh * dpr; c.style.width = W + 'px'; c.style.height = Hh + 'px';
  const g = c.getContext('2d'); g.scale(dpr, dpr);
  const m = Math.max(1, ...v);
  g.beginPath();
  v.forEach((x, i) => { const px = i / Math.max(1, v.length - 1) * W, py = Hh - 1 - x / m * (Hh - 2); i ? g.lineTo(px, py) : g.moveTo(px, py); });
  g.strokeStyle = '#6fb0f0'; g.lineWidth = 1; g.stroke();
}

export function drawMini(c, v, color, o = {}) {
  const dpr = devicePixelRatio || 1, W = c.clientWidth, Hh = c.clientHeight;
  if (!W || !v.length) return;
  c.width = W * dpr; c.height = Hh * dpr;
  const g = c.getContext('2d'); g.scale(dpr, dpr);
  const mx = o.max ?? Math.max(1, ...v) * 1.1, mn = o.min ?? 0;
  g.beginPath();
  v.forEach((x, i) => { const px = i / Math.max(1, v.length - 1) * W, py = Hh - (x - mn) / (mx - mn || 1) * (Hh - 2) - 1; i ? g.lineTo(px, py) : g.moveTo(px, py); });
  g.strokeStyle = color; g.lineWidth = 1.2; g.stroke();
  g.lineTo(W, Hh); g.lineTo(0, Hh); g.closePath(); g.globalAlpha = .12; g.fillStyle = color; g.fill(); g.globalAlpha = 1;
}
