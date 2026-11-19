// The Issues view: findings about the same problem, grouped across processes
// and restarts (`GET /v1/issues`). The unresolved ones by default, each with
// how often it came back and since when; the status filter shows the rest.
// Resolve, Ignore and Reopen change the record only; Mulchie never acts on an
// issue. Only the engine that holds the engine lock keeps the issues, and the
// backend says so in words when this one does not.
import { st } from '../store.js';
import { invoke } from '../ipc.js';
import { $, into, $$, esc, th, agoCell, tilde } from '../util.js';
import { cached, stale } from '../fetchcache.js';
import { act } from '../act.js';
import { noticesHtml } from '../findings.js';

const FILTERS = { '': 'unresolved', open: 'open', regressed: 'regressed', resolved: 'resolved', ignored: 'ignored', all: 'any status' };
const STATUS_CHIP = { open: ['warn', 'open'], regressed: ['bad', 'regressed'], resolved: ['ok', 'resolved'], ignored: ['', 'ignored'] };
const DONE = { resolve: 'Marked resolved. Mulchie will say if it comes back.', ignore: 'Ignored. It stays in the list under Ignored.', reopen: 'Reopened.' };

const kindWords = k => String(k || '').replace(/_/g, ' ');

function row(i) {
  const [cls, word] = STATUS_CHIP[i.status] || ['', i.status];
  const unresolved = i.status === 'open' || i.status === 'regressed';
  const btn = (action, label, off, title) =>
    `<button class="btn sm" data-issue="${action}" data-id="${esc(i.id)}" ${off ? `disabled title="${esc(off)}"` : `title="${esc(title)}"`}>${label}</button>`;
  // A current finding backs the issue: the row opens it. An old one is gone.
  const open = i.active ? `class="click" role="button" tabindex="0" data-d="finding" data-dk="${esc(i.finding_id)}"` : '';
  return `<tr ${open}>
    <td title="${esc(i.title)}"><span class="cell"><span class="chip ${cls}">${esc(word)}</span>${i.active ? ' <span class="chip warn" title="A finding for it is current now">now</span>' : ''} ${esc(i.title)}</span>
      <span class="cell sub" title="${esc(i.culprit)}">${esc(i.culprit || '—')}${i.workspace ? ` · <span title="${esc(i.workspace)}">${esc(tilde(i.workspace))}</span>` : ''}</span></td>
    <td class="c-lg"><span class="chip">${esc(kindWords(i.kind))}</span></td>
    <td class="n" title="Separate findings it was; one that lasts counts once">${i.occurrences}</td>
    <td class="n c-md">${agoCell(i.first_seen_unix)}</td>
    <td class="n">${agoCell(i.last_seen_unix)}</td>
    <td class="n c-md" title="Times it came back after being resolved">${i.regressions ? `<span class="bad">${i.regressions}</span>` : '<span class="dim">0</span>'}</td>
    <td class="n">${btn('resolve', 'Resolve', i.status === 'resolved' ? 'Already resolved' : '', 'Mark it fixed; Mulchie says if it comes back')}
      ${btn('ignore', 'Ignore', i.status === 'ignored' ? 'Already ignored' : '', 'Keep the record but stop counting it as something to look at')}
      ${btn('reopen', 'Reopen', unresolved ? 'It is not resolved or ignored' : '', 'Put it back among the open issues')}</td></tr>`;
}

export function renderIssues() {
  const ui = st.ui, f = ui.iF ?? '';
  $$('#iSeg button').forEach(b => b.classList.toggle('on', b.dataset.f === f));
  const e = cached('issues:' + f, 'issues', f ? { status: f } : {}, 4000);
  const list = e.data?.issues;
  $('#iCount').textContent = list ? `${list.length} ${FILTERS[f] || f}` : '';
  into($('#iNote')).html = e.error ? noticesHtml([['bad', 'Could not load the issues', e.error]]) : '';
  const empty = !list ? (e.error ? '' : 'Loading…')
    : f === '' ? 'No unresolved issues. Issues appear when the same performance, hygiene or limit finding keeps coming back.'
    : `No ${FILTERS[f] || f} issues.`;
  into($('#issues')).html = th([{ l: 'Issue, culprit and workspace' }, { l: 'Kind', w: '140px', c: 'c-lg' },
    { l: 'Seen', n: 1, w: '48px' }, { l: 'First', n: 1, w: '54px', c: 'c-md' }, { l: 'Last', n: 1, w: '54px' }, { l: 'Back', n: 1, w: '48px', c: 'c-md' }, { l: '', w: '210px' }]) +
    ((list || []).map(row).join('') || `<tr><td colspan="7" class="empty">${esc(empty)}</td></tr>`);
}

export function issueFilter(f) {
  st.ui.iF = f;
}

// Resolve, Ignore or Reopen the issue on a row's button.
export function issueAction(t) {
  const { id, issue: action } = t.dataset;
  stale('issues');
  return act(() => invoke('issue_status', { id, action }), DONE[action]);
}
