// The Disk view: volume, worktrees, build caches, package stores.
// Every removal goes plan → review → execute the approved items.
import { st, hooks } from '../store.js';
import { $, $$, esc, B, ago, agoCell, rowBtn, tilde, sortRows, th, meter } from '../util.js';
import { planThenExecute } from '../dialogs.js';

const WT_F = { all: () => true, sweep: w => w.sweepable, removable: w => w.removable && !w.in_use, dirty: w => w.dirty_files > 0, inuse: w => w.in_use };
const WT_L = { all: 'All', sweep: 'Idle agents', removable: 'Removable', dirty: 'Dirty', inuse: 'In use' };
const BC_F = { stale: i => i.stale, recent: i => !i.stale && !i.in_use, inuse: i => i.in_use, all: () => true };
const BC_L = { stale: 'Stale', recent: 'Recent', inuse: 'In use', all: 'All' };
const KN = { cargo_target: 'rust target', node_modules: 'node_modules', next_cache: '.next', xcode_derived_data: 'DerivedData' };

export function renderDisk() {
  const S = st.S, ui = st.ui, s = S.snapshot, m = S.diskmap, used = s.disk_total - s.disk_free;
  const d = st.ui.detail;
  $('#dVol').textContent = `${B(used)} used · ${B(s.disk_free)} free · ${B(s.disk_total)}`;
  const mainCaches = (S.scan?.items || []).filter(i => !i.in_worktree).reduce((a, i) => a + i.bytes, 0);
  const parts = [['Worktrees', m?.worktree_bytes || 0, '#6fb0f0', 'worktrees'], ['Build caches in main repos', mainCaches, '#e3b24f', 'caches'],
    ...(m?.categories || []).slice(0, 5).map((c, i) => [c.name, c.bytes, ['#b691e8', '#5fc9c0', '#ec8a5a', '#9aa3a9', '#c4a7e7'][i], 'cat:' + c.key])];
  const known = parts.reduce((a, p) => a + p[1], 0);
  parts.push(['Everything else', Math.max(0, used - known), 'var(--disk-other)', 'other'], ['Free', s.disk_free, 'var(--disk-free)', 'free']);
  $('#dStack').innerHTML = parts.map(([l, b, c, k]) => `<i role="button" tabindex="0" data-d="seg" data-dk="${esc(k)}" title="${esc(l)} ${B(b)}" style="width:${s.disk_total ? b / s.disk_total * 100 : 0}%;background:${c}"></i>`).join('');
  $('#dLegend').innerHTML = parts.map(([l, b, c, k]) => `<span class="lg-i" role="button" tabindex="0" data-d="seg" data-dk="${esc(k)}"><i style="background:${c}"></i>${esc(l)} <span class="mono">${B(b)}</span></span>`).join('');

  const wts = m?.worktrees || [];
  for (const p of ui.wtSel) if (!wts.some(w => w.path === p)) ui.wtSel.delete(p);
  $$('#wtSeg button').forEach(b => { b.classList.toggle('on', b.dataset.f === ui.wtF); b.textContent = `${WT_L[b.dataset.f]} ${wts.filter(WT_F[b.dataset.f]).length}`; });
  // "Idle" sorts by the last activity itself, so hours order within a day.
  const now = Date.now() / 1000, idle = w => w.last_activity_unix ? now - w.last_activity_unix : -1;
  const rows = sortRows(wts.filter(WT_F[ui.wtF]), [ui.wtSort[0] === 'idle_days' ? idle : ui.wtSort[0], ui.wtSort[1]]);
  const wm = Math.max(1, ...wts.map(w => w.bytes));
  $('#wts').innerHTML = th([{ check: 1 }, { l: 'Repo', k: 'repo_name', c: 'c-md' }, { l: 'Worktree', k: 'name' }, { l: 'Branch', k: 'branch', c: 'c-lg' }, { l: 'Size', k: 'bytes', n: 1, w: '112px' }, { l: 'Idle', k: 'idle_days', n: 1, w: '56px' }, { l: 'Dirty', k: 'dirty_files', n: 1, w: '52px', c: 'c-md' }, { l: 'Verdict', w: '30%' }, { l: '', w: '84px' }], ui.wtSort, 'wt') +
    (rows.map(w => {
      const can = !w.in_use && !w.locked;
      const cls = w.in_use ? 'dim' : w.sweepable ? 'warn' : '';
      return `<tr class="click ${d?.type === 'wt' && d.key === w.path ? 'sel' : ''}" role="button" tabindex="0" data-d="wt" data-dk="${esc(w.path)}"><td><input type="checkbox" data-wtsel="${esc(w.path)}" aria-label="Select ${esc(w.name)}" ${ui.wtSel.has(w.path) ? 'checked' : ''} ${can ? '' : 'disabled'}></td>
        <td class="c-md" title="${esc(w.repo_name)}">${esc(w.repo_name)}</td><td title="${esc(w.path)}">${esc(w.name)}${w.agent ? ' <span class="chip agent">agent</span>' : ''}</td>
        <td class="dim mono c-lg" title="${esc(w.branch || '')}">${esc(w.detached ? '(detached)' : w.branch || '—')}${w.merged ? ' <span class="chip ok">merged</span>' : ''}</td>
        <td class="n">${meter(w.bytes, wm)}${B(w.bytes)}</td><td class="n ${w.sweepable ? 'warn' : ''}">${w.missing || w.activity_unknown ? '<span class="dim">—</span>' : agoCell(w.last_activity_unix)}</td><td class="n c-md ${w.dirty_files ? 'warn' : 'dim'}">${w.dirty_files}</td>
        <td class="wrap ${cls}">${esc(w.verdict)}</td>
        <td class="n">${rowBtn('Remove…', 'remove-wt', w.path, { danger: true, off: w.in_use ? 'Something is running in it' : w.locked ? 'Locked in git' : '', title: 'Review, then remove this worktree. The branch is kept and uncommitted changes are saved first.' })}</td></tr>`;
    }).join('') || `<tr><td colspan="9" class="empty">${S.scanning && !m ? 'Scanning worktrees… this can take a few minutes on a busy machine.' : 'None in this filter.'}</td></tr>`);
  const selB = [...ui.wtSel].reduce((a, p) => a + (wts.find(w => w.path === p)?.bytes || 0), 0);
  $('#wtBulk').classList.toggle('on', ui.wtSel.size > 0);
  $('#wtBulkN').textContent = `${ui.wtSel.size} selected · ${B(selB)}`;
  const nSweep = wts.filter(w => w.sweepable).length;
  $('#wtSweep').textContent = nSweep ? `Sweep ${nSweep} idle · ${B(m.sweepable_bytes)}…` : 'Nothing to sweep';
  $('#wtSweep').disabled = !nSweep;

  const items = S.scan?.items || [];
  for (const p of ui.bcSel) if (!items.some(i => i.path === p)) ui.bcSel.delete(p);
  $$('#bcSeg button').forEach(b => { b.classList.toggle('on', b.dataset.f === ui.bcF); b.textContent = `${BC_L[b.dataset.f]} ${B(items.filter(BC_F[b.dataset.f]).reduce((a, i) => a + i.bytes, 0), 0)}`; });
  const staleN = items.filter(i => i.stale).length;
  $('#bcStale').disabled = !staleN;
  $('#bcStale').title = staleN ? `Review ${staleN} stale cache(s) and clear the ones you approve` : 'No stale caches';
  $('#bcs').innerHTML = th([{ check: 1 }, { l: 'Project' }, { l: 'Kind', w: '92px', c: 'c-lg' }, { l: 'Size', n: 1, w: '76px' }, { l: 'Last used', n: 1, w: '84px' }, { l: '', w: '70px' }], null, 'bc') +
    (items.filter(BC_F[ui.bcF]).map(i => `<tr class="click ${d?.type === 'cache' && d.key === i.path ? 'sel' : ''}" role="button" tabindex="0" data-d="cache" data-dk="${esc(i.path)}"><td><input type="checkbox" data-bcsel="${esc(i.path)}" aria-label="Select" ${ui.bcSel.has(i.path) ? 'checked' : ''} ${i.in_use ? 'disabled' : ''}></td>
      <td title="${esc(i.path)}">${esc(tilde(i.project))}</td><td class="dim c-lg">${KN[i.kind] || esc(i.kind)}</td>
      <td class="n">${B(i.bytes)}</td><td class="n ${i.in_use ? 'ok' : i.stale ? 'warn' : ''}" title="${esc(i.last_used_unix ? new Date(i.last_used_unix * 1000).toLocaleString() : '')}">${i.in_use ? 'in use' : ago(i.last_used_unix) + ' ago'}</td>
      <td class="n">${rowBtn('Clear…', 'clean-one', i.path, { danger: true, off: i.in_use ? 'A process is running in this project' : '', title: 'Review, then delete this cache. The next build recreates it.' })}</td></tr>`).join('') || '<tr><td colspan="6" class="empty">None in this filter.</td></tr>');
  const bsel = [...ui.bcSel].reduce((a, p) => a + (items.find(i => i.path === p)?.bytes || 0), 0);
  $('#bcBulk').classList.toggle('on', ui.bcSel.size > 0);
  $('#bcBulkN').textContent = `${ui.bcSel.size} selected · ${B(bsel)}`;

  $('#cats').innerHTML = th([{ l: 'Store' }, { l: 'Size', n: 1, w: '76px' }, { l: 'How to reclaim', c: 'c-lg' }, { l: '', w: '70px' }]) +
    ((m?.categories || []).map(c => `<tr class="click ${d?.type === 'cat' && d.key === c.key ? 'sel' : ''}" role="button" tabindex="0" data-d="cat" data-dk="${esc(c.key)}"><td title="${esc(c.path)}">${esc(c.name)}${c.prunable ? ' <span class="chip ok">prunable</span>' : ''}</td><td class="n">${B(c.bytes)}</td><td class="dim mono c-lg" title="${esc(c.hint)}">${esc(c.hint)}</td><td class="n">${c.prunable ? rowBtn('Prune…', 'prune', c.key, { title: `Runs ${c.hint}` }) : ''}</td></tr>`).join('') || `<tr><td colspan="4" class="empty">${S.scanning ? 'Scanning…' : 'No data yet.'}</td></tr>`);
}

