// The Performance view: the performance findings with their evidence and the
// commits (and agent sessions) in between, then compact lists of what the
// findings come from: build and test timings, slow endpoints from traces, dev
// server runs and production deploys. Findings come from the polled state;
// the lists are fetched while the view is open (see fetchcache.js). Nothing
// here acts on anything.
import { st } from '../store.js';
import { $, into, esc, B, ago, agoCell, tilde } from '../util.js';
import { cached } from '../fetchcache.js';
import { SEV_LVL, SEV_TEXT, watchFindings } from '../findings.js';

const TTL = 8000;
const AGENT = { claude: 'Claude Code', codex: 'Codex', gemini: 'Gemini CLI', copilot: 'Copilot CLI', 'cursor-agent': 'Cursor', aider: 'aider', goose: 'goose', opencode: 'opencode' };
export const agentName = a => AGENT[a] || a || 'an agent';

// 120 ms, 2.4 s, 45 s, 3m05s.
export function ms(n) {
  if (n == null || !Number.isFinite(+n)) return '—';
  n = +n;
  if (n < 1000) return `${Math.round(n)} ms`;
  if (n < 10000) return `${(n / 1000).toFixed(1)} s`;
  if (n < 60000) return `${Math.round(n / 1000)} s`;
  return `${Math.floor(n / 60000)}m${String(Math.round(n % 60000 / 1000)).padStart(2, '0')}s`;
}
const times = r => `${r >= 10 ? Math.round(r) : (+r).toFixed(1)}×`;
const base = p => String(p || '').split('/').filter(Boolean).pop() || p || '';
const sess = sid => `<a class="lnk mono" role="button" tabindex="0" data-d="session" data-dk="${esc(sid)}">${esc(sid)}</a>`;

// One commit of a finding's range, from a recorded move of HEAD (`head`) or
// a deploy's commit (`sha`, with `made`): who made it, when.
export function commitLine(c) {
  const sha = c.sha || c.head || '';
  const made = c.made || (c.session_id ? 'session' : 'no_session');
  const who = made === 'session' ? `made by ${esc(agentName(c.agent))} in session ${sess(c.session_id)}`
    : made === 'made_elsewhere' ? 'made elsewhere; this machine never recorded it'
    : 'no agent session was working then';
  return `<li><span class="mono">${esc(sha.slice(0, 8))}</span> ${c.branch ? `<span class="dim">${esc(c.branch)}</span> ` : ''}${who}${c.at_unix ? ` <span class="dim">· ${esc(ago(c.at_unix))} ago</span>` : ''}</li>`;
}
export function commitsHtml(list, truncated) {
  if (!list?.length) return '<div class="dim">No commits were recorded between the two points on this machine.</div>';
  return `<ul class="clist">${list.map(commitLine).join('')}</ul>${truncated ? '<div class="dim">More commits than listed; the newest are shown.</div>' : ''}`;
}

// What a finding measured, in a line or two, and its commits.
function evidenceHtml(f) {
  const e = f.evidence || {};
  if (e.perf) {
    const p = e.perf;
    return `<div class="mono ev-cmd" title="${esc(p.command)}">${esc(p.command)}</div>
      <div>Usually ${ms(p.baseline_ms)} over ${p.baseline_runs} runs; the latest slow runs took ${esc((p.recent_ms || []).map(ms).join(', '))} (${times(p.ratio)} the usual).</div>${commitsHtml(p.commits, p.commits_truncated)}`;
  }
  if (e.devserver) {
    const d = e.devserver, mem = d.measure === 'settled_memory', v = x => mem ? B(x) : ms(x);
    return `<div class="mono ev-cmd" title="${esc(d.command)}">${esc(d.command)}</div>
      <div>${mem ? 'Settled memory' : 'Time to listen'}: usually ${v(d.baseline)} over ${d.baseline_runs} runs; the latest runs ${esc((d.recent || []).map(v).join(', '))} (${times(d.ratio)} the usual).${d.uncertainty_ms ? ` <span class="dim">The time to listen is only as exact as the watch interval (up to ${ms(d.uncertainty_ms)} lower).</span>` : ''}</div>${commitsHtml(d.commits, d.commits_truncated)}`;
  }
  if (e.endpoint) {
    const p = e.endpoint, errs = f.kind === 'endpoint_errors';
    const line = errs
      ? `${(p.recent_error_rate * 100).toFixed(1)}% of ${p.recent_count} requests failed in the last ${p.recent_minutes} min; usually ${(p.baseline_error_rate * 100).toFixed(1)}%.`
      : `p95 ${ms(p.recent_p95_ms)} over the last ${p.recent_minutes} min (${p.recent_count} requests); usually ${ms(p.baseline_p95_ms)} (${times(p.ratio)}).`;
    return `<div class="mono ev-cmd">${esc(p.service)} ${esc(p.operation)}</div><div>${line}</div>${p.attribution?.workspace ? `<div class="dim">Runs from ${esc(tilde(p.attribution.workspace))}.</div>` : ''}${commitsHtml(p.commits, p.commits_truncated)}`;
  }
  if (e.deploy) {
    const d = e.deploy;
    return `<div>${esc(d.metric_label)} on ${esc(d.provider)}: ${ms(d.before_ms)} before, ${ms(d.after_ms)} after deploy <span class="mono">${esc((d.sha || d.deploy_id || '').slice(0, 10))}</span> (${times(d.ratio)}).</div>${commitsHtml(d.commits, d.commits_truncated)}`;
  }
  return '';
}

