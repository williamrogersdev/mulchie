// Settings for session limits, the command guard and update checks. Each
// card saves only its own keys with a JSON merge patch (PATCH /v1/config),
// applied by the engine to the settings file's current content, so edits
// made elsewhere to other keys are kept. A refused value comes back as the
// engine's message naming the key; it is shown next to that field.
import { st, hooks } from '../store.js';
import { invoke } from '../ipc.js';
import { $, esc, ago, toast } from '../util.js';
import { planAndRun } from '../signals.js';

const CAPS = [
  ['cpu_cores', 'CPU (cores)', 'Cores in use by the session’s processes together.'],
  ['memory_mb', 'Memory (MB)', 'Resident memory of the session’s processes together.'],
  ['processes', 'Processes', 'How many processes the session has at once.'],
  ['write_mb_per_hour', 'Disk writes (MB per hour)', 'Not available where the system does not report writes per process.'],
  ['runtime_hours', 'Runtime (hours)', 'How long the session has been running.'],
];
const ACTIONS = [
  ['notify', 'Notify', 'Records an observation and sends a notification; nothing is signalled.'],
  ['deprioritize', 'Lower priority', 'Lowers the priority of the session’s processes until it is back under the cap.'],
  ['pause', 'Pause', 'Pauses (SIGSTOP) the processes the agent started, and resumes them after the resume delay; the agent itself is spared unless “include the agent” is on.'],
  ['stop', 'Stop', 'Asks the processes the agent started to quit (SIGTERM), then force-quits (SIGKILL) what is left after the grace period; the agent itself is spared unless “include the agent” is on.'],
];
const RULES = [
  ['recursive_delete', 'Recursive delete', '`rm -r` or `mv` of something outside the session’s workspace, or of the home folder or /.'],
  ['pipe_to_shell', 'Download piped to a shell', '`curl … | sh` and similar.'],
  ['sudo', 'sudo', '`sudo` or `doas`.'],
  ['force_push', 'Force push', 'Force-pushing to or deleting a protected branch, or rewriting its history.'],
  ['disable_mulchie', 'Disabling Mulchie', 'Stopping, unloading or removing Mulchie, or editing its files.'],
  ['credential_read', 'Reading credentials', 'Reading SSH private keys or cloud credentials, or dumping the keychain.'],
  ['startup_write', 'Startup files', 'Writing shell startup files, launch agents, systemd user units or the crontab.'],
];

const dirty = { lim: false, guard: false, upd: false };
let lstat = null, lstatAt = 0, lstatErr = null, lstatBusy = false;

// --------------------------------------------------------------- helpers

const ferr = path => `<div class="ferr" data-err="${esc(path)}" role="alert"></div>`;
const numIn = (attrs, v, label, step = 'any') => `<input type="number" step="${step}" ${attrs} value="${v ?? ''}" aria-label="${esc(label)}">`;

function budgetHtml(b, { ws = null } = {}) {
  b = b || {};
  const cap = ([k, l, h]) => {
    const c = b[k];
    return `<label title="${esc(h)}"><input type="checkbox" data-c="${k}" data-f="on" ${c ? 'checked' : ''}> ${esc(l)}</label>
      ${numIn(`data-c="${k}" data-f="max"`, c?.max, `${l}: maximum`)}
      ${numIn(`data-c="${k}" data-f="sustain_secs"`, c?.sustain_secs ?? 60, `${l}: seconds over before acting`, '1')}
      <select data-c="${k}" data-f="action" aria-label="${esc(l)}: action">${ACTIONS.map(([a, al]) => `<option value="${a}" ${(c?.action || 'notify') === a ? 'selected' : ''}>${al}</option>`).join('')}</select>
      <span></span>${ferr(k)}`;
  };
  return `<div class="budget" ${ws != null ? 'data-ws="1"' : 'data-default="1"'}>
    ${ws != null ? `<div class="row2" style="margin:0 0 6px"><label>Workspace folder <input type="text" data-f="path" value="${esc(ws)}" placeholder="/Users/you/code/project" style="width:min(420px,60vw)" aria-label="Workspace folder"></label><button class="btn sm" data-lim="remove-ws">Remove</button></div>${ferr('path')}` : ''}
    <div class="caps"><span class="hd">Cap</span><span class="hd">Maximum</span><span class="hd">Over for (s)</span><span class="hd">Then</span><span class="hd"></span>${CAPS.map(cap).join('')}</div>
    <div class="row2">
      <label>Resume paused after (s) ${numIn('data-f="resume_after_secs"', b.resume_after_secs ?? 300, 'Resume paused processes after seconds', '1')}</label>
      <label>Stop grace (s) ${numIn('data-f="stop_grace_secs"', b.stop_grace_secs ?? 10, 'Seconds between SIGTERM and SIGKILL', '1')}</label>
      <label><input type="checkbox" data-f="include_agent" ${b.include_agent ? 'checked' : ''}> Pause and stop include the agent itself</label>
    </div>${ferr('resume_after_secs')}${ferr('stop_grace_secs')}</div>`;
}

