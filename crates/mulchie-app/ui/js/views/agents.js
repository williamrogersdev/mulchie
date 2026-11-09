// The Agent API view: API, CLI, MCP and instructions for agents.
import { st } from '../store.js';
import { invoke } from '../ipc.js';
import { $, esc, tilde, th } from '../util.js';

export async function loadIntegration(force) {
  if (!st.integ || force) {
    try { st.integ = await invoke('integration'); } catch (e) { st.integ = {}; }
  }
  return st.integ;
}

const ENDPOINTS = [
  ['GET', '/v1/health', 'open', 'health (or stale), sample age, disk, load, free build slots'],
  ['GET', '/v1/state', 'token', 'full snapshot: processes, workspaces, scan, disk map, slots, actions'],
  ['GET', '/v1/history', 'token', '30 minutes of 3 s samples'],
  ['GET', '/v1/advice', 'token', 'ok_to_start_heavy_work, suggested_jobs, reasons'],
  ['POST', '/v1/scan', 'token', 'rescan caches and the disk map'],
  ['GET', '/v1/clean/plan', 'token', 'plan of stale caches (?want_bytes=N)'],
  ['POST', '/v1/clean/plan', 'token', '{paths:[…]} or {stale:true} → plan'],
  ['POST', '/v1/clean', 'token', '{items:[…]} from a plan → per-item results'],
  ['GET', '/v1/sweep', 'token', 'plan of idle agent worktrees'],
  ['POST', '/v1/sweep', 'token', '{items:[…]} from the sweep plan'],
  ['POST', '/v1/worktree/plan', 'token', '{paths:[…], allow_dirty} → plan'],
  ['POST', '/v1/worktree/remove', 'token', '{items:[…]} from the worktree plan'],
  ['POST', '/v1/process', 'token', '{pid, action: term|kill|stop|cont|renice, nice?}'],
  ['POST', '/v1/workspace', 'token', '{path, action: pause|resume|deprioritize|stop_builds|kill_all}'],
  ['POST', '/v1/prune', 'token', '{key: npm|bun|pnpm|yarn|uv|pip|homebrew|go|simulators}'],
  ['POST', '/v1/lease', 'token', '{label, pid?, ttl_secs?, ticket?} → granted, jobs, position'],
  ['POST', '/v1/release', 'token', '{ticket}'],
];

export async function renderAgents() {
  const I = await loadIntegration();
  const base = I.port ? `http://127.0.0.1:${I.port}` : '(the API is not running)';
  const cli = I.cli_linked || I.cli_path || 'mulchie';
  const tok = I.token
    ? `<span id="tok">••••••••••••</span> <button class="btn sm" id="showTok">show</button> <button class="btn sm" data-copy="token">copy</button>`
    : `<span class="warn">${esc(I.token_error || 'not available')}</span>`;
  const who = I.mode === 'client' ? '<dt>served by</dt><dd>another Mulchie engine (this app is its client)</dd>' : I.mode === 'engine' ? '<dt>served by</dt><dd>this app</dd>' : '';
  $('#apiKv').innerHTML = `<dt>base url</dt><dd>${esc(base)}</dd>${who}<dt>token file</dt><dd>${esc(tilde(I.token_path))}</dd><dt>token</dt><dd>${tok}</dd>`;
  $('#curlEx').textContent = `TOKEN=$(cat ${tilde(I.token_path)})\ncurl -s -H "Authorization: Bearer $TOKEN" ${base}/v1/advice\ncurl -s -H "Authorization: Bearer $TOKEN" ${base}/v1/sweep    # a plan; POST the approved items back`;
  $('#cliInfo').innerHTML = I.cli_linked
    ? `Linked at <span class="mono">${esc(tilde(I.cli_linked))}</span>.`
    : I.cli_path ? 'Bundled with the app. Link it to run <span class="mono">mulchie</span> from any shell.'
      : 'Not bundled in this build. Install with <span class="mono">cargo install mulchie</span> or <span class="mono">brew install williamrogersdev/tap/mulchie-cli</span>.';
  $('#installCli').disabled = !I.cli_path;
  $('#installCli').textContent = I.cli_linked ? 'Relink ~/.local/bin/mulchie' : 'Link mulchie into ~/.local/bin';
  $('#mcpJson').textContent = JSON.stringify({ mcpServers: { mulchie: { command: cli, args: ['mcp'] } } }, null, 2);
  $('#mcpCmd').textContent = `claude mcp add --scope user mulchie -- ${cli} mcp`;
  $('#agentsMd').textContent = `## Shared machine
Many agents run on this machine at once. Mulchie (MCP server "mulchie", or the \`mulchie\` CLI) tracks CPU, memory, disk and build slots.
- Before a heavy build or test suite, call \`can_i_build\` (or run \`mulchie advice\`). If it says wait, do lighter work first.
- Run heavy commands through the build queue so they get a fair job count:
  \`mulchie run --label "<repo>: <task>" -- <command>\`
- Do not delete worktrees or caches by hand. Use \`sweep_worktrees\` or \`clean_caches\`, which keep branches and back up uncommitted work.
- When the machine is slow, check \`top_processes\` and \`list_workspaces\` before stopping anything.`;
  $('#endpoints').innerHTML = th([{ l: 'Method' }, { l: 'Path' }, { l: 'Auth' }, { l: 'Body / returns' }]) + ENDPOINTS.map(e => `<tr><td class="mono">${e[0]}</td><td class="mono">${e[1]}</td><td class="dim">${e[2]}</td><td class="muted">${esc(e[3])}</td></tr>`).join('');
}
