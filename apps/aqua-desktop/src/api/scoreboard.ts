// The AquaBench scoreboard (GET /api/v1/eval/scoreboard, API token readable): the contents of the file `python scripts/eval/scoreboard.py` writes
// (scripts/eval/README.md, "The scoreboard"), or `{"present": false}` when no scoreboard has been made. The route is the core's; this module
// only reads it. Nothing is worked out here: a drop is called red only when the report lists it under `regressions`, which is the report's own
// verdict that it is larger than the noise it recorded. A change against the baseline is shown as a plain signed number, never coloured.
//
// Everything here is a plain function, so a test can call it without a page. Text from the report (task ids, messages, reasons) is only ever
// handed to React as a text child.

import { AquaError, agoText, bodyOrThrow, callCore, clip, count, isObject, stringList, text, UNREADABLE_ANSWER, type Wording } from './plain.ts';

export const SCOREBOARD_COMMAND = 'python scripts/eval/scoreboard.py';
const NO_SCOREBOARD_BEFORE = 'No scoreboard yet: run ';
const NO_SCOREBOARD_AFTER = ' after an AquaBench run.';

/** The sentence for "there is no scoreboard", in three parts so that the screen can show the command as code. */
export const NO_SCOREBOARD_PARTS = { before: NO_SCOREBOARD_BEFORE, command: SCOREBOARD_COMMAND, after: NO_SCOREBOARD_AFTER } as const;
export const NO_SCOREBOARD_TEXT = `${NO_SCOREBOARD_BEFORE}${SCOREBOARD_COMMAND}${NO_SCOREBOARD_AFTER}`;

/** The numbers of a group of tasks: all of them, or one category. */
export interface ScoreGroup {
  tasksRun: number;
  tasksSkipped: number;
  runs: number;
  passes: number;
  /** The share of runs that satisfied every check, 0 to 1, or null when nothing ran. */
  passRate: number | null;
  tasksPassed: number;
  failed: string[];
  flaky: string[];
  /** Seconds, or null. */
  p50: number | null;
  p90: number | null;
}

export interface CategoryScore extends ScoreGroup {
  name: string;
  /** The change against the baseline over the tasks both ran, or null when there is no baseline. */
  vsBaseline: { comparedTasks: number; passRateChange: number | null; p50Change: number | null } | null;
}

/** One line of `regressions`, `within_noise` or `improvements`: the report's own sentence about one task. */
export interface Finding {
  task: string;
  category: string;
  kind: string;
  message: string;
}

export interface SkippedGroup {
  reason: string;
  tasks: string[];
}

export interface NotCompared {
  task: string;
  category: string;
  reason: string;
}

export interface ScoreboardView {
  /** When the file was written (the text the report holds), or ''. */
  generated: string;
  reportCount: number;
  hasBaseline: boolean;
  overall: ScoreGroup & { hardGateFailed: string[] };
  categories: CategoryScore[];
  /** What the report lists as worse, beyond the noise it recorded. The only red there is. */
  regressions: Finding[];
  /** A drop the report records as noise: not red. */
  withinNoise: Finding[];
  improvements: Finding[];
  notCompared: NotCompared[];
  skipped: SkippedGroup[];
}

export type ScoreboardResult = { present: false } | { present: true; view: ScoreboardView };

function finiteOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

/** A share of runs: 0 to 1, or null. */
function rateOrNull(value: unknown): number | null {
  const n = finiteOrNull(value);
  return n === null ? null : Math.min(1, Math.max(0, n));
}

export function mapGroup(raw: unknown): ScoreGroup {
  const o = isObject(raw) ? raw : {};
  return {
    tasksRun: count(o.tasks_run),
    tasksSkipped: count(o.tasks_skipped),
    runs: count(o.runs),
    passes: count(o.passes),
    passRate: rateOrNull(o.pass_at_1),
    tasksPassed: count(o.tasks_passed),
    failed: stringList(o.failed),
    flaky: stringList(o.flaky),
    p50: finiteOrNull(o.ttc_p50),
    p90: finiteOrNull(o.ttc_p90),
  };
}

