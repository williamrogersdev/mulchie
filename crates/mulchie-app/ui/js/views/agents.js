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
  ['GET', '/v1/health', 'none', 'health (or stale), sample age, disk, load, free build slots'],
  ['GET', '/v1/state', 'any token', 'full snapshot: processes, workspaces, scan, disk map, slots, actions'],
  ['GET', '/v1/history', 'any token', '30 minutes of 3 s samples'],
  ['GET', '/v1/advice', 'any token', 'ok_to_start_heavy_work, suggested_jobs, reasons'],
  ['POST', '/v1/scan', 'full token', 'rescan caches and the disk map'],
  ['GET', '/v1/clean/plan', 'any token', 'plan of stale caches (?want_bytes=N)'],
  ['POST', '/v1/clean/plan', 'any token', '{paths:[…]} or {stale:true} → plan'],
  ['POST', '/v1/clean', 'full token', '{items:[…]} from a plan → per-item results'],
  ['GET', '/v1/sweep', 'any token', 'plan of idle agent worktrees'],
  ['POST', '/v1/sweep', 'full token', '{items:[…]} from the sweep plan'],
  ['POST', '/v1/worktree/plan', 'any token', '{paths:[…], allow_dirty} → plan'],
  ['POST', '/v1/worktree/remove', 'full token', '{items:[…]} from the worktree plan'],
  ['POST', '/v1/process/plan', 'any token', '{pid, signal?, tree?}, {group, action} or {session, action, include_agent?} → plan with affected processes'],
  ['POST', '/v1/process/execute', 'full token', '{items:[…]} from a process plan'],
  ['GET', '/v1/process/{pid}', 'any token', 'cwd, parents, children, listening ports, signature, findings'],
  ['POST', '/v1/process', 'full token', '{pid, action, expected_start_time} (one signal or renice)'],
  ['POST', '/v1/workspace', 'full token', '{path, action: pause|resume|deprioritize|stop_builds|kill_all}'],
  ['GET', '/v1/findings', 'any token', 'observations (?severity=&kind=&dismissed=1), counts'],
  ['POST', '/v1/findings/dismiss', 'full token', '{id, days?} → until_unix'],
  ['POST', '/v1/findings/undismiss', 'full token', '{id}'],
  ['GET', '/v1/config', 'any token', '{config, config_error, path}'],
  ['PUT', '/v1/config', 'full token', '{config} (the whole config) → saved and applied'],
  ['PATCH', '/v1/config', 'full token', 'a JSON merge patch of the config (null resets a key) → the config in use'],
  ['GET', '/v1/sessions', 'any token', 'agent sessions: {enabled, write_bytes_available, sessions}'],
  ['GET', '/v1/sessions/{id}', 'any token', 'one session: processes, breaches, processes Mulchie paused'],
  ['GET', '/v1/sessions/{id}/events', 'any token', 'its activity record (?since=&limit=), page with next_since'],
  ['GET', '/v1/limits/status', 'any token', 'report_only, enforcing, breaches, hogs, paused processes'],
  ['POST', '/v1/hook', 'any token', '{client, event, pid, …} from an agent hook → decision, context, breaches'],
  ['POST', '/v1/guard/check', 'any token', '{command, cwd} → allow, warn or block, and why'],
  ['GET', '/v1/updates', 'any token', 'update checks: last result, next check, available'],
  ['POST', '/v1/updates/check', 'full token', 'check now (409 updates_off when checks are off)'],
  ['POST', '/v1/prune', 'full token', '{key: npm|bun|pnpm|yarn|uv|pip|homebrew|go|simulators}'],
  ['POST', '/v1/lease', 'any token*', '{label, pid?, ttl_secs?, ticket?} → granted, jobs, position, release_secret'],
  ['POST', '/v1/release', 'any token*', '{ticket, secret} (the release_secret from the lease reply)'],
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
  $('#connectNote').textContent = I.cli_linked ? '' : I.cli_other ? `${tilde(I.cli_other)}; the commands above use whichever mulchie is first on your PATH.` : I.cli_path ? 'Link the CLI below first so `mulchie` runs from any shell.' : '';
  $('#cliInfo').innerHTML = I.cli_linked
    ? `Linked at <span class="mono">${esc(tilde(I.cli_linked))}</span>.`
    : I.cli_other ? `<span class="warn">${esc(tilde(I.cli_other))}</span>. Mulchie will not replace it; remove it yourself to link this app's CLI.`
    : I.cli_path ? 'Bundled with the app. Link it to run <span class="mono">mulchie</span> from any shell.'
      : 'Not bundled in this build. Install with <span class="mono">cargo install mulchie</span> or <span class="mono">brew install williamrogersdev/tap/mulchie-cli</span>.';
  $('#installCli').disabled = !I.cli_path || !!I.cli_other && !I.cli_other.includes(' links to ');
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
  $('#endpoints').innerHTML = th([{ l: 'Method', w: '62px' }, { l: 'Path', w: '170px' }, { l: 'Auth', w: '86px', c: 'c-md' }, { l: 'Body / returns' }]) + ENDPOINTS.map(e => `<tr><td class="mono">${e[0]}</td><td class="mono">${e[1]}</td><td class="dim c-md">${e[2]}</td><td class="muted wrap">${esc(e[3])}</td></tr>`).join('') +
    '<tr><td colspan="4" class="dim wrap">* The read-only token may take a build slot only with ttl_secs and no pid, and release it only with the lease’s release_secret.</td></tr>';
}