// A budget from its block, or `{errors}` (each [suffix, message]).
function readBudget(el) {
  const out = {}, errors = [];
  const q = sel => el.querySelector(sel);
  const int = (v, name, label) => { const s = String(v).trim(); if (s === '' || !Number.isInteger(Number(s))) { errors.push([name, `${label}: enter a whole number`]); return 0; } return Number(s); };
  for (const [k, l] of CAPS) {
    if (!q(`[data-c="${k}"][data-f="on"]`).checked) { out[k] = null; continue; }
    const m = q(`[data-c="${k}"][data-f="max"]`).value.trim();
    if (m === '' || !(Number(m) > 0)) errors.push([k, `${l}: enter a maximum above 0`]);
    out[k] = { max: Number(m), sustain_secs: int(q(`[data-c="${k}"][data-f="sustain_secs"]`).value, k, `${l}: seconds over`), action: q(`[data-c="${k}"][data-f="action"]`).value };
  }
  out.resume_after_secs = int(q('[data-f="resume_after_secs"]').value, 'resume_after_secs', 'Resume paused after');
  out.stop_grace_secs = int(q('[data-f="stop_grace_secs"]').value, 'stop_grace_secs', 'Stop grace');
  out.include_agent = q('[data-f="include_agent"]').checked;
  return { budget: out, errors };
}

const blockOf = (card, path) => {
  if (path.startsWith('limits.default')) return [card.querySelector('[data-default]'), path.replace(/^limits\.default\.?/, '')];
  const m = /^limits\.workspaces\[(\d+)\]\.?(?:budget\.)?(.*)$/.exec(path);
  if (m) return [card.querySelectorAll('[data-ws]')[+m[1]], m[2]];
  return [card, path];
};

// Put each part of an engine message next to the field its key names.
function showErrors(card, message, extra) {
  card.querySelectorAll('[data-err]').forEach(e => { e.textContent = ''; });
  const top = card.querySelector('.card-err');
  top.textContent = '';
  const parts = String(message).replace(/^config has invalid values: /, '').split('; ');
  for (const part of parts) {
    const key = (/((?:limits|guard|updates|sessions)(?:\.[a-z_]+|\[\d+\])+)/.exec(part) || [])[1];
    let placed = false;
    if (key) {
      const [block, rest] = blockOf(card, key);
      const name = rest.split('.')[0];
      const el = block && (block.querySelector(`[data-err="${CSS.escape(rest)}"]`) || block.querySelector(`[data-err="${CSS.escape(name)}"]`) || card.querySelector(`[data-err="${CSS.escape(key)}"]`));
      if (el) { el.textContent = part; placed = true; }
    }
    if (!placed) top.textContent += (top.textContent ? '\n' : '') + part;
  }
  if (extra) top.textContent += (top.textContent ? '\n' : '') + extra;
}

const LOCK_HELP = 'Only the engine that holds Mulchie’s engine lock writes settings, and the engine this app reached does not hold it, so nothing was saved. Save from the Mulchie that runs as the service (or quit the other engines and let this app take over).';

async function save(card, patch, which) {
  const btn = card.querySelector('[data-save]');
  btn.disabled = true;
  try {
    const cfg = await invoke('patch_config', { patch });
    st.cfg = { ...(st.cfg || {}), config: cfg };
    st.cfgRev = null;
    dirty[which] = false;
    toast('Saved');
    renderCfgMore(which);
    hooks.refresh(true);
  } catch (e) {
    // Not saved: keep the form as the user left it, with the reason.
    dirty[which] = true;
    const code = e?.code || '', msg = e?.message || String(e);
    if (code === 'not_lock_holder') showErrors(card, '', LOCK_HELP);
    else if (code === 'invalid_config') showErrors(card, msg);
    else showErrors(card, '', `Not saved: ${msg}`);
    toast('Not saved', true);
  } finally { btn.disabled = !st.cfg?.editable; }
}

