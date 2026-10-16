// Self-repair: what Aqua could not do (gaps), the fixes it proposes for them (proposals the person approves, one by one, and can undo), and the
// report of what it did by itself lately (documentation/SELF_REPAIR.md has the shapes; core/src/api_gateway/self_repair_api.rs has the routes).
// Every route here needs the UI token, reads included, which the shell or the dev proxy adds:
//
//   GET  /api/v1/self-repair/gaps?status=             the gaps, newest first (`open` is new and proposed)
//   POST /api/v1/self-repair/gaps/{id}/dismiss        the person does not want to see this gap again
//   POST /api/v1/self-repair/gaps/{id}/propose        makes the fixes a gap suggests; nothing is applied
//   GET  /api/v1/self-repair/proposals?status=&gap=   the proposals, newest first
//   POST /api/v1/self-repair/proposals/{id}/approve   approves and applies one; 409 {error, proposal} when it could not be applied. For a folder the
//                                                     core waits until the person has answered the tool host's own question, so this call can last
//   POST /api/v1/self-repair/proposals/{id}/reject    says no; nothing changes anywhere
//   POST /api/v1/self-repair/proposals/{id}/undo      takes an applied change back; 409 with the reason when it cannot
//   GET  /api/v1/self-repair/report?days=             what Aqua noticed, looked up, proposed, applied and undid, and what has no undo
//
// Nothing here applies anything by itself. Text from the core (a summary, what the person asked, a path, a reason) is made visible (see `shownText`)
// and only ever handed to React as a text child. A missing field is never read as good news: a report that does not say which changes have no undo
// does not say there are none.

import { answerBody, callCore, CoreError, count, isObject, isWholeNumber, jsonInit, stringList, UNREADABLE_ANSWER, type CoreAnswer, type Wording } from './coreRequest.ts';
import { cutText, shownText, tidy } from './plainErrors.ts';
import { agoText, toMs, type Tone } from './runs.ts';

export type { Tone };

const SUMMARY_CHARS = 240;
const DETAIL_CHARS = 600;
const DRY_RUN_CHARS = 1600;
/** The longest reason from the core that is shown whole. */
const REASON_CHARS = 300;

