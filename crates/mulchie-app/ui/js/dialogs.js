// The modal: plain confirmations, plan review, and per-item results.
import { invoke } from './ipc.js';
import { hooks } from './store.js';
import { $, esc, B, tilde, toast, hideToast } from './util.js';

export const modalOpen = () => $('#modal').classList.contains('on');

let closeModal = null;

const FOCUSABLE = 'button:not([disabled]),input:not([disabled]),select:not([disabled]),textarea:not([disabled]),a[href],[tabindex]:not([tabindex="-1"])';
const focusables = () => [...$('#mBox').querySelectorAll(FOCUSABLE)].filter(el => el.offsetParent !== null);

// Keep Tab and Shift-Tab inside the open dialog.
function trapTab(e) {
  if (e.key !== 'Tab' || !modalOpen()) return;
  const f = focusables();
  if (!f.length) return;
  const first = f[0], last = f[f.length - 1];
  if (e.shiftKey && (document.activeElement === first || !$('#mBox').contains(document.activeElement))) { e.preventDefault(); last.focus(); }
  else if (!e.shiftKey && (document.activeElement === last || !$('#mBox').contains(document.activeElement))) { e.preventDefault(); first.focus(); }
}
document.addEventListener('keydown', trapTab);

// Show the modal (a dialog: focus moves in, stays in, and returns to what
// opened it). `setup(body, yesButton)` may wire controls inside the body.
// Resolves true on confirm, false on cancel or Escape.
export function modal({ title, html, yes = 'Confirm', danger = true, wide = false, cancel = 'Cancel', setup }) {
  if (closeModal) closeModal(false);
  const opener = document.activeElement;
  return new Promise(res => {
    const yesB = $('#mYes'), noB = $('#mNo');
    $('#mBox').classList.toggle('wide', wide);
    $('#mTitle').textContent = title;
    $('#mBody').innerHTML = html;
    yesB.textContent = yes;
    yesB.className = 'btn ' + (danger ? 'danger' : 'pri');
    yesB.disabled = false;
    yesB.style.display = yes ? '' : 'none';
    noB.textContent = cancel;
    $('#modal').classList.add('on');
    const done = v => {
      $('#modal').classList.remove('on');
      yesB.onclick = noB.onclick = null;
      $('#mBody').onchange = null;
      closeModal = null;
      if (opener && opener.isConnected && typeof opener.focus === 'function') opener.focus({ preventScroll: true });
      res(v);
    };
    closeModal = done;
    yesB.onclick = () => done(true);
    noB.onclick = () => done(false);
    if (setup) setup($('#mBody'), yesB);
    // A destructive action starts on Cancel; otherwise on the first control
    // in the body, or the action button.
    const start = danger || !yes ? noB : ($('#mBody').querySelector(FOCUSABLE) || yesB);
    start.focus({ preventScroll: true });
  });
}

export const confirmBox = (title, html, yes = 'Confirm', danger = true) => modal({ title, html, yes, danger });
export const dismissModal = () => { if (closeModal) closeModal(false); };

const REASONS = {
  not_same_process: 'No longer the same process (another start time)',
  not_absolute: 'Not an absolute path',
  not_found: 'No longer exists',
  symlink: 'Is a symbolic link',
  not_canonical: 'Path is not canonical',
  outside_scan_roots: 'Outside the scan roots in Settings',
  not_recognised_cache: 'Not a recognised build cache',
  missing_cachedir_tag: 'No CACHEDIR.TAG, so not treated as a cache',
  node_modules_disabled: 'node_modules clearing is off in Settings',
  xcode_disabled: 'Xcode DerivedData clearing is off in Settings',
  busy: 'In use by a running process',
  not_stale: 'Used recently',
  not_a_worktree: 'Not a git worktree',
  missing_expected_head: 'No expected commit given',
  head_moved: 'New commits since the plan was made',
  locked: 'Locked in git',
  not_agent: 'Not an agent worktree',
  not_idle: 'Not idle long enough',
  dirty: 'Has uncommitted changes',
  has_submodules: 'Has submodules',
  ignored_too_large: 'Too many ignored files to back up',
  backup_failed: 'Backup failed',
  changed_during_backup: 'Changed during backup',
  not_prunable: 'Git does not mark it prunable',
  prune_deferred: 'Prune deferred',
  git_failed: 'git failed',
  remove_failed: 'Removal failed',
  protected_process: 'Protected process',
  foreign_process: "Another user's process",
  process_gone: 'Process exited',
  start_time_mismatch: 'Process id was reused',
  signal_failed: 'Signal failed',
  unknown_target: 'Unknown target',
  no_prune_command: 'No safe prune command',
  tool_not_found: 'Tool not installed',
  timed_out: 'Timed out',
  command_failed: 'Command failed',
};
export const reasonText = r => REASONS[r] || String(r || '').replace(/_/g, ' ');

const itemName = i => i.repo_name ? `${i.repo_name} › ${i.name}` : i.name;