function mapVsBaseline(raw: unknown): CategoryScore['vsBaseline'] {
  if (!isObject(raw)) return null;
  return { comparedTasks: count(raw.compared_tasks), passRateChange: finiteOrNull(raw.pass_at_1), p50Change: finiteOrNull(raw.ttc_p50) };
}

/** The findings of one list: a row with no task and no sentence is left out; one with a task and no sentence gets a plain one. */
export function mapFindings(raw: unknown): Finding[] {
  const out: Finding[] = [];
  for (const row of Array.isArray(raw) ? raw : []) {
    if (!isObject(row)) continue;
    const task = text(row.task);
    const kind = text(row.kind);
    const message = text(row.message).trim();
    if (task === '' && message === '') continue;
    out.push({ task, category: text(row.category), kind, message: message !== '' ? message : `${task}: ${kind === '' ? 'listed by the report' : kind}` });
  }
  return out;
}

/** The skipped tasks grouped by the reason, the reasons in order, the tasks of each in order. */
export function groupSkipped(raw: unknown): SkippedGroup[] {
  const byReason = new Map<string, string[]>();
  for (const row of Array.isArray(raw) ? raw : []) {
    if (!isObject(row)) continue;
    const task = text(row.task);
    if (task === '') continue;
    const reason = text(row.reason).trim() || 'no reason given';
    byReason.set(reason, [...(byReason.get(reason) ?? []), task]);
  }
  return [...byReason.entries()]
    .map(([reason, tasks]) => ({ reason, tasks: [...tasks].sort() }))
    .sort((a, b) => (a.reason < b.reason ? -1 : a.reason > b.reason ? 1 : 0));
}

function mapNotCompared(raw: unknown): NotCompared[] {
  const out: NotCompared[] = [];
  for (const row of Array.isArray(raw) ? raw : []) {
    if (!isObject(row) || text(row.task) === '') continue;
    out.push({ task: text(row.task), category: text(row.category), reason: text(row.reason).trim() || 'no reason given' });
  }
  return out;
}

/** A scoreboard in the shape of scoreboard.json, or null when the body is not one (no overall numbers and no categories). */
export function mapScoreboard(body: unknown): ScoreboardView | null {
  if (!isObject(body) || !isObject(body.overall) || !isObject(body.categories)) return null;
  const categories: CategoryScore[] = [];
  for (const [name, raw] of Object.entries(body.categories)) {
    if (!isObject(raw)) continue;
    categories.push({ name, ...mapGroup(raw), vsBaseline: mapVsBaseline(raw.vs_baseline) });
  }
  categories.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  return {
    generated: text(body.generated),
    reportCount: Array.isArray(body.reports) ? body.reports.length : 0,
    hasBaseline: Array.isArray(body.baseline) && body.baseline.length > 0,
    overall: { ...mapGroup(body.overall), hardGateFailed: stringList(body.overall.hard_gate_failed) },
    categories,
    regressions: mapFindings(body.regressions),
    withinNoise: mapFindings(body.within_noise),
    improvements: mapFindings(body.improvements),
    notCompared: mapNotCompared(body.not_compared),
    skipped: groupSkipped(body.skipped),
  };
}

/** The route's answer: no scoreboard (`present: false`), a scoreboard, or null when it is neither. */
export function mapScoreboardAnswer(body: unknown): ScoreboardResult | null {
  if (!isObject(body)) return null;
  if (body.present === false) return { present: false };
  const view = mapScoreboard(body);
  return view ? { present: true, view } : null;
}

// ---- wording ---------------------------------------------------------------------------------------------------------------------------

/** 0.9166 as "92%". */
export function passRateText(rate: number | null): string {
  return rate === null ? 'no runs' : `${Math.round(rate * 100)}%`;
}

/** Seconds for a person: "1.40 s", or "-" when the report has none. */
export function secondsText(seconds: number | null): string {
  return seconds === null ? '-' : `${seconds.toFixed(2)} s`;
}

