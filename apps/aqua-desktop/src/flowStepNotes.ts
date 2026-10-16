// What the flow editor says about a kind of step, read from that kind's own entry in GET /api/v1/flows/node-types (documentation/AI_FLOWS.md,
// "For each and Database steps"). Nothing here decides which kinds can run: the core's `runnable` flag does. Every kind runs now, so the
// editor says nothing about it today; a kind that the core marks `runnable: false` gets the sentence below. The settings of the For each
// step (the list) and of the Database step (database, query, values, most rows) are drawn from the node type's fields like any other step.
// Plain functions, so a test can call them without a page.

/** The part of a node type these functions read. `runnable` is missing from an answer of an older core. */
export interface StepKindInfo {
  kind: string;
  label: string;
  description: string;
  runnable?: boolean;
}

export const CANNOT_RUN_NOTE = 'This kind of step cannot run yet. You can draw it and save it, but a run that contains one is refused before it starts.';

/** The sentence for a kind the core says cannot run; '' when it can, and when the core does not say (it is not guessed). */
export function cannotRunNote(spec: Pick<StepKindInfo, 'runnable'>): string {
  return spec.runnable === false ? CANNOT_RUN_NOTE : '';
}

/** The name of a kind in the list of kinds to add: with "(cannot run yet)" after it when the core says it cannot. */
export function kindOptionLabel(spec: Pick<StepKindInfo, 'label' | 'runnable'>): string {
  return spec.runnable === false ? `${spec.label} (cannot run yet)` : spec.label;
}

/** "Runs once for every item; at most 200 items." for the For each step, from the number in its own description; '' for any other kind, or when the description gives none. */
export function loopLimitNote(spec: Pick<StepKindInfo, 'kind' | 'description'>): string {
  if (spec.kind !== 'loop') return '';
  const found = /\bat most (\d[\d,]*) items\b/i.exec(spec.description);
  return found ? `Runs once for every item; at most ${found[1]} items.` : '';
}