function findingHtml(f) {
  const lvl = SEV_LVL[f.severity] || 'info';
  return `<div class="pfind"><div class="pf-h"><span class="chip ${lvl}">${esc(SEV_TEXT[f.severity] || f.severity)}</span>
    <a class="lnk pf-t" role="button" tabindex="0" data-d="finding" data-dk="${esc(f.id)}">${esc(f.title)}</a> <span class="chip">${esc(f.kind.replace(/_/g, ' '))}</span></div>
    <div class="muted">${esc(f.why)}</div>${evidenceHtml(f)}</div>`;
}

const card = (title, body, note = '') => `<div class="card"><div class="chead"><h3>${esc(title)}</h3>${note ? `<span class="dim c-md">${note}</span>` : ''}</div>${body}</div>`;
const none = text => `<div class="dim">${text}</div>`;
const mini = (cols, rows) => `<table class="mini-t wrap-tbl"><colgroup>${cols.map(w => `<col${w ? ` style="width:${w}"` : ''}>`).join('')}</colgroup>${rows.join('')}</table>`;
const where = r => [r.workspace ? base(r.workspace) : '', r.branch].filter(Boolean).join(' · ');

function timingsCard() {
  const e = cached('perf:timings', 'timings', { limit: 30 }, TTL), list = e.data?.timings;
  const body = e.error ? none(`Could not load: ${esc(e.error)}`) : !list ? none('Loading…') : !list.length
    ? none('No timings yet. <span class="mono">mulchie run -- &lt;build or test command&gt;</span> records one; agents send theirs through MCP.')
    : mini(['58px', '', '64px', '30%'], list.slice(0, 10).map(t => `<tr title="${esc(t.command)}"><td class="dim">${agoCell(t.at_unix)}</td>
      <td><span class="cell mono">${esc(t.key || t.command)}</span></td>
      <td class="n">${ms(t.duration_ms)}${t.exit_code ? ` <span class="bad" title="exit code ${t.exit_code}">✕</span>` : ''}</td>
      <td class="dim"><span class="cell">${esc(where(t))}${t.agent ? ` · ${esc(agentName(t.agent))}` : ''}</span></td></tr>`));
  return card('Builds and tests', body, e.data ? `${e.data.total} recorded` : '');
}

function otelCard() {
  const cfg = st.S?.config?.otel;
  if (!cfg?.enabled) {
    return card('Slow endpoints (traces)', none('Traces are off; see <a class="lnk" role="button" tabindex="0" data-go="settings">Settings</a>.'));
  }
  const e = cached('perf:otel', 'otel_report', { minutes: 15 }, TTL), o = e.data;
  let body;
  if (e.error) body = none(`Could not load: ${esc(e.error)}`);
  else if (!o) body = none('Loading…');
  else if (!o.listening) body = `<div class="warn">The trace receiver is not listening${o.error ? `: ${esc(o.error)}` : ''}. Only the engine that holds the engine lock runs it.</div>`;
  else {
    const ops = (o.services || []).flatMap(s => (s.operations || []).map(x => ({ ...x, service: s.name })))
      .sort((a, b) => (b.slower || b.more_errors) - (a.slower || a.more_errors) || b.p95_ms - a.p95_ms);
    body = `<div class="dim" style="margin-bottom:6px">Receiving at <span class="mono">${esc(o.endpoint || '')}</span> · ${o.recorded_spans} spans recorded</div>` + (!ops.length
      ? none(`No requests in the last ${o.minutes} minutes. Point an app's <span class="mono">OTEL_EXPORTER_OTLP_ENDPOINT</span> at the address above.`)
      : mini(['', '78px', '58px', '52px'], ops.slice(0, 8).map(x => `<tr title="${esc(x.service)} ${esc(x.operation)}"><td><span class="cell"><span class="dim">${esc(x.service)}</span> <span class="mono">${esc(x.operation)}</span></span></td>
        <td class="n">${ms(x.p95_ms)}${x.baseline ? `<span class="dim"> / ${ms(x.baseline.p95_ms)}</span>` : ''}</td>
        <td class="n">${x.errors ? `<span class="${x.more_errors ? 'bad' : ''}">${(x.error_rate * 100).toFixed(0)}% err</span>` : ''}${x.slower ? ' <span class="chip warn">slower</span>' : ''}</td><td class="n dim">${x.count}</td></tr>`)) +
      '<div class="dim" style="margin-top:4px">p95 now / usual p95, then the failure rate and the request count.</div>');
  }
  return card('Slow endpoints (traces)', body, `last ${o?.minutes || 15} min`);
}