/** A change of the pass rate (a share, so 0.05 is five points) as a signed number of points. It is a number, not a verdict. */
export function pointsChangeText(change: number | null): string {
  if (change === null) return '-';
  const points = change * 100;
  const size = Math.abs(points);
  if (size < 0.05) return 'same';
  const shown = size >= 1 ? String(Math.round(size)) : size.toFixed(1);
  return `${points < 0 ? '-' : '+'}${shown} ${shown === '1' ? 'point' : 'points'}`;
}

/** A change of a time in seconds as a signed number. */
export function secondsChangeText(change: number | null): string {
  if (change === null) return '-';
  if (Math.abs(change) < 0.005) return 'same';
  return `${change < 0 ? '-' : '+'}${Math.abs(change).toFixed(2)} s`;
}

/** Task ids in a row: the first few, then how many more. */
export function idsText(ids: string[], max = 8): string {
  if (ids.length <= max) return ids.join(', ');
  return `${ids.slice(0, max).join(', ')} and ${ids.length - max} more`;
}

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

/** The sentence above the table. */
export function summaryText(view: ScoreboardView): string {
  const o = view.overall;
  if (o.runs === 0) return `No task ran in ${plural(view.reportCount, 'report', 'reports')}${o.tasksSkipped > 0 ? ` (${o.tasksSkipped} skipped)` : ''}.`;
  const skipped = o.tasksSkipped > 0 ? ` (${o.tasksSkipped} skipped)` : '';
  const times = o.p50 === null ? '' : ` Typical time ${secondsText(o.p50)}${o.p90 === null ? '' : `, slow end ${secondsText(o.p90)}`}.`;
  return `${plural(o.runs, 'run', 'runs')} of ${plural(o.tasksRun, 'task', 'tasks')}${skipped} from ${plural(view.reportCount, 'report', 'reports')}. Satisfied every check in ${passRateText(o.passRate)} of runs.${times}`;
}

/** What to say about the red list. Red is only what the report lists as a regression. */
export function redText(view: ScoreboardView): string {
  if (view.regressions.length > 0) return view.regressions.length === 1 ? '1 thing is red:' : `${view.regressions.length} things are red:`;
  return view.hasBaseline
    ? 'Nothing is red against the baseline.'
    : 'Nothing is red. No baseline was given, so only a safety task that failed a run would show here.';
}

/** How long ago the file was written, or '' when the report has no readable time. `nowSecs` is given so the answer does not depend on the clock. */
export function writtenText(generated: string, nowSecs: number, formatDate?: (unix: number) => string): string {
  const at = Date.parse(generated);
  if (!Number.isFinite(at)) return '';
  return `Written ${agoText(at / 1000, nowSecs, formatDate)}.`;
}

/** The failed and flaky tasks of a row of the table. */
export function noteText(group: ScoreGroup): string {
  const parts: string[] = [];
  if (group.failed.length > 0) parts.push(`failed every run: ${idsText(group.failed, 5)}`);
  if (group.flaky.length > 0) parts.push(`mixed results: ${idsText(group.flaky, 5)}`);
  return parts.join('; ');
}

/** How many of the red items belong to a category (the report's own list). */
export function redCount(view: ScoreboardView, category: string): number {
  return view.regressions.filter((r) => r.category === category).length;
}

/** A skip reason cut when very long. */
export function reasonText(reason: string): string {
  return clip(reason, 200);
}

// ---- the call --------------------------------------------------------------------------------------------------------------------------

const WORDING: Wording = { fallback: 'The scoreboard could not be read.' };

export const scoreboardApi = {
  /** The scoreboard, or `{ present: false }` when there is none (the route says so, or this version of Aqua has no such route). */
  get: async (): Promise<ScoreboardResult> => {
    const answer = await callCore('/api/v1/eval/scoreboard', undefined, WORDING);
    if (!answer.ok && answer.status === 404) return { present: false };
    const result = mapScoreboardAnswer(bodyOrThrow(answer));
    if (!result) throw new AquaError(UNREADABLE_ANSWER);
    return result;
  },
};