export async function removeSelectedWorktrees() {
  const ui = st.ui, paths = [...ui.wtSel], allow_dirty = $('#wtBackup').checked;
  const res = await planThenExecute({
    request: { kind: 'worktrees', paths, allow_dirty },
    batch: 'worktree',
    title: `Remove ${paths.length} worktree(s)?`,
    verb: 'Remove',
    intro: `Runs <span class="mono">git worktree remove</span>. Branches are kept, and a detached HEAD gets a <span class="mono">backup/&lt;name&gt;</span> branch. ${allow_dirty ? 'Uncommitted changes and ignored files that are not caches are saved first.' : 'Worktrees with uncommitted changes are left alone (tick “save a patch” to include them).'}`,
  });
  if (res) ui.wtSel.clear();
  hooks.render();
}

export function sweep() {
  return planThenExecute({
    request: { kind: 'sweep' },
    batch: 'sweep',
    title: 'Sweep idle agent worktrees?',
    verb: 'Sweep',
    intro: `Agent worktrees idle ${st.S.config.worktree_idle_days}+ days with no running process. Branches are kept; uncommitted changes and ignored files are saved first.`,
    empty: 'Nothing to sweep.',
  });
}

export async function clearSelectedCaches() {
  const ui = st.ui, paths = [...ui.bcSel];
  const res = await planThenExecute({
    request: { kind: 'clean', paths },
    batch: 'clean',
    title: `Clear ${paths.length} build cache(s)?`,
    verb: 'Clear',
    intro: 'Deletes these folders. They are rebuilt on the next build.',
  });
  if (res) ui.bcSel.clear();
  hooks.render();
}

export function clearStale() {
  return planThenExecute({
    request: { kind: 'stale' },
    batch: 'clean',
    title: 'Clear stale build caches?',
    verb: 'Clear',
    intro: 'Caches unused within the stale window, with no process running in the project. Staleness is checked again before each one is deleted.',
    empty: 'No stale caches.',
  });
}
