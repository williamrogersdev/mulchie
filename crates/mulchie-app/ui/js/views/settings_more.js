// Settings for session limits, the command guard and update checks. Each
// card saves with a JSON merge patch (PATCH /v1/config) holding only the
// fields edited since the form was drawn, applied by the engine to the
// settings file's current content, so changes made elsewhere to other keys
// are kept. A list (the workspace budgets) is replaced whole by a merge
// patch, so saving an edited list that also changed elsewhere asks first.
// A refused value comes back as the engine's message naming the key; it is
// shown next to that field.
//
// A form is drawn again only when the config changed and the card has no
// unsaved edits; the status parts are updated in place without moving focus.
import { st, hooks } from '../store.js';
import { invoke } from '../ipc.js';
import { $, esc, ago, toast, tilde, B } from '../util.js';
import { resumePaused } from '../signals.js';
import { modal } from '../dialogs.js';
import { isHookOnly } from './sessions.js';

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
  ['runaway_repeat', 'Runaway command again', 'Running again, in the same folder, a command Mulchie just saw starting itself over and over.'],
];

const dirty = { lim: false, guard: false, upd: false };
// Per card: the config revision and editability the form was drawn from,
// the config as it was then, and the form's own reading of it.
const drawn = { lim: null, guard: null, upd: null };
let lstat = null, lstatAt = 0, lstatErr = null, lstatBusy = false;

// --------------------------------------------------------------- helpers

const ferr = path => `<div class="ferr" data-err="${esc(path)}" role="alert"></div>`;
const numIn = (attrs, v, label, step = 'any', [lo, hi] = []) => `<input type="number" step="${step}" ${lo != null ? `min="${lo}"` : ''} ${hi != null ? `max="${hi}"` : ''} ${attrs} value="${v ?? ''}" aria-label="${esc(label)}">`;

// The ranges the engine accepts (Config::validate).
const RANGE = {
  sustain_secs: [0, 86400], resume_after_secs: [10, 86400], stop_grace_secs: [1, 300],
  hog_share_pct: [10, 100], hog_sustain_secs: [10, 3600], interval_hours: [6, 720],
};

// A whole number in a range, or an error for that field.
function intIn(v, [lo, hi], label, errors, key) {
  const s = String(v).trim(), n = Number(s);
  if (s === '' || !Number.isInteger(n)) errors.push([key, `${label}: enter a whole number`]);
  else if (n < lo || n > hi) errors.push([key, `${label}: between ${lo} and ${hi}`]);
  return n;
}

function budgetHtml(b, { ws = null } = {}) {
  b = b || {};
  const cap = ([k, l, h]) => {
    const c = b[k];
    return `<label title="${esc(h)}"><input type="checkbox" data-c="${k}" data-f="on" ${c ? 'checked' : ''}> ${esc(l)}</label>
      ${numIn(`data-c="${k}" data-f="max"`, c?.max, `${l}: maximum`, 'any', [0])}
      ${numIn(`data-c="${k}" data-f="sustain_secs"`, c?.sustain_secs ?? 60, `${l}: seconds over before acting`, '1', RANGE.sustain_secs)}
      <select data-c="${k}" data-f="action" aria-label="${esc(l)}: action">${ACTIONS.map(([a, al]) => `<option value="${a}" ${(c?.action || 'notify') === a ? 'selected' : ''}>${al}</option>`).join('')}</select>
      <span></span>${ferr(k)}`;
  };
  return `<div class="budget" ${ws != null ? 'data-ws="1"' : 'data-default="1"'}>
    ${ws != null ? `<div class="row2" style="margin:0 0 6px"><label>Workspace folder <input type="text" data-f="path" value="${esc(ws)}" placeholder="/Users/you/code/project" style="width:min(420px,60vw)" aria-label="Workspace folder"></label><button class="btn sm" data-lim="remove-ws">Remove</button></div>${ferr('path')}` : ''}
    <div class="caps"><span class="hd">Cap</span><span class="hd">Maximum</span><span class="hd">Over for (s)</span><span class="hd">Then</span><span class="hd"></span>${CAPS.map(cap).join('')}</div>
    <div class="row2">
      <label>Resume paused after (s) ${numIn('data-f="resume_after_secs"', b.resume_after_secs ?? 300, 'Resume paused processes after seconds', '1', RANGE.resume_after_secs)}</label>
      <label>Stop grace (s) ${numIn('data-f="stop_grace_secs"', b.stop_grace_secs ?? 10, 'Seconds between SIGTERM and SIGKILL', '1', RANGE.stop_grace_secs)}</label>
      <label><input type="checkbox" data-f="include_agent" ${b.include_agent ? 'checked' : ''}> Pause and stop include the agent itself</label>
    </div>${ferr('resume_after_secs')}${ferr('stop_grace_secs')}</div>`;
}

