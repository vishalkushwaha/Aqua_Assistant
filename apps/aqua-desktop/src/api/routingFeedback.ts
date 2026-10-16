// How often the route Aqua chose for a request was wrong, by what you said on the feedback buttons (GET /api/v1/laya/feedback/stats?days=, either
// token), and the check whether a threshold of the router could change (POST /api/v1/laya/calibrate, UI token). documentation/ROUTING_FEEDBACK.md.
//
// The rate is of the requests you ANSWERED, not the router's true error rate, and the core says so in its `note`. The check only proposes: it
// never changes a threshold, and nothing here suggests that it does. Everything here is a plain function, so a test can call it without a page.
// Text from the core (a note, a reason) is only ever handed to React as a text child.

import { AquaError, bodyOrThrow, callCore, clip, count, isObject, jsonInit, text, UNREADABLE_ANSWER, type Wording } from './plain.ts';

/** How far back the rate looks: the choices on the screen, and what is asked when nothing is chosen. */
export const DAY_CHOICES: readonly number[] = [7, 30, 90];
export const DEFAULT_DAYS = 30;

/** Answers judged, how many were wrong, and the rate with its interval (shares, 0 to 1; null when there are no answers). */
export interface Tally {
  judged: number;
  wrong: number;
  rate: number | null;
  low: number | null;
  high: number | null;
}

export interface Breakdown {
  name: string;
  tally: Tally;
}

export interface Mistake {
  chosen: string;
  correct: string;
  count: number;
}

export interface RoutingStats {
  days: number;
  /** The confidence of the interval, 0 to 1 (0.95). */
  confidence: number;
  /** Every decision made in the window. */
  decisions: number;
  answered: number;
  /** Decisions nobody answered: the rate says nothing about them. */
  noFeedback: number;
  /** Answers whose decision could not be found, so that no route is called right or wrong for them. */
  unjudged: number;
  overall: Tally;
  byRoute: Breakdown[];
  bySource: Breakdown[];
  /** The wrong answers, the most common first. */
  mistakes: Mistake[];
  /** The core's own note on what the rate is not. */
  note: string;
}

function share(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? Math.min(1, Math.max(0, value)) : null;
}

export function mapTally(raw: unknown): Tally {
  const o = isObject(raw) ? raw : {};
  return { judged: count(o.judged), wrong: count(o.wrong), rate: share(o.rate), low: share(o.low), high: share(o.high) };
}