function planItemHtml(i, idx) {
  const meta = [];
  if (i.branch) meta.push(`<span class="chip">${esc(i.branch)}</span>`);
  if (i.idle_days != null) meta.push(`idle ${i.idle_days}d`);
  if (i.dirty_files) meta.push(`<span class="warn">${i.dirty_files} uncommitted file(s)</span>`);
  if (i.ignored_files) meta.push(`${i.ignored_files} ignored item(s), ${B(i.ignored_bytes)}`);
  if (i.ready) {
    if (i.would_backup?.length) meta.push(`<span class="bk">saves first: ${i.would_backup.map(esc).join('; ')}</span>`);
    else if (i.request?.action === 'remove_worktree') meta.push('<span class="dim">nothing to back up</span>');
    if (i.verdict) meta.push(`<span class="dim">${esc(i.verdict)}</span>`);
  } else {
    meta.push(`<span class="why">${esc(reasonText(i.reason))}</span>${i.verdict ? ` <span class="dim">· ${esc(i.verdict)}</span>` : ''}`);
  }
  return `<div class="pi ${i.ready ? '' : 'off'}">
    ${i.ready ? `<input type="checkbox" data-pi="${idx}" checked>` : '<span></span>'}
    <div><div class="nm" title="${esc(i.target)}">${esc(itemName(i))}</div><div class="path" title="${esc(i.target)}">${esc(tilde(i.target))}</div></div>
    <div class="sz">${i.bytes ? B(i.bytes) : ''}</div>
    <div class="meta">${meta.join(' ')}</div></div>`;
}

// Show a plan; resolve to the requests of the ready items left checked, or
// null when cancelled.
export async function reviewPlan(plan, { title, verb, intro }) {
  const ready = plan.items.map((it, idx) => [it, idx]).filter(([i]) => i.ready);
  const not = plan.items.map((it, idx) => [it, idx]).filter(([i]) => !i.ready);
  const backups = ready.filter(([i]) => i.would_backup?.length).length;
  const html = `${intro ? `<div>${intro}</div>` : ''}
    <div class="sum"><span>Ready <b>${ready.length}</b> · <b>${B(plan.ready_bytes)}</b></span>${backups ? `<span>Backed up first <b>${backups}</b></span>` : ''}${not.length ? `<span>Will not be touched <b>${not.length}</b></span>` : ''}</div>
    <div class="plan">
      ${ready.length ? `<h4>Will ${esc(verb.toLowerCase())}</h4>${ready.map(([i, idx]) => planItemHtml(i, idx)).join('')}` : ''}
      ${not.length ? `<h4>Not ready, left alone</h4>${not.map(([i, idx]) => planItemHtml(i, idx)).join('')}` : ''}
    </div>
    <div class="note">Each item is checked again just before it runs. Backups go to the backups folder in Mulchie's config directory.</div>`;
  const picked = () => [...$('#mBody').querySelectorAll('input[data-pi]')].filter(x => x.checked).map(x => plan.items[+x.dataset.pi]);
  const ok = await modal({
    title, html, wide: true, yes: ready.length ? verb : '', cancel: ready.length ? 'Cancel' : 'Close',
    setup: (body, yesB) => {
      const upd = () => {
        const p = picked();
        yesB.disabled = !p.length;
        yesB.textContent = p.length ? `${verb} ${p.length} · ${B(p.reduce((a, i) => a + i.bytes, 0))}` : verb;
      };
      body.onchange = upd;
      upd();
    },
  });
  if (!ok) return null;
  return picked().map(i => i.request);
}

// Show per-item results: done with freed bytes and backups, skipped with why.
export function showResults(results, title) {
  const done = results.filter(r => r.status === 'done'), skipped = results.filter(r => r.status === 'skipped');
  const freed = done.reduce((a, r) => a + (r.freed_bytes || 0), 0);
  const KIND = { patch: 'patch', ignored_archive: 'ignored files', branch: 'branch' };
  const row = r => {
    const meta = r.status === 'done'
      ? [r.freed_bytes ? `freed ${B(r.freed_bytes)}` : 'done', ...(r.backups || []).map(b => b.kind === 'branch'
        ? `<span class="bk">branch <span class="mono">${esc(b.location)}</span></span>`
        : `<span class="bk">${KIND[b.kind] || esc(b.kind)} saved to <span class="mono">${esc(tilde(b.location))}</span> <button class="btn sm" data-reveal="${esc(b.location)}">Show</button></span>`)]
      : [`<span class="why">${esc(reasonText(r.reason))}</span>`, r.detail ? `<span class="dim">${esc(r.detail)}</span>` : ''];
    return `<div class="pi ${r.status === 'done' ? '' : 'off'}"><span class="${r.status === 'done' ? 'ok' : 'warn'}">${r.status === 'done' ? '✓' : '–'}</span>
      <div><div class="path" title="${esc(r.target)}">${esc(tilde(r.target))}</div></div><div class="sz">${r.freed_bytes ? B(r.freed_bytes) : ''}</div>
      <div class="meta">${meta.join(' ')}</div></div>`;
  };
  const html = `<div class="sum"><span>Done <b>${done.length}</b></span><span>Freed <b>${B(freed)}</b></span>${skipped.length ? `<span>Skipped <b>${skipped.length}</b></span>` : ''}</div>
    <div class="plan">${done.length ? `<h4>Done</h4>${done.map(row).join('')}` : ''}${skipped.length ? `<h4>Skipped</h4>${skipped.map(row).join('')}` : ''}</div>`;
  return modal({ title, html, wide: true, yes: '', cancel: 'Close' });
}

// Plan, review, execute exactly the approved items, show the results.
export async function planThenExecute({ request, batch, title, verb, intro, empty }) {
  toast('Checking…');
  let plan;
  try { plan = await invoke('plan', { request }); } catch (e) { return toast(String(e), true); }
  hideToast();
  if (!plan.items.length) return toast(empty || 'Nothing to do.');
  const items = await reviewPlan(plan, { title, verb, intro });
  if (!items?.length) return;
  toast(`${verb}…`);
  let res;
  try { res = await invoke('execute', { batch, items }); } catch (e) { await hooks.refresh(true); return toast(String(e), true); }
  hideToast();
  await hooks.refresh(true);
  await showResults(res, `${verb}: ${res.filter(r => r.status === 'done').length} of ${res.length} done`);
  return res;
}