// ---------------------------------------------------------------- limits

function limitsForm(c) {
  const l = c.limits || {}, s = c.sessions || {};
  return `<label class="chk"><input type="checkbox" data-k="sessions.enabled" ${s.enabled !== false ? 'checked' : ''}> Track agent sessions (needed for the Sessions view, activity records and limits)</label>${ferr('sessions.enabled')}
    <ul class="explain">${ACTIONS.map(([, n, d]) => `<li><b>${esc(n)}</b>: ${esc(d)}</li>`).join('')}</ul>
    <label class="chk"><input type="checkbox" data-k="default-on" ${l.default ? 'checked' : ''}> A default budget for every session</label>
    <div id="limDefault" ${l.default ? '' : 'hidden'}>${budgetHtml(l.default)}</div>
    <h4 class="cat-h">Per workspace</h4>
    <div class="dim">A session in a folder at or under one of these uses that budget instead of the default (the longest matching folder wins; caps are not merged).</div>
    <div id="limWs">${(l.workspaces || []).map(w => budgetHtml(w.budget, { ws: w.path })).join('')}</div>
    <div class="actions"><button class="btn sm" data-lim="add-ws">Add a workspace budget</button></div>
    <h4 class="cat-h">Sessions hogging the machine</h4>
    <div class="row2"><label>Share of CPU or memory (%) ${numIn('data-k="limits.hog_share_pct"', l.hog_share_pct ?? 50, 'Hog share percent')}</label>
      <label>For at least (s) ${numIn('data-k="limits.hog_sustain_secs"', l.hog_sustain_secs ?? 120, 'Hog seconds', '1')}</label></div>
    <div class="dim">While the machine is strained, a session using this share for this long is reported. Reporting only; nothing is signalled.</div>
    ${ferr('limits.hog_share_pct')}${ferr('limits.hog_sustain_secs')}`;
}

function readLimits(card) {
  const errors = [];
  const def = card.querySelector('[data-k="default-on"]').checked ? readBudget(card.querySelector('[data-default]')) : null;
  if (def) errors.push(...def.errors.map(([k, m]) => [`limits.default.${k}`, m]));
  const ws = [...card.querySelectorAll('[data-ws]')].map((el, i) => {
    const r = readBudget(el), path = el.querySelector('[data-f="path"]').value.trim();
    errors.push(...r.errors.map(([k, m]) => [`limits.workspaces[${i}].budget.${k}`, m]));
    if (!path.startsWith('/')) errors.push([`limits.workspaces[${i}].path`, 'Workspace folder: enter an absolute path']);
    return { path, budget: r.budget };
  });
  const n = k => card.querySelector(`[data-k="${k}"]`).value.trim();
  const share = Number(n('limits.hog_share_pct')), secs = Number(n('limits.hog_sustain_secs'));
  if (n('limits.hog_share_pct') === '' || !Number.isFinite(share)) errors.push(['limits.hog_share_pct', 'Share: enter a number']);
  if (n('limits.hog_sustain_secs') === '' || !Number.isInteger(secs)) errors.push(['limits.hog_sustain_secs', 'Seconds: enter a whole number']);
  return {
    errors,
    patch: {
      sessions: { enabled: card.querySelector('[data-k="sessions.enabled"]').checked },
      limits: { default: def ? def.budget : null, workspaces: ws, hog_share_pct: share, hog_sustain_secs: secs },
    },
  };
}