function devserversCard() {
  const e = cached('perf:dev', 'devservers', { limit: 20 }, TTL), list = e.data?.dev_servers;
  const body = e.error ? none(`Could not load: ${esc(e.error)}`) : !list ? none('Loading…') : !list.length
    ? none('No dev server runs recorded. A process in a workspace that listens on a port for a minute or more is recorded as one.')
    : mini(['58px', '', '82px', '64px', '58px'], list.slice(0, 8).map(r => `<tr title="${esc(r.command)}"><td class="dim">${agoCell(r.start_unix)}</td>
      <td><span class="cell"><span class="dim">${esc(base(r.workspace))}</span> <span class="mono">${esc(r.key || r.command)}</span>${r.end === 'running' ? ' <span class="chip ok">running</span>' : ''}</span></td>
      <td class="n" title="Process start to the first watch run that saw it listening: an upper bound">${r.listen_ms != null ? `≤ ${ms(r.listen_ms)}` : '<span class="dim">—</span>'}</td>
      <td class="n" title="Memory it settled at">${r.settled_mem_bytes ? B(r.settled_mem_bytes) : '<span class="dim">—</span>'}</td>
      <td class="n dim" title="Peak CPU, percent of one core">${r.peak_cpu_pct != null ? Math.round(r.peak_cpu_pct) + '%' : ''}</td></tr>`)
      .concat([`<tr><td colspan="5" class="dim">Columns: started, command, time to listen, settled memory, peak CPU.</td></tr>`]));
  return card('Dev servers', body, e.data ? `${e.data.total} runs` : '');
}

const METRIC = { build_duration: 'build', p95: 'p95', lcp_p75: 'LCP', inp_p75: 'INP' };
function deploysCard() {
  const i = cached('perf:integ', 'integrations_status', {}, TTL), provs = (i.data?.providers || []).filter(p => p.enabled);
  if (i.error) return card('Production deploys', none(`Could not load: ${esc(i.error)}`));
  if (!i.data) return card('Production deploys', none('Loading…'));
  if (!provs.length) {
    return card('Production deploys', none('No integrations connected. Connect Vercel, Sentry or PostHog with <span class="mono">mulchie integrations add</span> to see deploys and how they performed.'));
  }
  const d = cached('perf:deploys', 'deploys', {}, TTL), list = d.data?.deploys;
  const errs = provs.filter(p => p.last_error).map(p => `<div class="warn">${esc(p.provider)}: ${esc(p.last_error)}</div>`).join('');
  const body = d.error ? none(`Could not load: ${esc(d.error)}`) : !list ? none('Loading…') : !list.length
    ? none('No deploys yet; the first poll runs a minute after start.')
    : mini(['66px', '', '', '34%'], list.slice(0, 8).map(x => {
      const made = [...new Set((x.commits || []).filter(c => c.made === 'session').map(c => agentName(c.agent)))];
      return `<tr><td class="dim">${esc(x.provider)}</td><td><span class="cell mono" title="${esc(x.id)}">${esc((x.sha || x.id).slice(0, 10))}</span></td>
        <td class="n">${(x.metrics || []).map(m => `${METRIC[m.metric] || m.metric} ${ms(m.value_ms)}`).join(' · ') || '<span class="dim">—</span>'}</td>
        <td class="dim"><span class="cell" title="${esc((x.commits || []).length)} commit(s) since the deploy before">${agoCell(x.at_unix)} · ${(x.commits || []).length} commit(s)${made.length ? `, ${esc(made.join(', '))}` : ''}</span></td></tr>`;
    }));
  return card('Production deploys', errs + body, 'from your connected providers');
}

export function renderPerformance() {
  const S = st.S;
  const list = watchFindings(S).filter(f => f.category === 'performance');
  into($('#perfFindings')).html = list.length ? list.map(findingHtml).join('')
    : none('No performance findings. Mulchie raises one when a build, dev server, endpoint or production number gets much slower than usual; the thresholds are under Settings.');
  $('#perfCount').textContent = list.length ? `${list.length} finding${list.length === 1 ? '' : 's'}` : '';
  into($('#perfLists')).html = [timingsCard(), otelCard(), devserversCard(), deploysCard()].join('');
}
