// The Settings view. The engine this app runs saves directly; as a client
// the app saves through the running engine (PUT /v1/config), which checks
// and writes the same file. The watch section sets the thresholds of the
// read-only detectors and what they never report.
import { st } from '../store.js';
import { invoke } from '../ipc.js';
import { $, esc, tilde } from '../util.js';
import { act } from '../act.js';
import { noticesHtml } from '../findings.js';

const CFG = [
  ['scan_roots', 'Scan roots', 'list', 'Folders searched for repos, worktrees and build caches. One per line; empty means the defaults under your home folder.'],
  ['stale_hours', 'Build cache stale after (hours, main repos)', 'num', 'Unused this long and no agent running in the project.'],
  ['worktree_target_stale_hours', 'Build cache stale after (hours, worktrees)', 'num'],
  ['worktree_idle_days', 'Agent worktree idle after (days)', 'num', 'Idle means no file change outside target, node_modules and .git, and no commit.'],
  ['auto_sweep_worktrees', 'Sweep idle agent worktrees automatically', 'bool', 'Runs with each periodic scan. Branches are kept and uncommitted changes are saved as patches.'],
  ['backup_ignored_max_mb', 'Largest ignored-file backup (MB)', 'num', 'A worktree whose gitignored files (other than caches) exceed this is skipped instead of removed.'],
  ['low_disk_gb', 'Low disk floor (GB)', 'num'],
  ['alert_below_gb', 'Notify when free space drops below (GB)', 'num'],
  ['auto_reclaim', 'Clear stale caches automatically below the floor', 'bool'],
  ['auto_reclaim_target_gb', 'Auto-clear until this much is free (GB)', 'num'],
  ['include_node_modules', 'Include node_modules as caches', 'bool'],
  ['include_xcode', 'Include Xcode DerivedData', 'bool'],
  ['throttle_builds', 'Lower build priority under pressure', 'bool'],
  ['throttle_cpu_pct', 'Pressure when CPU is above (%)', 'num'],
  ['throttle_mem_free_pct', 'Pressure when free memory is below (%)', 'num'],
  ['build_nice', 'Nice level for throttled builds', 'num'],
  ['max_heavy_jobs', 'Build slots (0 = cores ÷ 4)', 'num', 'How many heavy jobs `mulchie run` and acquire_build_slot allow at once.'],
  ['scan_interval_mins', 'Disk scan interval (minutes)', 'num'],
  ['api_port', 'API port', 'num', 'Takes effect after restart.'],
];

// `config.watch`: the detectors behind "Observations". They only report.
const WATCH = [
  ['enabled', 'Watch running programs', 'bool', 'Looks for stuck, runaway or unusual processes and reports them as observations. It never acts on its own.'],
  ['interval_secs', 'Check every (seconds)', 'num'],
  ['runaway_minutes', 'Runaway window (minutes)', 'num', 'CPU and memory are judged over this long.'],
  ['runaway_cpu_pct', 'Runaway CPU (% of one core, averaged)', 'num'],
  ['memory_growth_mb', 'Report memory growth over the window (MB)', 'num'],
  ['stale_process_hours', 'Report idle agents and builds older than (hours)', 'num'],
  ['stale_idle_cpu_pct', 'Idle means average CPU below (% of one core)', 'num'],
  ['stopped_hours', 'Report processes paused longer than (hours)', 'num'],
  ['check_listeners', 'Check listening network ports', 'bool'],
  ['check_signatures', 'Check code signatures and download flags (macOS)', 'bool'],
  ['allow_exes', 'Never report these programs', 'list', 'Full paths, one per line.'],
  ['allow_ids', 'Never report these observations', 'list', 'Observation ids, one per line (the drawer’s Copy for agent shows the id).'],
];

const field = (id, [, l, t, h], v) => `<label for="${id}">${esc(l)}</label>${t === 'bool' ? `<div><input type="checkbox" id="${id}" ${v ? 'checked' : ''}></div>` : t === 'list' ? `<textarea id="${id}" rows="3" style="font:11.5px var(--mono)">${esc((v || []).join('\n'))}</textarea>` : `<input type="number" step="any" id="${id}" value="${esc(v)}" style="width:120px">`}${h ? `<div class="hint">${esc(h)}</div>` : ''}`;
const read = (el, t) => t === 'bool' ? el.checked : t === 'list' ? el.value.split('\n').map(x => x.trim()).filter(Boolean) : Number(el.value);

export function renderSettings(force) {
  const ui = st.ui, cfg = st.cfg;
  if (!cfg || (ui.cfgDirty && !force)) return;
  const c = cfg.config, w = c.watch || {}, editable = cfg.editable, conn = st.conn;
  const notes = [];
  if (cfg.config_error) notes.push(['bad', 'The settings file could not be used', `${cfg.config_error}. Mulchie runs on safe defaults with automatic cleanup off. Saving here writes a valid file and keeps the invalid one beside it.`]);
  if (!editable) notes.push(['info', 'Not connected to an engine', 'Settings can be saved once the app reaches Mulchie’s engine again.']);
  else if (conn?.mode === 'client') notes.push(['info', `Saved through the running engine (pid ${conn.engine_pid ?? '?'})`, `It checks the values and writes ${tilde(cfg.path || '')}; changes apply at once.`]);
  $('#cfgNote').innerHTML = noticesHtml(notes);
  const dis = (w.dismissed || []).length;
  $('#form').innerHTML = CFG.map(f => field('cf_' + f[0], f, c[f[0]])).join('') +
    `<h3 class="form-h">Watch</h3><div class="hint form-hint">Observations are checks for you to look at, not verdicts.${dis ? ` ${dis} dismissed observation(s) are kept in the file; show them again from Overview → All observations.` : ''}</div>` +
    WATCH.map(f => field('cw_' + f[0], f, w[f[0]])).join('');
  $('#form').querySelectorAll('input,textarea').forEach(el => { el.disabled = !editable; });
  $('#saveCfg').disabled = !editable;
}

export async function saveCfg() {
  // Dismissals change from the drawer while the form is open: keep the
  // engine's current list, not the one the form was drawn from.
  let fresh = st.cfg;
  try { fresh = await invoke('get_config'); } catch (e) { /* save what we have */ }
  const c = { ...st.cfg.config, watch: { ...(st.cfg.config.watch || {}), dismissed: fresh.config?.watch?.dismissed || [] } };
  for (const [k, , t] of CFG) c[k] = read($('#cf_' + k), t);
  for (const [k, , t] of WATCH) c.watch[k] = read($('#cw_' + k), t);
  const r = await act(() => invoke('set_config', { config: c }).then(() => true), 'Settings saved');
  if (r) {
    st.ui.cfgDirty = false;
    st.cfgRev = null;
  }
}
