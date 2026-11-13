// Formatting and small DOM helpers.

export const $ = s => document.querySelector(s);
export const $$ = s => [...document.querySelectorAll(s)];

export const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

export function B(n, d = 1) {
  n = Number(n) || 0;
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (Math.abs(n) >= 1000 && i < u.length - 1) { n /= 1000; i++; }
  return (i === 0 ? n.toFixed(0) : n.toFixed(Math.abs(n) >= 100 ? 0 : d)) + ' ' + u[i];
}
// A colour token's current value (canvas drawing cannot use var()).
export const css = name => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
export const BS = n => B(n) + '/s';
export const P = (n, d = 0) => (Number(n) || 0).toFixed(d) + '%';

export function ago(t) {
  if (!t) return '—';
  const s = Math.max(0, Date.now() / 1000 - t);
  if (s < 60) return Math.round(s) + 's';
  if (s < 3600) return Math.round(s / 60) + 'm';
  if (s < 86400) return (s / 3600).toFixed(s < 36000 ? 1 : 0) + 'h';
  return Math.round(s / 86400) + 'd';
}

// How long ago, for a table cell: the short age, with the date as a tooltip.
export const agoCell = t => t ? `<span title="${esc(new Date(t * 1000).toLocaleString())}">${ago(t)}</span>` : '<span class="dim">—</span>';
// A row's own action button (`data-da` is run by detail.js). `off`: why it
// cannot run now, shown as the tooltip of a disabled button.
export const rowBtn = (label, name, arg, { danger = false, off = '', title = '' } = {}) =>
  `<button class="btn sm${danger ? ' danger' : ''}" data-da="${esc(name)}" data-arg="${esc(arg)}" ${off ? `disabled title="${esc(off)}"` : title ? `title="${esc(title)}"` : ''}>${esc(label)}</button>`;

export function dur(s) {
  if (s < 60) return s + 's';
  if (s < 3600) return Math.floor(s / 60) + 'm';
  if (s < 86400) return Math.floor(s / 3600) + 'h' + String(Math.floor(s % 3600 / 60)).padStart(2, '0');
  return Math.floor(s / 86400) + 'd' + Math.floor(s % 86400 / 3600) + 'h';
}

export const clock = t => new Date(t * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });

let HOME = '';
export const setHome = h => { HOME = (h || '').replace(/\/$/, ''); };
export const tilde = p => HOME && p?.startsWith(HOME + '/') ? '~' + p.slice(HOME.length) : (p || '');

export function toast(msg, err) {
  const t = $('#toast');
  t.textContent = msg;
  t.className = 'toast' + (err ? ' err' : '');
  t.style.display = 'block';
  clearTimeout(toast.h);
  toast.h = setTimeout(() => { t.style.display = 'none'; }, err ? 8000 : 4500);
}
export function hideToast() {
  clearTimeout(toast.h);
  $('#toast').style.display = 'none';
}

export function sortRows(rows, [k, dir]) {
  return rows.sort((a, b) => {
    const x = typeof k === 'function' ? k(a) : a[k], y = typeof k === 'function' ? k(b) : b[k];
    if (typeof x === 'string' || typeof y === 'string') return String(x ?? '').localeCompare(String(y ?? '')) * dir;
    return ((x ?? 0) - (y ?? 0)) * dir;
  });
}

// A table head with a <colgroup>: tables use a fixed layout, so columns with
// a width `w` keep it, the others share what is left (with an ellipsis and
// the full text in a tooltip), and a class `c` such as c-md or c-lg hides the
// column on narrow windows.
export function th(cols, sort, id) {
  const cg = '<colgroup>' + cols.map(c => `<col class="${c.c || ''}"${c.check ? ' style="width:30px"' : c.w ? ` style="width:${c.w}"` : ''}>`).join('') + '</colgroup>';
  return cg + '<tr>' + cols.map(c => {
    if (c.check) return `<th><input type="checkbox" data-all="${id}" aria-label="Select all"></th>`;
    const on = sort && sort[0] === c.k;
    return `<th class="${c.n ? 'n ' : ''}${c.k ? 's ' : ''}${c.c || ''}" ${c.k ? `data-sort="${id}:${c.k}"` : ''}>${c.l}${on ? ` <span class="arr">${sort[1] < 0 ? '↓' : '↑'}</span>` : ''}</th>`;
  }).join('') + '</tr>';
}

export const meter = (v, max, color) => `<span class="meter"><i style="width:${Math.min(100, max ? v / max * 100 : 0)}%;background:${color || 'var(--c1)'}"></i></span>`;
export const kindChip = k => `<span class="chip ${esc(k)}">${esc(k)}</span>`;
export const legend = items => items.map(([l, c]) => `<span><i style="background:${c}"></i>${esc(l)}</span>`).join('');