function limitsStatusHtml() {
  const S = st.S, s = S?.sessions || {}, l = lstat;
  const sessName = id => (s.list || []).find(x => x.id === id)?.agent || id;
  const sLink = (id, text) => `<a class="lnk" role="button" tabindex="0" data-d="session" data-dk="${esc(id)}">${esc(text)}</a>`;
  const active = (l?.breaches || s.breaches || []).filter(b => b.cleared_unix == null);
  const hogs = l?.hogs || s.hogs || [];
  const paused = l?.paused_by_mulchie || s.paused_by_mulchie || [], left = l?.paused_left || s.paused_left || [];
  const mode = (l?.report_only ?? s.limits_report_only)
    ? 'Report only: no budget is set, so Mulchie only reports sessions that hog the machine.'
    : l?.enforcing === false ? 'Budgets are set, but this engine does not hold the engine lock, so it reports breaches and never acts on them.'
      : 'Enforcing: budgets are set and this engine acts on them as configured.';
  const pausedRow = (list, title, hint) => list.length ? `<h4 class="cat-h">${esc(title)} (${list.length})</h4>${list.map(p => `<div class="bkrow"><a class="lnk" role="button" tabindex="0" data-d="proc" data-dk="${p.pid}">pid ${p.pid}</a> · ${sLink(p.session_id, sessName(p.session_id))} <span class="dim">since ${esc(ago(p.since_unix))} ago</span></div>`).join('')}<div class="actions"><button class="btn sm" data-lim="resume" data-pids="${list.map(p => p.pid).join(',')}">Resume ${list.length === 1 ? 'it' : 'them'}…</button></div>${hint ? `<div class="dim">${esc(hint)}</div>` : ''}` : '';
  return `<p class="why-s ${l?.enforcing === false && !(l?.report_only) ? 'warn' : ''}">${esc(mode)}</p>
    ${lstatErr ? `<p class="warn">Could not read the limits status: ${esc(lstatErr)}</p>` : ''}
    <h4 class="cat-h">Over a limit now (${active.length})</h4>${active.length ? active.map(b => `<div class="bkrow">${sLink(b.session_id, b.agent || sessName(b.session_id))}: ${esc(b.cap.replace(/_/g, ' '))} ${esc(+Number(b.value).toFixed(1))} against ${esc(b.max)} · <span class="dim">${esc(b.action)}, ${esc(String(b.action_state).replace(/_/g, ' '))}</span></div>`).join('') : '<div class="dim">No session is over a limit.</div>'}
    <h4 class="cat-h">Hogging the machine (${hogs.length})</h4>${hogs.length ? hogs.map(h => `<div class="bkrow">${sLink(h.session_id, h.agent)}: ${esc(Math.round(h.share_pct))}% of the ${esc(h.resource === 'cpu' ? 'CPU' : 'memory')} since ${esc(ago(h.since_unix))} ago</div>`).join('') : '<div class="dim">None.</div>'}
    ${pausedRow(paused, 'Paused by Mulchie', 'They resume on their own after the resume delay; resume them now through a plan.')}
    ${pausedRow(left, 'Left paused by an earlier engine', 'This engine will not resume these on its own.')}
    ${!paused.length && !left.length ? '<h4 class="cat-h">Paused by Mulchie (0)</h4><div class="dim">Nothing is paused.</div>' : ''}`;
}

function loadLimitsStatus() {
  if (lstatBusy || Date.now() - lstatAt < 5000) return;
  lstatBusy = true;
  invoke('limits_status').then(r => { lstat = r; lstatErr = null; }, e => { lstatErr = String(e); })
    .finally(() => { lstatBusy = false; lstatAt = Date.now(); if (st.view === 'settings') { const el = $('#limStat'); if (el) el.innerHTML = limitsStatusHtml(); } });
}

// ----------------------------------------------------------------- guard

