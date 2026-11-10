// The Settings view. The engine this app runs saves directly; as a client
// the app saves through the running engine (PUT /v1/config), which checks
// and writes its file. The watch section sets the thresholds of the
// read-only detectors and what they never report.
//
// A save starts from the config as the engine has it now and changes only
// the fields edited here, so changes made elsewhere meanwhile (a dismissal,
// an allow-list entry, keys this build does not know) are kept.
import { st } from '../store.js';
import { invoke } from '../ipc.js';
import { $, esc, tilde, toast } from '../util.js';
import { act } from '../act.js';
import { noticesHtml } from '../findings.js';
import { renderCfgMore, setHtml } from './settings_more.js';

const CFG = [
  ['scan_roots', 'Scan roots', 'list', 'Folders searched for repos, worktrees and build caches. One per line; empty means the defaults under your home folder.'],
  ['stale_hours', 'Build cache stale after (hours, main repos)', 'int', 'Unused this long and no agent running in the project.'],
  ['worktree_target_stale_hours', 'Build cache stale after (hours, worktrees)', 'int'],
  ['worktree_idle_days', 'Agent worktree idle after (days)', 'int', 'Idle means no file change outside target, node_modules and .git, and no commit.'],
  ['auto_sweep_worktrees', 'Sweep idle agent worktrees automatically', 'bool', 'Runs with each periodic scan. Branches are kept and uncommitted changes are saved as patches.'],
  ['backup_ignored_max_mb', 'Largest ignored-file backup (MB)', 'int', 'A worktree whose gitignored files (other than caches) exceed this is skipped instead of removed.'],
  ['low_disk_gb', 'Low disk floor (GB)', 'int'],
  ['alert_below_gb', 'Notify when free space drops below (GB)', 'int'],
  ['auto_reclaim', 'Clear stale caches automatically below the floor', 'bool'],
  ['auto_reclaim_target_gb', 'Auto-clear until this much is free (GB)', 'int'],
  ['include_node_modules', 'Include node_modules as caches', 'bool'],
  ['include_xcode', 'Include Xcode DerivedData', 'bool'],
  ['throttle_builds', 'Lower build priority under pressure', 'bool'],
  ['throttle_cpu_pct', 'Pressure when CPU is above (%)', 'num'],
  ['throttle_mem_free_pct', 'Pressure when free memory is below (%)', 'num'],
  ['build_nice', 'Nice level for throttled builds', 'int'],
  ['max_heavy_jobs', 'Build slots (0 = cores ÷ 4)', 'int', 'How many heavy jobs `mulchie run` and acquire_build_slot allow at once.'],
  ['scan_interval_mins', 'Disk scan interval (minutes)', 'int'],
  ['api_port', 'API port', 'int', 'Takes effect after restart.'],
];

// `config.watch`: the detectors behind "Observations". They only report.
const WATCH = [
  ['enabled', 'Watch running programs', 'bool', 'Looks for stuck, runaway or unusual processes and reports them as observations. It never acts on its own.'],
  ['interval_secs', 'Check every (seconds)', 'int'],
  ['runaway_minutes', 'Runaway window (minutes)', 'int', 'CPU and memory are judged over this long.'],
  ['runaway_cpu_pct', 'Runaway CPU (% of one core, averaged)', 'num'],
  ['memory_growth_mb', 'Report memory growth over the window (MB)', 'int'],
  ['stale_process_hours', 'Report idle agents and builds older than (hours)', 'int'],
  ['stale_idle_cpu_pct', 'Idle means average CPU below (% of one core)', 'num'],
  ['stopped_hours', 'Report processes paused longer than (hours)', 'int'],
  ['check_listeners', 'Check listening network ports', 'bool'],
  ['check_signatures', 'Check code signatures and download flags (macOS)', 'bool'],
  ['allow_exes', 'Never report these programs', 'list', 'Full paths, one per line.'],
  ['allow_ids', 'Never report these observations', 'list', 'Observation ids, one per line (the drawer’s Copy for agent shows the id).'],
];

const field = (id, [, l, t, h], v) => `<label for="${id}">${esc(l)}</label>${t === 'bool' ? `<div><input type="checkbox" id="${id}" ${v ? 'checked' : ''}></div>` : t === 'list' ? `<textarea id="${id}" rows="3" style="font:11.5px var(--mono)">${esc((v || []).join('\n'))}</textarea>` : `<div><input type="number" step="${t === 'int' ? '1' : 'any'}" id="${id}" value="${esc(v)}" style="width:120px" aria-describedby="${id}_err"><div class="bad" id="${id}_err" role="alert"></div></div>`}${h ? `<div class="hint">${esc(h)}</div>` : ''}`;

// The raw form value, to tell which fields the user changed.
const raw = el => el.type === 'checkbox' ? el.checked : el.value;

