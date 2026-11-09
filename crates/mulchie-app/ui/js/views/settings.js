// The Settings view. Only the engine this app runs can save; as a client the
// form is read-only and says where the settings live.
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

export function renderSettings(force) {
  const ui = st.ui, cfg = st.cfg;
  if (!cfg || (ui.cfgDirty && !force)) return;
  const c = cfg.config, editable = cfg.editable;
  $('#cfgNote').innerHTML = editable ? '' : noticesHtml([['info', 'Read-only while connected to another engine',
    `The running engine owns these settings. Edit ${tilde(cfg.path)} and restart that engine, or stop it so this app runs the engine.`]]);
  $('#form').innerHTML = CFG.map(([k, l, t, h]) => `<label for="cf_${k}">${l}</label>${t === 'bool' ? `<div><input type="checkbox" id="cf_${k}" ${c[k] ? 'checked' : ''}></div>` : t === 'list' ? `<textarea id="cf_${k}" rows="3" style="font:11.5px var(--mono)">${esc((c[k] || []).join('\n'))}</textarea>` : `<input type="number" id="cf_${k}" value="${esc(c[k])}" style="width:120px">`}${h ? `<div class="hint">${esc(h)}</div>` : ''}`).join('');
  $('#form').querySelectorAll('input,textarea').forEach(el => { el.disabled = !editable; });
  $('#saveCfg').disabled = !editable;
}

export async function saveCfg() {
  const c = { ...st.cfg.config };
  for (const [k, , t] of CFG) {
    const el = $('#cf_' + k);
    c[k] = t === 'bool' ? el.checked : t === 'list' ? el.value.split('\n').map(x => x.trim()).filter(Boolean) : Number(el.value);
  }
  const r = await act(() => invoke('set_config', { config: c }).then(() => true), 'Settings saved');
  if (r) {
    st.ui.cfgDirty = false;
    st.cfgRev = null;
  }
}