function guardForm(c) {
  const g = c.guard || {}, r = g.rules || {};
  return `<label class="chk"><input type="checkbox" data-k="guard.enabled" ${g.enabled !== false ? 'checked' : ''}> Check agents’ shell commands (through agent hooks)</label>
    <div class="dim">Agents connected with <span class="mono">mulchie hooks install</span> ask Mulchie before running a shell command. <b>Warn</b> makes the agent ask you first; <b>Block</b> refuses the command; <b>Off</b> ignores the rule.</div>
    <table class="mini-t" style="margin-top:6px"><colgroup><col style="width:34%"><col><col style="width:96px"></colgroup>${RULES.map(([k, l, d]) => `<tr><td>${esc(l)}</td><td class="dim" style="white-space:normal">${esc(d.replace(/`/g, ''))}</td><td><select data-rule="${k}" aria-label="${esc(l)}">${['off', 'warn', 'block'].map(m => `<option value="${m}" ${(r[k] || 'warn') === m ? 'selected' : ''}>${m}</option>`).join('')}</select></td></tr>`).join('')}</table>
    <label for="gBranches" style="display:block;margin-top:8px">Protected branches (one per line)</label><textarea id="gBranches" data-k="guard.protected_branches" rows="3" style="font:11.5px var(--mono);width:min(320px,100%)">${esc((g.protected_branches || []).join('\n'))}</textarea>${ferr('guard.protected_branches')}
    <label class="chk"><input type="checkbox" data-k="guard.build_advice" ${g.build_advice !== false ? 'checked' : ''}> Under pressure, advise running heavy builds through <span class="mono">mulchie run</span></label>`;
}

function readGuard(card) {
  const rules = Object.fromEntries([...card.querySelectorAll('[data-rule]')].map(s => [s.dataset.rule, s.value]));
  return {
    errors: [],
    patch: { guard: {
      enabled: card.querySelector('[data-k="guard.enabled"]').checked, rules,
      protected_branches: card.querySelector('[data-k="guard.protected_branches"]').value.split('\n').map(x => x.trim()).filter(Boolean),
      build_advice: card.querySelector('[data-k="guard.build_advice"]').checked,
    } },
  };
}

// --------------------------------------------------------------- updates

function updatesForm(c) {
  const u = c.updates || {};
  return `<label class="chk"><input type="checkbox" data-k="updates.check" ${u.check !== 'off' ? 'checked' : ''}> Check for updates</label>
    <div class="dim">Off: no request is made and no version command runs.</div>
    <label class="chk"><input type="checkbox" data-k="updates.mulchie" ${u.mulchie !== false ? 'checked' : ''}> Mulchie itself (its latest release on GitHub)</label>
    <label class="chk"><input type="checkbox" data-k="updates.agents" ${u.agents !== false ? 'checked' : ''}> Agent tools (claude, codex, gemini, cursor-agent, against npm)</label>
    <label class="chk"><input type="checkbox" data-k="updates.homebrew" ${u.homebrew !== false ? 'checked' : ''}> Homebrew packages (brew outdated, local data only)</label>
    <label class="chk"><input type="checkbox" data-k="updates.advisories" ${u.advisories ? 'checked' : ''}> Security advisories for your projects’ dependencies</label>
    <div class="dim">This sends the package names and versions from your workspaces’ lockfiles to OSV.dev, which tells that service what your projects depend on. Off unless you turn it on.</div>
    <div class="row2"><label>Check every (hours) ${numIn('data-k="updates.interval_hours"', u.interval_hours ?? 24, 'Hours between checks', '1')}</label></div>${ferr('updates.interval_hours')}`;
}

function readUpdates(card) {
  const b = k => card.querySelector(`[data-k="updates.${k}"]`).checked;
  const h = card.querySelector('[data-k="updates.interval_hours"]').value.trim();
  const errors = h === '' || !Number.isInteger(Number(h)) ? [['updates.interval_hours', 'Hours: enter a whole number']] : [];
  return { errors, patch: { updates: { check: b('check') ? 'on' : 'off', mulchie: b('mulchie'), agents: b('agents'), homebrew: b('homebrew'), advisories: b('advisories'), interval_hours: Number(h) } } };
}

function updatesStatusHtml() {
  const u = st.S?.updates;
  if (!u) return '<div class="dim">The engine reports no update status.</div>';
  const when = t => t ? `${ago(t)} ago` : 'never';
  const next = t => t ? new Date(t * 1000).toLocaleString() : '—';
  return `${!u.enabled ? '<p class="why-s">Update checks are off.</p>' : ''}
    <dl class="kv"><dt>last check</dt><dd>${u.checking ? '<span class="info">checking now…</span>' : esc(when(u.checked_unix))}</dd>
    ${u.enabled ? `<dt>next check</dt><dd>${esc(next(u.next_check_unix))}</dd>` : ''}
    <dt>available</dt><dd>${u.available ? `<a class="lnk" role="button" tabindex="0" data-d="findings" data-dk="update">${u.available} update(s) →</a>` : 'none'}</dd>
    ${u.failures ? `<dt>failed checks</dt><dd class="warn">${u.failures} in a row</dd>` : ''}</dl>
    ${u.errors?.length ? `<div class="warn">${u.errors.map(esc).join('<br>')}</div>` : ''}`;
}

// ---------------------------------------------------------------- render

const CARDS = [
  ['lim', 'Limits for agent sessions', limitsForm, readLimits, 'limits'],
  ['guard', 'Command guard', guardForm, readGuard, 'guard'],
  ['upd', 'Update checks', updatesForm, readUpdates, 'updates'],
];

// Draw the cards. A card with unsaved edits keeps them; its status part is
// redrawn on every poll. `only` redraws that card's form (after a save).
export function renderCfgMore(only) {
  const root = $('#cfgMore'), cfg = st.cfg;
  if (!root || !cfg) return;
  if (!root.dataset.built) {
    root.innerHTML = CARDS.map(([id, title]) => `<div class="card cfg-card" id="c-${id}" data-card="${id}"><h3>${esc(title)}</h3><div class="card-err why-s warn" role="alert" style="white-space:pre-line"></div><div class="cform"></div>
      <div class="actions"><button class="btn pri" data-save="${id}">Save ${esc(title.toLowerCase())}</button>${id === 'upd' ? '<button class="btn" data-upd="check">Check now</button>' : ''}</div>
      ${id === 'lim' ? '<div class="lstat" id="limStat"></div>' : id === 'upd' ? '<div id="updStat"></div>' : ''}</div>`).join('');
    root.dataset.built = '1';
    root.addEventListener('input', e => { const c = e.target.closest('[data-card]'); if (c) dirty[c.dataset.card] = true; });
    root.addEventListener('change', e => {
      const c = e.target.closest('[data-card]'); if (c) dirty[c.dataset.card] = true;
      if (e.target.dataset.k === 'default-on') $('#limDefault').hidden = !e.target.checked;
    });
    root.addEventListener('click', onClick);
  }
  for (const [id, , form] of CARDS) {
    const card = $(`#c-${id}`);
    if (only === id || (!dirty[id] && only == null)) {
      card.querySelector('.cform').innerHTML = form(cfg.config || {});
      card.querySelector('.card-err').textContent = '';
    }
    card.querySelectorAll('.cform input, .cform textarea, .cform select, [data-save]').forEach(el => { el.disabled = !cfg.editable; });
    card.querySelector('.card-err').hidden = !card.querySelector('.card-err').textContent;
  }
  $('#limStat').innerHTML = limitsStatusHtml();
  $('#updStat').innerHTML = updatesStatusHtml();
  loadLimitsStatus();
}