// A field's value, or `{error}` naming the field.
function read(el, [, l, t]) {
  if (t === 'bool') return { v: el.checked };
  if (t === 'list') return { v: el.value.split('\n').map(x => x.trim()).filter(Boolean) };
  const s = el.value.trim();
  if (s === '' || !Number.isFinite(Number(s))) return { error: `${l}: enter a number` };
  const n = Number(s);
  if (t === 'int' && !Number.isInteger(n)) return { error: `${l}: enter a whole number` };
  return { v: n };
}

const FIELDS = () => [...CFG.map(f => ['cf_' + f[0], f, false]), ...WATCH.map(f => ['cw_' + f[0], f, true])];

function notes(cfg, changedElsewhere) {
  const editable = cfg.editable, conn = st.conn, out = [];
  if (cfg.config_error) out.push(['bad', 'The settings file could not be used', `${cfg.config_error}. Mulchie runs on safe defaults with automatic cleanup off. Saving here writes a valid file and keeps the invalid one beside it.`]);
  if (changedElsewhere) out.push(['warn', 'The settings changed elsewhere', 'Your edits here are kept. Saving applies only the fields you changed, on top of the current settings.']);
  if (!editable) out.push(['info', 'Not connected to an engine', 'Settings can be saved once the app reaches Mulchie’s engine again.']);
  else if (conn?.mode === 'client') out.push(['info', `Saved through the running engine (pid ${conn.engine_pid ?? '?'})`, `It checks the values and writes ${tilde(cfg.path || '')}; changes apply at once.`]);
  return noticesHtml(out);
}

// Draw the form from the current config, only when the config (or whether
// it can be saved) changed since the form was drawn, so a poll never
// replaces a focused field or closes an open control. While it has unsaved
// edits, a new config only adds a note; `force` redraws anyway (after a
// save).
export function renderSettings(force) {
  const ui = st.ui, cfg = st.cfg;
  if (!cfg) return;
  renderCfgMore();
  const same = $('#form').childElementCount && cfg.rev === ui.cfgFormRev && cfg.editable === ui.cfgFormEditable;
  if ((ui.cfgDirty || same) && !force) {
    setHtml($('#cfgNote'), notes(cfg, ui.cfgDirty && ui.cfgFormRev != null && cfg.rev !== ui.cfgFormRev));
    return;
  }
  const c = cfg.config, w = c.watch || {}, editable = cfg.editable;
  setHtml($('#cfgNote'), notes(cfg, false));
  const dis = (w.dismissed || []).length;
  $('#form').innerHTML = CFG.map(f => field('cf_' + f[0], f, c[f[0]])).join('') +
    `<h3 class="form-h">Watch</h3><div class="hint form-hint">Observations are checks for you to look at, not verdicts.${dis ? ` ${dis} dismissed observation(s) are kept in the file; show them again from Overview → All observations.` : ''}</div>` +
    WATCH.map(f => field('cw_' + f[0], f, w[f[0]])).join('');
  $('#form').querySelectorAll('input,textarea').forEach(el => { el.disabled = !editable; });
  $('#saveCfg').disabled = !editable;
  ui.cfgDirty = false;
  ui.cfgFormRev = cfg.rev;
  ui.cfgFormEditable = editable;
  ui.cfgBase = Object.fromEntries(FIELDS().map(([id]) => [id, raw($('#' + id))]));
}

export async function saveCfg() {
  const ui = st.ui, base = ui.cfgBase || {};
  // Check every edited field first, and say which is wrong.
  const changes = [], errors = [];
  for (const [id, f, inWatch] of FIELDS()) {
    const el = $('#' + id);
    const errEl = $('#' + id + '_err');
    if (errEl) errEl.textContent = '';
    if (raw(el) === base[id]) continue;
    const r = read(el, f);
    if (r.error) { errors.push(r.error); if (errEl) errEl.textContent = r.error; continue; }
    changes.push([inWatch, f[0], r.v]);
  }
  if (errors.length) return toast(errors.join('\n'), true);
  if (!changes.length) { ui.cfgDirty = false; return toast('Nothing changed'); }
  // Apply only those fields to the config as the engine has it now.
  let fresh;
  try { fresh = await invoke('get_config'); } catch (e) { return toast(`Not saved: ${e}`, true); }
  const c = structuredClone(fresh.config);
  c.watch = c.watch || {};
  for (const [inWatch, k, v] of changes) (inWatch ? c.watch : c)[k] = v;
  const r = await act(() => invoke('set_config', { config: c }).then(() => true), 'Settings saved');
  if (r) {
    st.cfg = { ...fresh, config: c };
    st.cfgRev = null;
    ui.cfgDirty = false;
    renderSettings(true);
  }
}