function capitalise(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

function lowerFirst(text: string): string {
  return text.charAt(0).toLowerCase() + text.slice(1);
}

function maybeNumber(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function sentenceOf(value: unknown, max: number, keepBreaks = false): string {
  return cutText(shownText(value, keepBreaks).trim(), max);
}

// ---- gaps -----------------------------------------------------------------------------------------

/** What kind of thing Aqua could not do, as a noun phrase in lower case (the core's kinds; one it does not know gets a plain stand-in). */
const KIND_PHRASE: Record<string, string> = {
  platform_not_built: 'a platform that is not built',
  step_not_runnable: 'a step that cannot run yet',
  folder_not_allowed: 'a folder that is not allowed',
  tool_unavailable: 'a tool that is not available',
  credential_missing: 'a sign-in or key that is missing',
  other: 'something else it could not do',
};

export function gapKindPhrase(kind: string): string {
  return KIND_PHRASE[kind] ?? 'something it could not do';
}

/** The kind as a title for a row: "A platform that is not built". */
export function gapKindWords(kind: string): string {
  return capitalise(gapKindPhrase(kind));
}

const GAP_STATUS_WORDS: Record<string, string> = {
  new: 'New',
  proposed: 'A fix is proposed',
  fixed: 'Fixed',
  dismissed: 'Dismissed',
};

export function gapStatusWords(status: string): string {
  return GAP_STATUS_WORDS[status] ?? 'Unknown';
}

export function gapStatusTone(status: string): Tone {
  if (status === 'fixed') return 'ok';
  if (status === 'new' || status === 'proposed') return 'attention';
  return 'neutral';
}

export interface Gap {
  id: number;
  kind: string;
  summary: string;
  detail: string;
  /** What the person asked for, in their own words, when there was a request. */
  asked: string;
  /** How many times it happened. */
  count: number;
  firstMs: number | null;
  lastMs: number | null;
  status: string;
  /** For a folder: did the person's own words name it? null when the core does not say. */
  namedByPerson: boolean | null;
  /** For a folder: Aqua never hands this kind of place out. null when not said. */
  neverHandedOut: boolean | null;
}

/** One gap, or null when it has no whole-number id (it could not be answered about). A missing field is empty; a missing count is 1 (it happened once). */
export function mapGap(raw: unknown): Gap | null {
  if (!isObject(raw) || !isWholeNumber(raw.id) || raw.id <= 0) return null;
  const data = isObject(raw.data) ? raw.data : {};
  const seen = maybeNumber(raw.count);
  return {
    id: raw.id,
    kind: typeof raw.kind === 'string' ? raw.kind : '',
    summary: sentenceOf(raw.summary, SUMMARY_CHARS),
    detail: sentenceOf(raw.detail, DETAIL_CHARS, true),
    asked: sentenceOf(raw.asked, SUMMARY_CHARS),
    count: seen !== null && seen >= 1 ? Math.floor(seen) : 1,
    firstMs: toMs(raw.at),
    lastMs: toMs(raw.last_at),
    status: typeof raw.status === 'string' && raw.status !== '' ? raw.status : 'new',
    namedByPerson: typeof data.named_by_person === 'boolean' ? data.named_by_person : null,
    neverHandedOut: typeof data.refused_by_guard === 'boolean' ? data.refused_by_guard : null,
  };
}

/** Newest first (the last time it happened); among gaps that happened together, the larger id first. */
export function newestGapsFirst(gaps: Gap[]): Gap[] {
  return [...gaps].sort((a, b) => (b.lastMs ?? b.firstMs ?? 0) - (a.lastMs ?? a.firstMs ?? 0) || b.id - a.id);
}

/** The list route's answer, newest first, or null when it is not a list at all. A row with no usable id is left out. */
export function mapGapList(body: unknown): Gap[] | null {
  if (!isObject(body) || !Array.isArray(body.gaps)) return null;
  const gaps: Gap[] = [];
  for (const row of body.gaps) {
    const gap = mapGap(row);
    if (gap) gaps.push(gap);
  }
  return newestGapsFirst(gaps);
}

/** How often and when: "Happened once, 3 h ago." / "Happened 4 times, the last 2 h ago." `nowMs` is given so the answer does not depend on the clock. */
export function gapTimesText(g: Gap, nowMs: number, formatDate?: (ms: number) => string): string {
  const when = g.lastMs ?? g.firstMs;
  if (g.count <= 1) return when === null ? 'Happened once.' : `Happened once, ${agoText(when, nowMs, formatDate)}.`;
  return when === null ? `Happened ${g.count} times.` : `Happened ${g.count} times, the last ${agoText(when, nowMs, formatDate)}.`;
}

/** For a folder, whether Aqua will offer to allow it, in words. Empty for anything else, and when the core does not say. */
export function gapFolderText(g: Gap): string {
  if (g.kind !== 'folder_not_allowed') return '';
  if (g.neverHandedOut === true) return 'Aqua never hands this kind of place out (Windows, programs, credentials, its own data), so it makes no offer.';
  if (g.namedByPerson === true) return 'Your own words named this folder, so Aqua can offer to allow it.';
  if (g.namedByPerson === false) return 'Your request did not name this folder, so Aqua does not offer to allow it.';
  return '';
}

export interface GapActions {
  propose: boolean;
  dismiss: boolean;
}

/** What may be done with a gap: a fix is asked for while it is open, and it can be dismissed while it is open. A fixed gap that comes back is new again. */
export function gapActions(g: Gap): GapActions {
  const open = g.status === 'new' || g.status === 'proposed';
  return { propose: open, dismiss: open };
}

// ---- proposals ------------------------------------------------------------------------------------

const PROPOSAL_KIND_WORDS: Record<string, string> = {
  allow_folder: 'Allow a folder',
  switch_platform: 'Post somewhere else',
  switch_off_step: 'Switch a step off',
};

export function proposalKindWords(kind: string): string {
  return PROPOSAL_KIND_WORDS[kind] ?? 'A change';
}

const PROPOSAL_STATUS_WORDS: Record<string, string> = {
  pending: 'Waiting for your answer',
  approved: 'Being applied',
  rejected: 'Rejected',
  applied: 'Applied',
  undone: 'Undone',
  failed: 'Not applied',
};

export function proposalStatusWords(status: string): string {
  return PROPOSAL_STATUS_WORDS[status] ?? 'Unknown';
}

export function proposalTone(status: string): Tone {
  if (status === 'applied') return 'ok';
  if (status === 'failed') return 'problem';
  if (status === 'pending' || status === 'approved') return 'attention';
  return 'neutral';
}

export interface Proposal {
  id: number;
  gapId: number | null;
  /** `allow_folder`, `switch_platform` or `switch_off_step`. */
  kind: string;
  title: string;
  /** One line: why this fix. */
  reason: string;
  /** What would change, in words, worked out when the proposal was made. */
  dryRun: string;
  /** `pending`, `approved` (being applied), `rejected`, `applied`, `undone` or `failed`. */
  status: string;
  atMs: number | null;
  approvedBy: string;
  appliedMs: number | null;
  undoneMs: number | null;
  /** Why it was not applied, or why it was rejected. */
  error: string;
  /** How the change is taken back: `flow_version`, `revoke_folder`, `nothing`, or '' when none is recorded. */
  undoKind: string;
}

/** One proposal, or null when it has no whole-number id. A missing field is empty; a status the core did not say is not guessed (it is `unknown`, and no button is offered). */
export function mapProposal(raw: unknown): Proposal | null {
  if (!isObject(raw) || !isWholeNumber(raw.id) || raw.id <= 0) return null;
  const undo = isObject(raw.undo) ? raw.undo : {};
  return {
    id: raw.id,
    gapId: isWholeNumber(raw.gap_id) ? raw.gap_id : null,
    kind: typeof raw.kind === 'string' ? raw.kind : '',
    title: sentenceOf(raw.title, SUMMARY_CHARS),
    reason: sentenceOf(raw.reason, SUMMARY_CHARS),
    dryRun: sentenceOf(raw.dry_run, DRY_RUN_CHARS, true),
    status: typeof raw.status === 'string' && raw.status !== '' ? raw.status : 'unknown',
    atMs: toMs(raw.at),
    approvedBy: sentenceOf(raw.approved_by, 40),
    appliedMs: toMs(raw.applied_at),
    undoneMs: toMs(raw.undone_at),
    error: sentenceOf(raw.error, REASON_CHARS),
    undoKind: typeof undo.kind === 'string' ? undo.kind : '',
  };
}

/** Newest first (the larger id is the newer proposal). */
export function newestProposalsFirst(proposals: Proposal[]): Proposal[] {
  return [...proposals].sort((a, b) => b.id - a.id);
}

/** The list route's answer, newest first, or null when it is not a list at all. */
export function mapProposalList(body: unknown): Proposal[] | null {
  if (!isObject(body) || !Array.isArray(body.proposals)) return null;
  const list: Proposal[] = [];
  for (const row of body.proposals) {
    const p = mapProposal(row);
    if (p) list.push(p);
  }
  return newestProposalsFirst(list);
}

/** The proposals that wait for the person (or are being applied), and the ones that are decided. */
export function splitProposals(list: Proposal[]): { waiting: Proposal[]; decided: Proposal[] } {
  const waiting = list.filter((p) => p.status === 'pending' || p.status === 'approved');
  return { waiting, decided: list.filter((p) => !waiting.includes(p)) };
}

export interface ProposalActions {
  approve: boolean;
  reject: boolean;
  undo: boolean;
}

/**
 * Which buttons a proposal has. Only a pending one can be approved or rejected; only an applied one can be undone, and one whose recorded undo is "nothing
 * to undo" (the folder was already allowed) has no use for it. A status the page does not know has none: the core is asked, never guessed.
 */
export function proposalActions(p: Proposal): ProposalActions {
  return { approve: p.status === 'pending', reject: p.status === 'pending', undo: p.status === 'applied' && p.undoKind !== 'nothing' };
}

/** Who approved it, in words. The app's own click is recorded as `ui`. */
export function approvedByWords(by: string): string {
  const who = by.trim();
  if (who === '') return 'nobody recorded';
  if (who === 'ui') return 'you, in the app';
  return who;
}

/** What the person must know before approving or after the change, said plainly. Empty for an ordinary flow change. */
export function proposalNotes(p: Proposal): string[] {
  const out: string[] = [];
  if (p.status === 'approved') out.push('This is being applied now. If it stays like this for long, Aqua may have been closed while it was applied: ask for the fix again on the gap.');
  if (p.kind === 'allow_folder') {
    if (p.status === 'pending') {
      out.push('When you approve, the app asks you once more before the folder is allowed. An allowed folder lasts until Aqua restarts; Aqua cannot take it back sooner.');
    } else if (p.status === 'applied' && p.undoKind === 'nothing') {
      out.push('The folder was already allowed, so nothing was changed.');
    } else if (p.status === 'applied') {
      out.push('This folder stays allowed until Aqua restarts. Aqua cannot take it back sooner, so Undo may be refused.');
    }
  }
  return out;
}

/** The line about what became of a decided proposal: when it was applied and by whom, when it was undone, why it was not applied or was rejected. */
export function proposalOutcomeText(p: Proposal, nowMs: number, formatDate?: (ms: number) => string): string {
  if (p.status === 'applied') {
    const when = p.appliedMs === null ? '' : `, ${agoText(p.appliedMs, nowMs, formatDate)}`;
    return `Applied${when}. Approved by ${approvedByWords(p.approvedBy)}.`;
  }
  if (p.status === 'undone') {
    const when = p.undoneMs === null ? '' : `, ${agoText(p.undoneMs, nowMs, formatDate)}`;
    return `Undone${when}.`;
  }
  if (p.status === 'failed') {
    return p.error === '' ? 'It was not applied. Nothing was changed.' : `It was not applied: ${lowerFirst(p.error.replace(/[.!?]+$/, ''))}. Nothing was changed.`;
  }
  if (p.status === 'rejected') {
    const why = p.error.replace(/[.!?]+$/, '');
    return why === '' || why === 'rejected by the person' ? 'Rejected. Nothing was changed.' : `Rejected: ${why}. Nothing was changed.`;
  }
  return '';
}

/** Which gap a proposal is for, in words: the gap's kind and summary when the gap is on the screen, else its number. Empty when the core names no gap. */
export function proposalGapText(p: Proposal, gaps: Gap[]): string {
  if (p.gapId === null) return '';
  const g = gaps.find((x) => x.id === p.gapId);
  if (!g) return `For gap number ${p.gapId}.`;
  return g.summary === '' ? `For: ${lowerFirst(gapKindWords(g.kind))}.` : `For: ${g.summary.replace(/[.!?]+$/, '')}.`;
}

/** What to say about a proposal that is waiting on the core: the whole screen's "waiting" state for one approve. */
export function applyingText(p: Proposal): string {
  if (p.kind === 'allow_folder') return 'Waiting for your answer in the approval prompt. Aqua asks once more before it allows the folder.';
  return 'Applying the change. Nothing is changed until the core answers.';
}

/** The sentence after a change was applied. */
export function appliedSentence(p: Proposal): string {
  const title = p.title === '' ? 'The change' : p.title;
  const tail =
    p.kind === 'allow_folder'
      ? ' The folder stays allowed until Aqua restarts.'
      : p.kind === 'switch_platform' || p.kind === 'switch_off_step'
        ? ' It is a new version of the flow. Undo puts the earlier version back.'
        : '';
  return `Applied: ${title}.${tail}`;
}

/** The sentence after a change was taken back. */
export function undoneSentence(p: Proposal): string {
  const title = p.title === '' ? 'The change' : p.title;
  return `Undone: ${title}.`;
}

export interface Proposed {
  proposals: Proposal[];
  /** Why there are none, when there are none. */
  note: string;
}

/** The answer of "propose a fix": the proposals that wait, and the core's word when there are none. */
export function mapProposed(body: unknown): Proposed | null {
  const list = mapProposalList(body);
  if (list === null || !isObject(body)) return null;
  return { proposals: list, note: sentenceOf(body.note, REASON_CHARS) };
}

/** The sentence after a fix was asked for. */
export function proposedSentence(p: Proposed): string {
  const n = p.proposals.length;
  if (n === 0) return p.note !== '' ? tidy(p.note) : 'Aqua has no fix to offer for this one.';
  return `${n === 1 ? 'Aqua proposes 1 fix' : `Aqua proposes ${n} fixes`}. Nothing is applied until you approve one under Proposals.`;
}

// ---- the report -----------------------------------------------------------------------------------

export interface ReportChange {
  id: number | null;
  title: string;
  kind: string;
  status: string;
  appliedMs: number | null;
  undoneMs: number | null;
  approvedBy: string;
  /** The tool host asked again (for a folder): the kind of answer and who gave it. null when the log shows none. */
  hostAsked: { answer: string; by: string } | null;
  error: string;
}

export interface NoUndoItem {
  /** `tool_call` (an agent run's call that nothing can put back), `proposal` (an applied change that cannot be taken back) or `other`. */
  source: string;
  tool: string;
  title: string;
  runId: string;
  calls: number | null;
  lastMs: number | null;
  targets: string[];
  why: string;
}

export type Chain = 'holds' | 'broken' | 'not_checked' | 'unknown';

export interface Report {
  days: number;
  noticed: { total: number; fresh: number; again: number; byKind: { kind: string; count: number }[] } | null;
  lookedUp: { calls: number; failed: number; runs: number } | null;
  proposedTotal: number | null;
  applied: ReportChange[] | null;
  undone: ReportChange[] | null;
  rejected: number | null;
  notApplied: number | null;
  /** Changes nothing can take back. null when the core did not say: that is not "none". */
  noUndo: { count: number; total: number; notListed: number; items: NoUndoItem[] } | null;
  withUndoCalls: number | null;
  ledger: { coversWindow: boolean | null; chain: Chain; problem: string; lines: number | null; oldestMs: number | null };
  /** The core's own plain-text version. */
  text: string;
}

function mapReportChange(raw: unknown): ReportChange | null {
  if (!isObject(raw)) return null;
  const asked = isObject(raw.host_asked) && typeof raw.host_asked.kind === 'string' ? { answer: raw.host_asked.kind, by: sentenceOf(raw.host_asked.by, 40) } : null;
  return {
    id: isWholeNumber(raw.id) ? raw.id : null,
    title: sentenceOf(raw.title, SUMMARY_CHARS),
    kind: typeof raw.kind === 'string' ? raw.kind : '',
    status: typeof raw.status === 'string' ? raw.status : '',
    appliedMs: toMs(raw.applied_at),
    undoneMs: toMs(raw.undone_at),
    approvedBy: sentenceOf(raw.approved_by, 40),
    hostAsked: asked,
    error: sentenceOf(raw.error, REASON_CHARS),
  };
}

function changeList(value: unknown): ReportChange[] | null {
  if (!Array.isArray(value)) return null;
  const out: ReportChange[] = [];
  for (const row of value.slice(0, 100)) {
    const c = mapReportChange(row);
    if (c) out.push(c);
  }
  return out;
}

function mapNoUndoItem(raw: unknown): NoUndoItem | null {
  if (!isObject(raw)) return null;
  const calls = maybeNumber(raw.calls);
  return {
    source: typeof raw.source === 'string' ? raw.source : 'other',
    tool: sentenceOf(raw.tool, 80),
    title: sentenceOf(raw.title, SUMMARY_CHARS),
    runId: sentenceOf(raw.run_id, 80),
    calls: calls !== null && calls >= 0 ? Math.floor(calls) : null,
    lastMs: toMs(raw.last_at ?? raw.applied_at),
    targets: stringList(raw.targets).slice(0, 3).map((t) => sentenceOf(t, 160)),
    why: sentenceOf(raw.why, REASON_CHARS),
  };
}

function optionalCount(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? Math.floor(value) : null;
}

function mapChain(raw: unknown): { chain: Chain; problem: string } {
  if (!isObject(raw)) return { chain: 'unknown', problem: '' };
  if (raw.checked === false) return { chain: 'not_checked', problem: '' };
  if (raw.checked === true && raw.ok === true) return { chain: 'holds', problem: '' };
  if (raw.checked === true && raw.ok === false) return { chain: 'broken', problem: sentenceOf(raw.problem, REASON_CHARS) };
  return { chain: 'unknown', problem: '' };
}

/** The report, or null when the answer is not one (no `window`). A section the core did not send is `null`: it is shown as "not said", never as zero. */
export function mapReport(body: unknown): Report | null {
  if (!isObject(body) || !isObject(body.window)) return null;
  const w = body.window;
  const days = optionalCount(w.days);
  const n = isObject(body.noticed) ? body.noticed : null;
  const byKind: { kind: string; count: number }[] = [];
  if (n && isObject(n.by_kind)) {
    for (const [kind, c] of Object.entries(n.by_kind)) {
      const k = optionalCount(c);
      if (k !== null && k > 0) byKind.push({ kind, count: k });
    }
  }
  const noticedTotal = n ? optionalCount(n.total) : null;
  const l = isObject(body.looked_up) ? body.looked_up : null;
  const p = isObject(body.proposed) ? body.proposed : null;
  const u = isObject(body.changes_without_undo) ? body.changes_without_undo : null;
  const uTotal = u ? optionalCount(u.items_total) : null;
  const items: NoUndoItem[] = [];
  if (u && Array.isArray(u.items)) {
    for (const row of u.items.slice(0, 100)) {
      const item = mapNoUndoItem(row);
      if (item) items.push(item);
    }
  }
  const uCount = u ? optionalCount(u.count) : null;
  const wu = isObject(body.changes_with_undo) ? body.changes_with_undo : null;
  const ledger = isObject(body.ledger) ? body.ledger : {};
  const chain = mapChain(ledger.chain);
  return {
    days: days !== null && days >= 1 ? days : 7,
    noticed: n && noticedTotal !== null ? { total: noticedTotal, fresh: count(n.new), again: count(n.again), byKind } : null,
    lookedUp: l && optionalCount(l.calls) !== null ? { calls: count(l.calls), failed: count(l.failed), runs: count(l.runs) } : null,
    proposedTotal: p ? optionalCount(p.total) : null,
    applied: changeList(body.applied),
    undone: changeList(body.undone),
    rejected: optionalCount(body.rejected),
    notApplied: optionalCount(body.not_applied),
    noUndo: u && (uCount !== null || uTotal !== null || items.length > 0) ? { count: uCount ?? items.length, total: uTotal ?? items.length, notListed: count(u.not_listed), items } : null,
    withUndoCalls: wu ? optionalCount(wu.calls) : null,
    ledger: {
      coversWindow: typeof ledger.covers_window === 'boolean' ? ledger.covers_window : null,
      chain: chain.chain,
      problem: chain.problem,
      lines: optionalCount(ledger.lines_in_window),
      oldestMs: toMs(ledger.oldest_line_at),
    },
    text: sentenceOf(body.text, 6000, true),
  };
}

function plural(n: number, one: string, many: string): string {
  return n === 1 ? `1 ${one}` : `${n} ${many}`;
}

/** What was noticed, in one or two sentences. */
export function noticedText(r: Report): string {
  const n = r.noticed;
  if (n === null) return 'Aqua did not say what it noticed.';
  if (n.total === 0) return 'Noticed: nothing that it could not do.';
  const kinds = n.byKind.map((k) => `${gapKindPhrase(k.kind)} (${k.count})`).join(', ');
  const again = n.again === 0 ? '' : ` ${n.again} of them had been noticed before and happened again.`;
  return `Noticed ${plural(n.total, 'thing', 'things')} it could not do${kinds === '' ? '' : `: ${kinds}`}.${again}`;
}

/** What was looked up on the web or in research. */
export function lookedUpText(r: Report): string {
  const l = r.lookedUp;
  if (l === null) return 'Aqua did not say what it looked up.';
  if (l.calls === 0) return 'Looked up: nothing on the web.';
  return `Looked up: ${plural(l.calls, 'web or research call', 'web or research calls')} in ${plural(l.runs, 'run', 'runs')}; ${l.failed} failed.`;
}

/** What was proposed and what became of it. */
export function proposedText(r: Report): string {
  if (r.proposedTotal === null) return 'Aqua did not say what it proposed.';
  const applied = r.applied === null ? 'not said' : String(r.applied.length);
  const undone = r.undone === null ? 'not said' : String(r.undone.length);
  const rejected = r.rejected === null ? 'not said' : String(r.rejected);
  const notApplied = r.notApplied === null ? 'not said' : String(r.notApplied);
  return `Proposed ${plural(r.proposedTotal, 'fix', 'fixes')}. Applied with your approval: ${applied}. Undone: ${undone}. Rejected: ${rejected}. Not applied: ${notApplied}.`;
}

const HOST_ANSWER_WORDS: Record<string, string> = {
  once: 'you said yes this time',
  session: 'a yes you gave earlier in the session covered it',
  always: 'you said yes, and to remember it',
  refused: 'the answer was no',
  policy: 'a rule answered it',
};

/** What the tool host's own question about a folder came to, as the audit log records it (`once`, `session`, `always`, `refused`, `policy`). */
export function hostAnswerWords(answer: string): string {
  return HOST_ANSWER_WORDS[answer] ?? `its answer was ${cutText(shownText(answer), 40)}`;
}

/** One applied change in the report: what, when, who approved it, and what the tool host asked again. */
export function reportChangeText(c: ReportChange, nowMs: number, formatDate?: (ms: number) => string): string {
  const title = c.title === '' ? 'A change' : c.title;
  const when = c.appliedMs === null ? '' : `, ${agoText(c.appliedMs, nowMs, formatDate)}`;
  const asked = c.hostAsked === null ? '' : ` The tool host asked again: ${hostAnswerWords(c.hostAsked.answer)}.`;
  return `${title}. Applied${when}. Approved by ${approvedByWords(c.approvedBy)}.${asked}`;
}

/** One undone change in the report. */
export function reportUndoneText(c: ReportChange, nowMs: number, formatDate?: (ms: number) => string): string {
  const title = c.title === '' ? 'A change' : c.title;
  const when = c.undoneMs === null ? '' : `, ${agoText(c.undoneMs, nowMs, formatDate)}`;
  return `${title}. Undone${when}.`;
}

/** The heading over the changes that have no undo. */
export function noUndoHeading(r: Report): string {
  const u = r.noUndo;
  if (u === null) return 'Aqua did not say which changes have no undo.';
  if (u.count === 0) {
    const back = r.withUndoCalls === null ? '' : ` ${plural(r.withUndoCalls, 'change by agent runs can', 'changes by agent runs can')} be put back.`;
    return `Changes with no undo: none.${back}`;
  }
  return `Changes with no undo: ${u.count}. Aqua cannot take these back.`;
}

/** One change with no undo, with the reason. Always starts with "No undo", so it never depends on colour. */
export function noUndoItemText(item: NoUndoItem, nowMs: number, formatDate?: (ms: number) => string): string {
  const what = item.source === 'proposal' ? (item.title !== '' ? item.title : item.tool) : item.tool;
  const run = item.runId === '' ? '' : ` in the run ${item.runId}`;
  const calls = item.calls === null ? '' : ` (${plural(item.calls, 'call', 'calls')})`;
  const where = item.targets.length === 0 ? '' : ` on ${item.targets.join(', ')}`;
  const when = item.lastMs === null ? '' : `, ${agoText(item.lastMs, nowMs, formatDate)}`;
  const why = item.why === '' ? 'Aqua has no way to take it back.' : tidy(item.why);
  return `No undo: ${what === '' ? 'a change' : what}${run}${calls}${where}${when}. ${why}`;
}

/** A note when the list of changes with no undo is cut: how many are shown out of how many, and what was left out altogether. Empty when nothing is cut. */
export function noUndoCutText(r: Report): string {
  const u = r.noUndo;
  if (u === null || (u.total <= u.items.length && u.notListed === 0)) return '';
  const shown = `Showing ${u.items.length} of ${u.total}.`;
  return u.notListed === 0 ? shown : `${shown} ${plural(u.notListed, 'more was', 'more were')} left out because there were too many to keep track of.`;
}

/** The note about the record Aqua read the report from. */
export function ledgerText(r: Report, formatDate: (ms: number) => string = (ms) => new Date(ms).toLocaleDateString()): string[] {
  const out: string[] = [];
  const l = r.ledger;
  if (l.coversWindow === true) out.push(`The audit log covers the whole window${l.lines === null ? '' : ` (${plural(l.lines, 'line', 'lines')} in it)`}.`);
  else if (l.coversWindow === false) out.push(`The audit log only goes back to ${l.oldestMs === null ? 'a later date' : formatDate(l.oldestMs)}; older calls were rotated away, so this report may be missing some.`);
  else out.push('Aqua did not say how much of the window the audit log covers.');
  if (l.chain === 'holds') out.push("The audit log's chain was checked and holds.");
  else if (l.chain === 'broken') out.push(`Warning: the audit log's chain did not check out${l.problem === '' ? '.' : `: ${l.problem.replace(/[.!?]+$/, '')}.`}`);
  else if (l.chain === 'not_checked') out.push("The audit log's chain was not checked for this report.");
  else out.push("Aqua did not say whether the audit log's chain holds.");
  return out;
}

export const REPORT_DAYS = [1, 7, 30, 90] as const;

export function reportDaysWords(days: number): string {
  return days === 1 ? 'The last day' : `The last ${days} days`;
}

// ---- the calls ------------------------------------------------------------------------------------

const BASE = '/api/v1/self-repair';

/** A gap status the list can be asked for. */
const GAP_FILTERS = new Set(['open', 'new', 'proposed', 'fixed', 'dismissed']);

export function gapsUrl(status?: string): string {
  return status !== undefined && GAP_FILTERS.has(status) ? `${BASE}/gaps?status=${status}&limit=200` : `${BASE}/gaps?limit=200`;
}

export function proposalsUrl(limit = 100): string {
  return `${BASE}/proposals?limit=${Number.isSafeInteger(limit) && limit > 0 ? Math.min(limit, 500) : 100}`;
}

export function reportUrl(days: number): string {
  const d = Number.isSafeInteger(days) && days >= 1 ? Math.min(days, 90) : 7;
  return `${BASE}/report?days=${d}`;
}

function checkedId(id: unknown): number {
  if (!isWholeNumber(id) || id <= 0) throw new CoreError('That item cannot be answered.', 400);
  return id;
}

/** The core's own reason from an error body, cut and made visible, or ''. Longer than `serverSentence` allows: a reason here is a sentence or two. */
export function reasonOf(body: unknown): string {
  return isObject(body) && typeof body.error === 'string' ? sentenceOf(body.error, REASON_CHARS) : '';
}

/** A 409 is "the core will not do that now", and its reason is the answer: shown whole, after the words that say what did not happen. */
function refusal(answer: CoreAnswer, lead: string, fallback: string, tail = ''): CoreError | null {
  if (answer.ok || answer.status !== 409) return null;
  const reason = reasonOf(answer.body);
  return new CoreError(`${lead} ${reason === '' ? fallback : tidy(reason)}${tail === '' ? '' : ` ${tail}`}`, 409);
}

const GONE: Partial<Record<number, string>> = { 404: 'That item is not there any more. The list is read again.' };

const LIST_GAPS: Wording = { fallback: 'The things Aqua could not do could not be read.', status: { 404: 'This version of Aqua has no self-repair record.' } };
const LIST_PROPOSALS: Wording = { fallback: 'The proposals could not be read.', status: { 404: 'This version of Aqua has no self-repair record.' } };
const REPORT: Wording = { fallback: 'The report could not be read.', status: { 404: 'This version of Aqua has no self-repair report.' } };
const DISMISS: Wording = { fallback: 'The gap could not be dismissed.', status: GONE };
const PROPOSE: Wording = { fallback: 'Aqua could not work out a fix.', status: GONE };
const APPROVE: Wording = { fallback: 'The change could not be applied.', status: GONE };
const REJECT: Wording = { fallback: 'The proposal could not be rejected.', status: GONE };
const UNDO: Wording = { fallback: 'The change could not be taken back.', status: GONE };

export const selfRepairApi = {
  /** The gaps, newest first. `status` is `open` (new and proposed) or a gap status; none is all of them. */
  gaps: async (status?: string): Promise<Gap[]> => {
    const gaps = mapGapList(answerBody(await callCore(gapsUrl(status), undefined, LIST_GAPS)));
    if (!gaps) throw new CoreError(UNREADABLE_ANSWER);
    return gaps;
  },

  dismiss: async (id: number): Promise<void> => {
    const safe = checkedId(id);
    const answer = await callCore(`${BASE}/gaps/${safe}/dismiss`, jsonInit('POST'), DISMISS);
    const refused = refusal(answer, 'Not dismissed.', 'Aqua would not do that now.');
    if (refused) throw refused;
    const body = answerBody(answer);
    if (!isObject(body) || !mapGap(body.gap)) throw new CoreError(UNREADABLE_ANSWER);
  },

  /** Asks for the fixes a gap suggests. Nothing is applied: they wait under Proposals. */
  propose: async (id: number): Promise<Proposed> => {
    const safe = checkedId(id);
    const answer = await callCore(`${BASE}/gaps/${safe}/propose`, jsonInit('POST'), PROPOSE);
    const refused = refusal(answer, 'No fix was proposed.', 'Aqua would not do that now.');
    if (refused) throw refused;
    const proposed = mapProposed(answerBody(answer));
    if (!proposed) throw new CoreError(UNREADABLE_ANSWER);
    return proposed;
  },

  /** Every proposal, newest first. */
  proposals: async (): Promise<Proposal[]> => {
    const list = mapProposalList(answerBody(await callCore(proposalsUrl(), undefined, LIST_PROPOSALS)));
    if (!list) throw new CoreError(UNREADABLE_ANSWER);
    return list;
  },

  /**
   * Approves one proposal and applies it. This can last: for a folder the core waits until the person has answered the tool host's own question in the approval
   * prompt. An answer that is not "applied" is an error with the core's reason, and says that nothing was changed.
   */
  approve: async (id: number): Promise<Proposal> => {
    const safe = checkedId(id);
    const answer = await callCore(`${BASE}/proposals/${safe}/approve`, jsonInit('POST'), APPROVE);
    // A 409 with the proposal in it is a change that was tried and not made (the flow moved on, the person said no to the folder): nothing was changed.
    const tried = !answer.ok && answer.status === 409 && isObject(answer.body) ? mapProposal(answer.body.proposal) : null;
    const refused = tried?.status === 'failed' ? refusal(answer, 'Not applied.', 'Aqua gave no reason.', 'Nothing was changed.') : refusal(answer, 'Not approved.', 'Aqua would not do that now.');
    if (refused) throw refused;
    const body = answerBody(answer);
    const p = isObject(body) ? mapProposal(body.proposal) : null;
    if (!p) throw new CoreError(UNREADABLE_ANSWER);
    if (p.status !== 'applied') throw new CoreError('Aqua did not say that the change was applied, so treat it as not done.');
    return p;
  },

  reject: async (id: number): Promise<Proposal> => {
    const safe = checkedId(id);
    const answer = await callCore(`${BASE}/proposals/${safe}/reject`, jsonInit('POST'), REJECT);
    const refused = refusal(answer, 'Not rejected.', 'Aqua would not do that now.');
    if (refused) throw refused;
    const body = answerBody(answer);
    const p = isObject(body) ? mapProposal(body.proposal) : null;
    if (!p) throw new CoreError(UNREADABLE_ANSWER);
    return p;
  },

  /** Takes an applied change back. The core refuses when it cannot (a folder grant, a flow edited since), and its reason is the error. */
  undo: async (id: number): Promise<Proposal> => {
    const safe = checkedId(id);
    const answer = await callCore(`${BASE}/proposals/${safe}/undo`, jsonInit('POST'), UNDO);
    const refused = refusal(answer, 'Not undone.', 'Aqua would not do that now.', 'Nothing was changed.');
    if (refused) throw refused;
    const body = answerBody(answer);
    const p = isObject(body) ? mapProposal(body.proposal) : null;
    if (!p) throw new CoreError(UNREADABLE_ANSWER);
    return p;
  },

  /** What Aqua did by itself in the last `days` days. Reading it checks the audit log, so the screen asks for it when the person opens it. */
  report: async (days: number): Promise<Report> => {
    const report = mapReport(answerBody(await callCore(reportUrl(days), undefined, REPORT)));
    if (!report) throw new CoreError(UNREADABLE_ANSWER);
    return report;
  },
};