async function onClick(e) {
  const t = e.target.closest('button');
  if (!t || t.disabled) return;
  const card = t.closest('[data-card]');
  if (t.dataset.save) {
    const [, , , read] = CARDS.find(c => c[0] === t.dataset.save);
    const r = read(card);
    if (r.errors.length) {
      dirty[t.dataset.save] = true;
      showErrors(card, r.errors.map(([k, m]) => `${k}: ${m}`).join('; '));
      card.querySelector('.card-err').hidden = !card.querySelector('.card-err').textContent;
      return toast('Not saved: check the marked fields', true);
    }
    await save(card, r.patch, t.dataset.save);
    card.querySelector('.card-err').hidden = !card.querySelector('.card-err').textContent;
    return;
  }
  if (t.dataset.lim === 'add-ws') {
    $('#limWs').insertAdjacentHTML('beforeend', budgetHtml({ memory_mb: { max: 8192, sustain_secs: 60, action: 'notify' } }, { ws: '' }));
    dirty.lim = true;
    return $('#limWs').lastElementChild.querySelector('[data-f="path"]').focus();
  }
  if (t.dataset.lim === 'remove-ws') { t.closest('[data-ws]').remove(); dirty.lim = true; return; }
  if (t.dataset.lim === 'resume') {
    const pids = t.dataset.pids.split(',').filter(Boolean).map(Number);
    return planAndRun(pids.map(pid => ({ pid, signal: 'cont' })), { title: `Resume ${pids.length} paused process(es)?`, verb: 'Resume', note: 'Sends SIGCONT to each process that is still the one Mulchie paused.', danger: false });
  }
  if (t.dataset.upd === 'check') {
    t.disabled = true;
    try { await invoke('check_updates'); toast('Update check started; the result shows here when it finishes.'); hooks.refresh(true); }
    catch (err) { toast(String(err), true); }
    finally { t.disabled = false; }
  }
}