// A budget from its block, or `{errors}` (each [suffix, message]).
function readBudget(el) {
  const out = {}, errors = [];
  const q = sel => el.querySelector(sel);
  for (const [k, l] of CAPS) {
    if (!q(`[data-c="${k}"][data-f="on"]`).checked) { out[k] = null; continue; }
    const m = q(`[data-c="${k}"][data-f="max"]`).value.trim();
    if (m === '' || !(Number(m) > 0)) errors.push([k, `${l}: enter a maximum above 0`]);
    out[k] = { max: Number(m), sustain_secs: intIn(q(`[data-c="${k}"][data-f="sustain_secs"]`).value, RANGE.sustain_secs, `${l}: seconds over`, errors, k), action: q(`[data-c="${k}"][data-f="action"]`).value };
  }
  out.resume_after_secs = intIn(q('[data-f="resume_after_secs"]').value, RANGE.resume_after_secs, 'Resume paused after', errors, 'resume_after_secs');
  out.stop_grace_secs = intIn(q('[data-f="stop_grace_secs"]').value, RANGE.stop_grace_secs, 'Stop grace', errors, 'stop_grace_secs');
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
    const key = (/((?:limits|memory_guard|guard|updates|sessions)(?:\.[a-z_]+|\[\d+\])+)/.exec(part) || [])[1];
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

// Only what changed between two readings of a form: objects field by field,
// a cap (an object with `max`) or a list whole.
const isObj = v => v && typeof v === 'object' && !Array.isArray(v);
export function diffPatch(before, after) {
  if (isObj(before) && isObj(after) && !('max' in after)) {
    const out = {};
    for (const k of Object.keys(after)) {
      const d = diffPatch(before[k], after[k]);
      if (d !== undefined) out[k] = d;
    }
    return Object.keys(out).length ? out : undefined;
  }
  return JSON.stringify(before) === JSON.stringify(after) ? undefined : after;
}

const CHANGED_HELP = 'The settings file changed elsewhere since this form was drawn, so nothing was saved. Your edits are kept. Save again to apply only the fields you changed on top of the settings as they are now.';

async function save(card, patch, which) {
  const btn = card.querySelector('[data-save]');
  btn.disabled = true;
  try {
    // Only while the file is still at the revision the form was drawn from
    // (an engine without revisions writes as before).
    const ifRevision = drawn[which]?.revision || null;
    const cfg = await invoke('patch_config', { patch, ifRevision });
    // The config and revision as saved, so the redrawn form writes against
    // the new revision.
    try { st.cfg = await invoke('get_config'); st.cfgRev = st.cfg.rev; }
    catch (err) { st.cfg = { ...(st.cfg || {}), config: cfg, revision: null }; st.cfgRev = null; }
    dirty[which] = false;
    toast('Saved');
    renderCfgMore(which);
    hooks.refresh(true);
  } catch (e) {
    // Not saved: keep the form as the user left it, with the reason.
    dirty[which] = true;
    const code = e?.code || '', msg = e?.message || String(e);
    if (code === 'config_changed') {
      // The user has now seen that it changed: the next save goes on top
      // of the file as it is.
      try { const fresh = await invoke('get_config'); if (drawn[which]) drawn[which].revision = fresh.revision || null; } catch (err) { /* the next save says why */ }
      showErrors(card, '', CHANGED_HELP);
    } else if (code === 'not_lock_holder') showErrors(card, '', LOCK_HELP);
    else if (code === 'invalid_config' || code === 'unknown_key') showErrors(card, msg);
    else showErrors(card, '', `Not saved: ${msg}`);
    toast('Not saved', true);
  } finally { btn.disabled = !st.cfg?.editable; }
}

// ------------------------------------------------- stricter limits check

const SEVERE = { pause: 1, stop: 2 };
const CAP_UNIT = { cpu_cores: 'cores', memory_mb: 'MB', processes: 'processes', write_mb_per_hour: 'MB/h', runtime_hours: 'h' };
const capName = k => (CAPS.find(c => c[0] === k)?.[1] || k).replace(/ \(.*\)$/, '');
const budgets = l => [['default', 'Default budget', l?.default || null], ...(l?.workspaces || []).map(w => [w.path, `Budget for ${tilde(w.path)}`, w.budget])];

// Caps that now pause or stop where they did not, act sooner or lower, or
// that newly include the agent: each as a sentence.
export function stricter(before, after) {
  const old = new Map(budgets(before).map(([k, , b]) => [k, b]));
  const out = [];
  for (const [k, title, b] of budgets(after)) {
    if (!b) continue;
    const o = old.get(k) || null;
    for (const [cap] of CAPS) {
      const c = b[cap], oc = o?.[cap];
      if (!c || !SEVERE[c.action]) continue;
      const was = oc && SEVERE[oc.action] ? oc : null;
      const why = !was ? `now ${c.action === 'stop' ? 'stops' : 'pauses'}` : SEVERE[c.action] > SEVERE[was.action] ? 'now stops instead of pausing'
        : c.max < was.max ? `cap lowered from ${was.max}` : c.sustain_secs < was.sustain_secs ? `acts after ${c.sustain_secs} s instead of ${was.sustain_secs} s` : '';
      if (why) out.push(`${title}: ${capName(cap)} over ${c.max} ${CAP_UNIT[cap]} for ${c.sustain_secs} s → ${c.action} (${why})`);
    }
    if (b.include_agent && !o?.include_agent && CAPS.some(([cap]) => SEVERE[b[cap]?.action])) out.push(`${title}: pause and stop now include the agent itself`);
  }
  return out;
}

// The budget a session falls under: the longest workspace folder that holds
// its workspace, else the default (LimitsConfig::budget_for).
function budgetOf(l, ws) {
  const inside = (w, p) => w === p || w.startsWith(p.endsWith('/') ? p : p + '/');
  const m = ws ? (l?.workspaces || []).filter(o => inside(ws, o.path)).sort((a, b) => b.path.split('/').length - a.path.split('/').length)[0] : null;
  return m ? m.budget : l?.default || null;
}

const usageOf = (r, cap) => ({ cpu_cores: r.cpu_cores, memory_mb: r.memory_mb, processes: r.processes, write_mb_per_hour: r.write_mb_per_hour, runtime_hours: r.runtime_secs != null ? r.runtime_secs / 3600 : null })[cap];
const usageText = (cap, v) => cap === 'memory_mb' ? B(v * 1048576) : cap === 'write_mb_per_hour' ? `${B(v * 1048576)}/h` : cap === 'runtime_hours' ? `${v.toFixed(1)} h` : `${+Number(v).toFixed(1)} ${CAP_UNIT[cap]}`;

// Running sessions over a pause or stop cap of the new limits, and what the
// next samples would do to each.
export function overNewCaps(limits, S) {
  const s = S?.sessions || {};
  const enforcing = lstat ? lstat.enforcing !== false : true;
  return (s.list || []).filter(r => r.status === 'running').flatMap(r => {
    const b = budgetOf(limits, r.workspace);
    if (!b) return [];
    return CAPS.filter(([cap]) => SEVERE[b[cap]?.action]).flatMap(([cap]) => {
      const c = b[cap], v = usageOf(r, cap);
      if (v == null || !(v > c.max)) return [];
      const who = `${r.agent}${r.workspace ? ` in ${tilde(r.workspace)}` : ''}`;
      const them = b.include_agent ? 'it and everything it started' : 'what it started (not the agent itself)';
      const what = c.action === 'pause'
        ? `Mulchie pauses ${them} and resumes them after ${b.resume_after_secs} s`
        : `Mulchie asks ${them} to quit, and force-quits what is left after ${b.stop_grace_secs} s`;
      const then = isHookOnly(r) ? 'It was registered by an agent hook, so Mulchie only reports it; nothing is signalled.'
        : !enforcing ? `This engine is not enforcing now (${notEnforcingText()}), so nothing happens until it is.`
          : `If it stays over for ${c.sustain_secs} s from the next sample, ${what}.`;
      return [`${who}: ${capName(cap)} ${usageText(cap, v)}, over the new cap of ${c.max} ${CAP_UNIT[cap]}. ${then}`];
    });
  });
}

function confirmStricter(lines, limits) {
  const over = overNewCaps(limits, st.S);
  return modal({
    title: 'Save limits that pause or stop processes?', wide: true, danger: true, yes: 'Save these limits',
    html: `<div>With these changes Mulchie pauses or stops processes on its own:</div><ul class="explain">${lines.map(l => `<li>${esc(l)}</li>`).join('')}</ul>
      <h4 class="cat-h">Running sessions over the new caps now (${over.length})</h4>
      ${over.length ? `<ul class="explain">${over.map(l => `<li>${esc(l)}</li>`).join('')}</ul>` : '<div class="dim">No running session is over the new caps right now.</div>'}`,
  });
}

// ---------------------------------------------------------------- limits

const MG_ACTIONS = [['off', 'Off'], ['notify', 'Report only'], ['stop', 'Close the biggest consumer']];
const MG_SCOPES = [['sessions', 'What agent sessions started (never the agent itself)'], ['all', 'Also my other apps and programs']];
const opts = (list, cur) => list.map(([v, n]) => `<option value="${v}" ${v === cur ? 'selected' : ''}>${esc(n)}</option>`).join('');

function limitsForm(c) {
  const l = c.limits || {}, s = c.sessions || {}, g = c.memory_guard || {};
  return `<label class="chk"><input type="checkbox" data-k="sessions.enabled" ${s.enabled !== false ? 'checked' : ''}> Track agent sessions (needed for the Sessions view, activity records and limits)</label>${ferr('sessions.enabled')}
    <ul class="explain">${ACTIONS.map(([, n, d]) => `<li><b>${esc(n)}</b>: ${esc(d)}</li>`).join('')}</ul>
    <label class="chk"><input type="checkbox" data-k="default-on" ${l.default ? 'checked' : ''}> A default budget for every session</label>
    <div id="limDefault" ${l.default ? '' : 'hidden'}>${budgetHtml(l.default)}</div>
    <h4 class="cat-h">Per workspace</h4>
    <div class="dim">A session in a folder at or under one of these uses that budget instead of the default (the longest matching folder wins; caps are not merged).</div>
    <div id="limWs">${(l.workspaces || []).map(w => budgetHtml(w.budget, { ws: w.path })).join('')}</div>
    <div class="actions"><button class="btn sm" data-lim="add-ws">Add a workspace budget</button></div>
    <h4 class="cat-h">Sessions hogging the machine</h4>
    <div class="row2"><label>Share of CPU or memory (%) ${numIn('data-k="limits.hog_share_pct"', l.hog_share_pct ?? 50, 'Hog share percent', 'any', RANGE.hog_share_pct)}</label>
      <label>For at least (s) ${numIn('data-k="limits.hog_sustain_secs"', l.hog_sustain_secs ?? 120, 'Hog seconds', '1', RANGE.hog_sustain_secs)}</label></div>
    <div class="dim">While the machine is strained, a session using this share for this long is reported. Reporting only; nothing is signalled.</div>
    ${ferr('limits.hog_share_pct')}${ferr('limits.hog_sustain_secs')}
    <h4 class="cat-h">When the machine runs low on memory</h4>
    <div class="row2"><label>Memory guard <select data-k="memory_guard.action" aria-label="Memory guard action">${opts(MG_ACTIONS, g.action || 'notify')}</select></label>
      <label>It may close <select data-k="memory_guard.scope" aria-label="Memory guard scope">${opts(MG_SCOPES, g.scope || 'sessions')}</select></label></div>
    <div class="dim">Memory is low under ${esc(g.free_pct ?? 5)}% free, or when the system reports critical memory pressure. Closing ends the biggest consumer (SIGTERM, then SIGKILL), one at a time, until memory is back; what it had not saved is lost.</div>
    ${ferr('memory_guard.action')}${ferr('memory_guard.scope')}`;
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
  const share = Number(n('limits.hog_share_pct'));
  if (n('limits.hog_share_pct') === '' || !Number.isFinite(share)) errors.push(['limits.hog_share_pct', 'Share: enter a number']);
  else if (share < RANGE.hog_share_pct[0] || share > RANGE.hog_share_pct[1]) errors.push(['limits.hog_share_pct', `Share: between ${RANGE.hog_share_pct[0]} and ${RANGE.hog_share_pct[1]}`]);
  const secs = intIn(n('limits.hog_sustain_secs'), RANGE.hog_sustain_secs, 'Seconds', errors, 'limits.hog_sustain_secs');
  return {
    errors,
    patch: {
      sessions: { enabled: card.querySelector('[data-k="sessions.enabled"]').checked },
      limits: { default: def ? def.budget : null, workspaces: ws, hog_share_pct: share, hog_sustain_secs: secs },
      memory_guard: { action: n('memory_guard.action'), scope: n('memory_guard.scope') },
    },
  };
}

// Why this engine does not act on budgets: the engine's own reason when it
// gives one, else what the state shows.
function notEnforcingText() {
  const S = st.S || {}, why = lstat?.not_enforcing_reason;
  if (why === 'not_lock_holder') return `another Mulchie engine${S.lock_held_by_other ? ` (pid ${S.lock_held_by_other})` : ''} holds the engine lock and acts on the budgets`;
  if (why === 'config_error') return `the settings file did not load, so nothing acts until it is fixed${S.config_error ? ` (${S.config_error})` : ''}`;
  if (why) return String(why).replace(/_/g, ' ');
  if (S.config_error) return `the settings file has an error, so automatic actions are off until it is fixed (${S.config_error})`;
  if (S.lock_held_by_other) return `another Mulchie engine (pid ${S.lock_held_by_other}) holds the engine lock and acts on the budgets`;
  return 'automatic actions are off in this engine';
}

const mbText = mb => mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${Math.round(mb)} MB`;

// The memory guard: whether it acts, whether memory is low, what it closed.
function memoryGuardHtml(g) {
  if (!g || g.action === 'off') return '';
  const mode = g.action !== 'stop' ? 'Reports only.' : g.enforcing ? 'On: it closes the biggest consumer when memory runs low.' : 'Set to close, but this engine does not act.';
  const w = g.would_close, closed = g.closings || [];
  const low = g.low ? `<div class="warn">Memory is low now (${esc(Math.round(g.free_pct))}% free)${w ? `; ${g.enforcing ? 'next to close' : 'would close'}: ${esc(w.name)} (${esc(mbText(w.mem_mb))})` : '; nothing it may close is big enough'}.</div>` : '';
  return `<h4 class="cat-h">Memory guard</h4><div class="dim">${esc(mode)}</div>${low}
    ${closed.slice(0, 10).map(c => `<div class="bkrow">${esc(c.name)} (${esc(mbText(c.mem_mb))}) <span class="dim">${esc(ago(c.at_unix))} ago · ${esc(c.detail || String(c.state).replace(/_/g, ' '))}</span></div>`).join('')}`;
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
    : l?.enforcing === false ? `Budgets are set, but this engine does not act on them: ${notEnforcingText()}. It reports breaches only.`
      : 'Enforcing: budgets are set and this engine acts on them as configured.';
  const pausedRow = (list, title, hint) => list.length ? `<h4 class="cat-h">${esc(title)} (${list.length})</h4>${list.map(p => `<div class="bkrow"><a class="lnk" role="button" tabindex="0" data-d="proc" data-dk="${p.pid}">pid ${p.pid}</a> · ${sLink(p.session_id, sessName(p.session_id))} <span class="dim">since ${esc(ago(p.since_unix))} ago</span>${p.pausing ? ' <span class="warn">pause not confirmed yet</span>' : ''}</div>`).join('')}<div class="actions"><button class="btn sm" data-lim="resume" data-pids="${list.map(p => `${p.pid}:${p.start_time}`).join(',')}">Resume ${list.length === 1 ? 'it' : 'them'}…</button></div>${hint ? `<div class="dim">${esc(hint)}</div>` : ''}` : '';
  return `<p class="why-s ${l?.enforcing === false && !(l?.report_only) ? 'warn' : ''}">${esc(mode)}</p>
    ${lstatErr ? `<p class="warn">Could not read the limits status: ${esc(lstatErr)}</p>` : ''}
    <h4 class="cat-h">Over a limit now (${active.length})</h4>${active.length ? active.map(b => `<div class="bkrow">${sLink(b.session_id, b.agent || sessName(b.session_id))}: ${esc(b.cap.replace(/_/g, ' '))} ${esc(+Number(b.value).toFixed(1))} against ${esc(b.max)} · <span class="dim">${esc(b.action)}, ${esc(String(b.action_state).replace(/_/g, ' '))}</span></div>`).join('') : '<div class="dim">No session is over a limit.</div>'}
    <h4 class="cat-h">Hogging the machine (${hogs.length})</h4>${hogs.length ? hogs.map(h => `<div class="bkrow">${sLink(h.session_id, h.agent)}: ${esc(Math.round(h.share_pct))}% of the ${esc(h.resource === 'cpu' ? 'CPU' : 'memory')} since ${esc(ago(h.since_unix))} ago</div>`).join('') : '<div class="dim">None.</div>'}
    ${pausedRow(paused, 'Paused by Mulchie', 'They resume on their own after the resume delay; resume them now through a plan.')}
    ${pausedRow(left, 'Left paused by an earlier engine', 'This engine will not resume these on its own.')}
    ${!paused.length && !left.length ? '<h4 class="cat-h">Paused by Mulchie (0)</h4><div class="dim">Nothing is paused.</div>' : ''}
    ${memoryGuardHtml(l?.memory_guard || S?.memory_guard)}`;
}

function loadLimitsStatus() {
  if (lstatBusy || Date.now() - lstatAt < 5000) return;
  lstatBusy = true;
  invoke('limits_status').then(r => { lstat = r; lstatErr = null; }, e => { lstatErr = String(e); })
    .finally(() => { lstatBusy = false; lstatAt = Date.now(); if (st.view === 'settings') { const el = $('#limStat'); if (el) setHtml(el, limitsStatusHtml()); } });
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
  const every = u.interval_hours ?? 24;
  return `<p class="why-s">When on, Mulchie asks GitHub for its own latest release and the npm registry for the latest versions of the agent tools you have installed, and reads Homebrew’s local data for outdated packages, every ${esc(every)} hours. It only reports what is newer; nothing is downloaded or installed. Untick “Check for updates” and save to turn this off.</p>
    <label class="chk"><input type="checkbox" data-k="updates.check" ${u.check !== 'off' ? 'checked' : ''}> Check for updates</label>
    <div class="dim">Off: no request is made and no version command runs.</div>
    <label class="chk"><input type="checkbox" data-k="updates.mulchie" ${u.mulchie !== false ? 'checked' : ''}> Mulchie itself (its latest release on GitHub)</label>
    <label class="chk"><input type="checkbox" data-k="updates.agents" ${u.agents !== false ? 'checked' : ''}> Agent tools (claude, codex, gemini, cursor-agent, against npm)</label>
    <label class="chk"><input type="checkbox" data-k="updates.homebrew" ${u.homebrew !== false ? 'checked' : ''}> Homebrew packages (brew outdated, local data only)</label>
    <label class="chk"><input type="checkbox" data-k="updates.advisories" ${u.advisories ? 'checked' : ''}> Security advisories for your projects’ dependencies</label>
    <div class="dim">This sends the package names and versions from your workspaces’ lockfiles to OSV.dev, which tells that service what your projects depend on. Off unless you turn it on.</div>
    <div class="row2"><label>Check every (hours) ${numIn('data-k="updates.interval_hours"', u.interval_hours ?? 24, 'Hours between checks', '1', RANGE.interval_hours)}</label></div>${ferr('updates.interval_hours')}`;
}

function readUpdates(card) {
  const b = k => card.querySelector(`[data-k="updates.${k}"]`).checked;
  const errors = [];
  const h = intIn(card.querySelector('[data-k="updates.interval_hours"]').value, RANGE.interval_hours, 'Hours', errors, 'updates.interval_hours');
  return { errors, patch: { updates: { check: b('check') ? 'on' : 'off', mulchie: b('mulchie'), agents: b('agents'), homebrew: b('homebrew'), advisories: b('advisories'), interval_hours: h } } };
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

// Replace a block's content only when it changed, and give focus back to
// the same control (by its data attributes) when it was inside.
export function setHtml(el, html) {
  if (el.__html === html) return;
  const f = document.activeElement, inside = f && f !== el && el.contains(f);
  const key = inside ? [...f.attributes].filter(a => a.name.startsWith('data-')).map(a => `[${a.name}="${CSS.escape(a.value)}"]`).join('') : '';
  el.innerHTML = html;
  el.__html = html;
  if (key) el.querySelector(key)?.focus({ preventScroll: true });
}

const setDisabled = (el, v) => { if (el.disabled !== v) el.disabled = v; };

// Why "Check now" cannot run, or ''.
function checkBlocked(cfg) {
  if (!cfg?.editable) return 'Not connected to an engine.';
  if (st.S?.updates?.enabled === false || cfg.config?.updates?.check === 'off') return 'Update checks are off; turn them on and save first.';
  return '';
}

// Draw the cards. A card's form is drawn again only when the config changed
// (or after its own save), and never over unsaved edits: then a note says
// the settings changed elsewhere. `only` redraws that card's form.
export function renderCfgMore(only) {
  const root = $('#cfgMore'), cfg = st.cfg;
  if (!root || !cfg) return;
  if (!root.dataset.built) {
    root.innerHTML = CARDS.map(([id, title]) => `<div class="card cfg-card" id="c-${id}" data-card="${id}"><h3>${esc(title)}</h3><div class="card-note"></div><div class="card-err why-s warn" role="alert" style="white-space:pre-line"></div><div class="cform"></div>
      <div class="actions"><button class="btn pri" data-save="${id}">Save ${esc(title.toLowerCase())}</button>${id === 'upd' ? '<button class="btn" data-upd="check" aria-describedby="updWhy">Check now</button><span class="dim" id="updWhy"></span>' : ''}</div>
      ${id === 'lim' ? '<div class="lstat" id="limStat"></div>' : id === 'upd' ? '<div id="updStat"></div>' : ''}</div>`).join('');
    root.dataset.built = '1';
    root.addEventListener('input', e => { const c = e.target.closest('[data-card]'); if (c) dirty[c.dataset.card] = true; });
    root.addEventListener('change', e => {
      const c = e.target.closest('[data-card]'); if (c) dirty[c.dataset.card] = true;
      if (e.target.dataset.k === 'default-on') $('#limDefault').hidden = !e.target.checked;
    });
    root.addEventListener('click', onClick);
  }
  for (const [id, , form, read, key] of CARDS) {
    const card = $(`#c-${id}`), was = drawn[id];
    const changed = !was || was.rev !== cfg.rev || was.editable !== cfg.editable;
    if (only === id || (!dirty[id] && changed)) {
      card.querySelector('.cform').innerHTML = form(cfg.config || {});
      card.querySelector('.card-err').textContent = '';
      const r = read(card);
      drawn[id] = { rev: cfg.rev, revision: cfg.revision || null, editable: cfg.editable, config: structuredClone(cfg.config || {}), base: r.errors.length ? null : r.patch };
      dirty[id] = false;
    }
    const elsewhere = dirty[id] && drawn[id] && drawn[id].rev !== cfg.rev;
    setHtml(card.querySelector('.card-note'), elsewhere
      ? `<p class="why-s warn" role="status">The ${esc(key)} settings changed elsewhere since this form was drawn. Your edits are kept; saving sends only the fields you changed, on top of the current settings.</p>` : '');
    card.querySelectorAll('.cform input, .cform textarea, .cform select, [data-save]').forEach(el => setDisabled(el, !cfg.editable));
    card.querySelector('.card-err').hidden = !card.querySelector('.card-err').textContent;
  }
  setHtml($('#limStat'), limitsStatusHtml());
  setHtml($('#updStat'), updatesStatusHtml());
  const why = checkBlocked(cfg), chk = root.querySelector('[data-upd="check"]');
  if (!chk.dataset.busy) setDisabled(chk, !!why);
  if ($('#updWhy').textContent !== why) $('#updWhy').textContent = why;
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
    const which = t.dataset.save, d = drawn[which];
    const patch = d?.base ? diffPatch(d.base, r.patch) : r.patch;
    if (patch === undefined) { dirty[which] = false; return toast('Nothing changed'); }
    if (which === 'lim') {
      // A list is replaced whole: if the workspace budgets were edited here
      // and also changed elsewhere, saving would drop the other change.
      if (patch.limits?.workspaces !== undefined) {
        let fresh;
        try { fresh = await invoke('get_config'); } catch (err) { return toast(`Not saved: ${err}`, true); }
        if (JSON.stringify(fresh.config?.limits?.workspaces || []) !== JSON.stringify(d?.config?.limits?.workspaces || [])) {
          const ok = await modal({ title: 'Replace workspace budgets changed elsewhere?', danger: true, yes: 'Replace them',
            html: '<div>The workspace budgets were changed elsewhere since this form was drawn. Saving replaces that list with the one here, so the other change is lost. Cancel keeps your edits unsaved.</div>' });
          if (!ok) return;
        }
      }
      const lines = stricter(d?.config?.limits, r.patch.limits);
      if (lines.length && !(await confirmStricter(lines, r.patch.limits))) return;
      // Turning on closing, or widening what may be closed while it is on.
      const mg = r.patch.memory_guard, was = d?.config?.memory_guard || {};
      if (patch.memory_guard && mg.action === 'stop' && (was.action !== 'stop' || (mg.scope === 'all' && was.scope !== 'all'))) {
        const ok = await modal({ title: 'Let Mulchie close processes when memory runs low?', danger: true, yes: 'Turn it on',
          html: `<div>When memory stays low, Mulchie will end the biggest consumer among ${esc(mg.scope === 'all' ? 'your apps, workspaces and programs (this can be an app you are using)' : 'what agent sessions started (builds, tests, dev servers; not the agent itself)')}, one at a time, until memory is back. Whatever a closed process had not saved is lost.</div>` });
        if (!ok) return;
      }
    }
    await save(card, patch, which);
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
    const list = t.dataset.pids.split(',').filter(Boolean).map(x => { const [pid, start] = x.split(':'); return { pid: +pid, start_time: +start }; });
    return resumePaused(list, `Resume ${list.length} paused process(es)?`);
  }
  if (t.dataset.upd === 'check') {
    t.disabled = true;
    t.dataset.busy = '1';
    try { await invoke('check_updates'); toast('Update check started; the result shows here when it finishes.'); hooks.refresh(true); }
    catch (err) { toast(String(err), true); }
    finally { delete t.dataset.busy; t.disabled = !!checkBlocked(st.cfg); }
  }
}