function mapBreakdown(raw: unknown): Breakdown[] {
  if (!isObject(raw)) return [];
  return Object.entries(raw)
    .filter(([, tally]) => isObject(tally))
    .map(([name, tally]) => ({ name, tally: mapTally(tally) }))
    .sort((a, b) => b.tally.judged - a.tally.judged || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
}

function mapMistakes(raw: unknown): Mistake[] {
  if (!isObject(raw)) return [];
  return Object.entries(raw)
    .map(([pair, n]): Mistake => {
      const at = pair.indexOf('>');
      return at < 0 ? { chosen: pair, correct: '', count: count(n) } : { chosen: pair.slice(0, at), correct: pair.slice(at + 1), count: count(n) };
    })
    .filter((m) => m.count > 0)
    .sort((a, b) => b.count - a.count || (a.chosen + a.correct < b.chosen + b.correct ? -1 : 1));
}

/** The answer of the stats route, or null when it is not one (no overall tally). A count the core leaves out is zero. */
export function mapStats(body: unknown): RoutingStats | null {
  if (!isObject(body) || !isObject(body.overall)) return null;
  const confidence = share(body.confidence);
  return {
    days: count(body.days),
    confidence: confidence === null || confidence === 0 ? 0.95 : confidence,
    decisions: count(body.decisions),
    answered: count(body.answered),
    noFeedback: count(body.no_feedback),
    unjudged: count(body.unjudged),
    overall: mapTally(body.overall),
    byRoute: mapBreakdown(body.by_route),
    bySource: mapBreakdown(body.by_source),
    mistakes: mapMistakes(body.confusion),
    note: text(body.note).trim(),
  };
}

/** What the core's own `note` should say, if it did not send one. */
export const FALLBACK_NOTE =
  'This is the share of the requests you answered whose route was wrong. It is not the true error rate: a button is pressed far more often when something went wrong, and the requests nobody answered say nothing either way.';

export function noteText(stats: RoutingStats): string {
  return stats.note !== '' ? stats.note : FALLBACK_NOTE;
}

// ---- wording ---------------------------------------------------------------------------------------------------------------------------

/** A share as a percentage, to a tenth when it is not whole: 0.083 is "8.3%", 0.5 is "50%". */
export function percentText(value: number | null): string {
  return value === null ? '-' : `${Math.round(value * 1000) / 10}%`;
}

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

/** The route as a person would say it. A route this version does not know is shown as the core wrote it (cut). */
export function routeName(route: string): string {
  switch (route) {
    case 'chat': return 'Chat';
    case 'mcp_agent': return 'Tools (the agent)';
    case 'refuse': return 'Refuse';
    case '': return 'Not found';
    default: return clip(route, 40);
  }
}

/** Who decided the route. */
export function sourceName(source: string): string {
  switch (source) {
    case 'rules': return 'Rules';
    case 'model_gate': return 'Model gate';
    case 'decision_router': return 'Decision router';
    case 'unknown': return 'Not found';
    case '': return 'Not found';
    default: return clip(source, 40);
  }
}

/** "8.3% wrong (2 of 24), 95% interval 2.3% to 25%", or that there are no answers. */
export function rateText(tally: Tally, confidence: number): string {
  if (tally.judged === 0 || tally.rate === null) return 'no answers yet';
  const interval = tally.low !== null && tally.high !== null ? `, ${Math.round(confidence * 100)}% interval ${percentText(tally.low)} to ${percentText(tally.high)}` : '';
  return `${percentText(tally.rate)} wrong (${tally.wrong} of ${tally.judged})${interval}`;
}

/** The line about the window: how many decisions, how many answered. */
export function windowText(stats: RoutingStats): string {
  const days = stats.days > 0 ? stats.days : DEFAULT_DAYS;
  return `In the last ${plural(days, 'day', 'days')}, Aqua chose a route for ${plural(stats.decisions, 'request', 'requests')} and you answered ${stats.answered}.`;
}

/** The decisions nobody answered, in words. */
export function unansweredText(stats: RoutingStats): string {
  if (stats.decisions === 0) return 'Aqua made no routing decision in this time.';
  if (stats.noFeedback === 0) return 'Every decision in this time has an answer.';
  return `${plural(stats.noFeedback, 'decision was', 'decisions were')} never answered. ${stats.noFeedback === 1 ? 'It is' : 'They are'} not in the rate.`;
}

/** The answers whose decision could not be found; '' when there are none. */
export function unjudgedText(stats: RoutingStats): string {
  if (stats.unjudged === 0) return '';
  return `${plural(stats.unjudged, 'answer', 'answers')} could not be matched to a decision (Aqua was restarted since, or the request was changed), so no route is called right or wrong for ${stats.unjudged === 1 ? 'it' : 'them'}.`;
}

/** The most common mistake, in words; '' when there is none. */
export function mistakeText(m: Mistake): string {
  return `${routeName(m.chosen)} chosen where ${m.correct === '' ? 'another route' : routeName(m.correct)} was right: ${plural(m.count, 'time', 'times')}`;
}

// ---- the check -------------------------------------------------------------------------------------------------------------------------

export interface Proposal {
  /** The threshold: `min_probability` or `min_margin`. */
  name: string;
  from: number;
  to: number;
  heldOutBefore: number | null;
  heldOutAfter: number | null;
  heldOutCount: number;
  feedbackBefore: number | null;
  feedbackAfter: number | null;
  feedbackCount: number;
}

export interface Calibration {
  /** A change the core proposes, or null. It was not made. */
  proposal: Proposal | null;
  /** Why there is no proposal, in the core's words; '' when there is one or the core gave none. */
  reasonText: string;
  answersUsed: number;
  heldOut: number;
  heldOutWithModel: number;
  days: number;
}

function mapProposal(raw: unknown): Proposal | null {
  if (!isObject(raw) || typeof raw.name !== 'string' || raw.name === '') return null;
  if (typeof raw.from !== 'number' || !Number.isFinite(raw.from) || typeof raw.to !== 'number' || !Number.isFinite(raw.to)) return null;
  return {
    name: raw.name,
    from: raw.from,
    to: raw.to,
    heldOutBefore: share(raw.held_out_before),
    heldOutAfter: share(raw.held_out_after),
    heldOutCount: count(raw.n_held_out),
    feedbackBefore: share(raw.feedback_before),
    feedbackAfter: share(raw.feedback_after),
    feedbackCount: count(raw.n_feedback),
  };
}

/**
 * The answer of the check, or null when it is not one. It is also null when the core says it APPLIED something (`applied: true`) or sends a
 * proposal this app cannot read: the check only proposes, and this screen does not describe what it does not understand.
 */
export function mapCalibration(body: unknown): Calibration | null {
  if (!isObject(body) || body.applied === true) return null;
  const hasProposal = body.proposal !== null && body.proposal !== undefined;
  const proposal = hasProposal ? mapProposal(body.proposal) : null;
  if (hasProposal && proposal === null) return null;
  const reason = isObject(body.reason) ? text(body.reason.text).trim() : '';
  return {
    proposal,
    reasonText: reason,
    answersUsed: count(body.answers_used),
    heldOut: count(body.held_out_requests),
    heldOutWithModel: count(body.held_out_with_model_answer),
    days: count(body.days),
  };
}

/** What a threshold is, in words, with its name for whoever has to change it. */
export function thresholdName(name: string): string {
  switch (name) {
    case 'min_probability': return 'how sure the model has to be (min_probability)';
    case 'min_margin': return 'how far its first choice has to be ahead of its second (min_margin)';
    default: return clip(name, 60);
  }
}

export interface CalibrationWords {
  headline: string;
  lines: string[];
}

/** The check's answer in words. It never says or hints that a change was made: it says the opposite. */
export function calibrationWords(c: Calibration): CalibrationWords {
  const back = c.days > 0 ? ` from the last ${plural(c.days, 'day', 'days')}` : '';
  const looked = `It looked at ${plural(c.answersUsed, 'answer', 'answers')} of yours${back} and ${plural(c.heldOut, 'held-out request', 'held-out requests')} (${c.heldOutWithModel} of them with the model's recorded answer).`;
  if (c.proposal === null) {
    return {
      headline: 'No change is proposed.',
      lines: [...(c.reasonText !== '' ? [c.reasonText] : []), looked, 'Nothing was changed.'],
    };
  }
  const p = c.proposal;
  return {
    headline: `A change is proposed, not made: ${thresholdName(p.name)} from ${p.from} to ${p.to}.`,
    lines: [
      `On the held-out requests it would route ${percentText(p.heldOutBefore)} right as it is now and ${percentText(p.heldOutAfter)} right with the change (${plural(p.heldOutCount, 'request', 'requests')}).`,
      `On your answers it would route ${percentText(p.feedbackBefore)} right as it is now and ${percentText(p.feedbackAfter)} right with the change (${plural(p.feedbackCount, 'answer', 'answers')}).`,
      looked,
      'Nothing was changed, and this screen cannot change it. A person would change the threshold in the code and test it first.',
    ],
  };
}

// ---- the calls -------------------------------------------------------------------------------------------------------------------------

/** A number of days the route accepts (1 to 365), or the default. */
export function daysOrDefault(days: number): number {
  return Number.isSafeInteger(days) && days >= 1 && days <= 365 ? days : DEFAULT_DAYS;
}

const STATS_WORDING: Wording = { fallback: 'The wrong-route rate could not be worked out.', status: { 404: 'This version of Aqua cannot say how often a route was wrong.' } };
const CALIBRATE_WORDING: Wording = { fallback: 'The thresholds could not be tried.', status: { 404: 'This version of Aqua cannot check the routing thresholds.' } };

export const routingFeedbackApi = {
  /** The wrong-route rate over the last `days` days. */
  stats: async (days: number = DEFAULT_DAYS): Promise<RoutingStats> => {
    const stats = mapStats(bodyOrThrow(await callCore(`/api/v1/laya/feedback/stats?days=${daysOrDefault(days)}`, undefined, STATS_WORDING)));
    if (!stats) throw new AquaError(UNREADABLE_ANSWER);
    return stats;
  },
  /** Asks whether a threshold could change (UI token). It proposes and changes nothing. */
  calibrate: async (): Promise<Calibration> => {
    const result = mapCalibration(bodyOrThrow(await callCore('/api/v1/laya/calibrate', jsonInit('POST'), CALIBRATE_WORDING)));
    if (!result) throw new AquaError(UNREADABLE_ANSWER);
    return result;
  },
};
