// The skills inbox: skills Aqua proposes after a task that went well, waiting to be read and approved (section 5.6 of
// documentation/UI_CONTRACT.md; core/src/skills_inbox.rs). Every route here needs the UI token, which the shell or the dev proxy adds.
//
//   GET  /api/v1/skills/proposals                the proposals, newest first (every state: proposed, approved, rejected)
//   POST /api/v1/skills/proposals/{id}/approve   the person's changes, all optional: {name, description, body, allowed_tools, replace};
//                                                409 {collision: true} when the name is one of the person's own skills
//   POST /api/v1/skills/proposals/{id}/reject    rejects a proposal
//
// Nothing becomes a skill until the person approves it, and a skill can only narrow what a run may do, never widen it. A proposal's
// text (name, description, body) is written from a run's record; the app shows it as plain text and never as markup.
// Drafting a proposal from a run, versions and rolling back are routes of the contract this screen does not use.

import { agoText } from './recipes.ts';
import { answerBody, callCore, CoreError, isObject, jsonInit, UNREADABLE_ANSWER, type Wording } from './coreRequest.ts';

/** What the core accepts as the id of a proposal (core/src/skills_inbox.rs `valid_id`): it goes into a path, so nothing else is used. */
const ID_SHAPE = /^[A-Za-z0-9-]{1,40}$/;
/** What a skill's name may be (the core's loader: a-z, 0-9, - and _). */
const NAME_SHAPE = /^[a-z0-9_-]{1,48}$/;

export interface SkillProposal {
  id: string;
  /** Milliseconds since 1970, or null when the core does not say. */
  createdMs: number | null;
  /** The run it was drafted from. */
  runId: string;
  name: string;
  description: string;
  /** The tools it is narrowed to; an empty list means no narrowing; null means the core did not say. */
  allowedTools: string[] | null;
  body: string;
  /** `proposed`, `approved` or `rejected`. */
  state: string;
  /** The run had read text from outside, and was proposed only because the person asked for it. */
  fromTaintedRun: boolean;
}

/** One proposal, or null when its id could not safely be sent back. A missing field is an empty text; a missing tool list is "not said". */
export function mapProposal(raw: unknown): SkillProposal | null {
  if (!isObject(raw) || typeof raw.id !== 'string' || !ID_SHAPE.test(raw.id)) return null;
  const created = raw.created_at_ms;
  return {
    id: raw.id,
    createdMs: typeof created === 'number' && Number.isFinite(created) && created > 0 ? created : null,
    runId: typeof raw.run_id === 'string' ? raw.run_id : '',
    name: typeof raw.name === 'string' ? raw.name : '',
    description: typeof raw.description === 'string' ? raw.description : '',
    allowedTools: Array.isArray(raw.allowed_tools) ? raw.allowed_tools.filter((t): t is string => typeof t === 'string') : null,
    body: typeof raw.body === 'string' ? raw.body : '',
    state: typeof raw.state === 'string' ? raw.state : 'proposed',
    fromTaintedRun: raw.from_tainted_run === true,
  };
}

export interface Inbox {
  /** Waiting for the person, newest first. */
  waiting: SkillProposal[];
  /** Decided before (approved or rejected), newest first. */
  decided: SkillProposal[];
}

/** The inbox as the screen shows it, or null when the answer is not a list. */
export function mapInbox(body: unknown): Inbox | null {
  if (!isObject(body) || !Array.isArray(body.proposals)) return null;
  const all: SkillProposal[] = [];
  for (const row of body.proposals) {
    const p = mapProposal(row);
    if (p) all.push(p);
  }
  all.sort((a, b) => (b.createdMs ?? 0) - (a.createdMs ?? 0));
  return { waiting: all.filter((p) => p.state === 'proposed'), decided: all.filter((p) => p.state !== 'proposed') };
}

// ---- wording ---------------------------------------------------------------------------------------------------------------------------

/** The name for the list, or a stand-in when the proposal has none. */
export function skillName(p: SkillProposal): string {
  const n = p.name.trim();
  return n === '' ? '(no name)' : n;
}

/** What the skill does, in the proposal's own words, cut when very long. */
export function skillDescription(p: SkillProposal): string {
  const d = p.description.split(/\s+/).filter(Boolean).join(' ');
  if (d === '') return '(no description)';
  const letters = Array.from(d);
  return letters.length > 400 ? `${letters.slice(0, 399).join('')}…` : d;
}

/** What the skill may use, in words. */
export function toolsText(p: SkillProposal): string {
  if (p.allowedTools === null) return 'The proposal does not say which tools it may use.';
  if (p.allowedTools.length === 0) return 'No list of tools: the skill is not narrowed to particular tools, so it may use whatever a run may use. A skill can only narrow what a run may do, never widen it.';
  return `It may use only these tools: ${p.allowedTools.join(', ')}.`;
}

/** Where it came from, in words. `nowMs` is given so the answer does not depend on the clock. */
export function originText(p: SkillProposal, nowMs: number, formatDate?: (unix: number) => string): string {
  const run = p.runId.trim() === '' ? 'a run' : `the run ${p.runId.trim()}`;
  if (p.createdMs === null) return `Drafted from ${run}.`;
  return `Drafted from ${run}, ${agoText(p.createdMs / 1000, nowMs / 1000, formatDate)}.`;
}

/** The things to read twice before accepting: what this skill asks for beyond the ordinary, in words. Empty when there is nothing to say. */
export function cautions(p: SkillProposal): string[] {
  const out: string[] = [];
  if (p.allowedTools !== null && p.allowedTools.length === 0) out.push('It lists no tools, so nothing narrows what it may use.');
  if (p.fromTaintedRun) out.push('It was drafted from a run that read text from outside (a page, a file, a message). What that run did may have been steered by that text, so read the steps before accepting.');
  return out;
}

/** The text of the proposal as it would be saved (the SKILL.md file), for the person to read. Plain text: the screen never renders it as markup. */
export function skillText(p: SkillProposal): string {
  const oneLine = p.description.split(/\s+/).filter(Boolean).join(' ');
  const head = ['---', `name: ${p.name}`, `description: ${oneLine}`];
  if (p.allowedTools !== null) head.push(`allowed-tools: ${p.allowedTools.join(', ')}`);
  head.push('---');
  return `${head.join('\n')}\n${p.body}\n`;
}

/** The sentence after a skill was accepted. */
export function acceptedSentence(skill: string, version: number): string {
  const what = skill.trim() === '' ? 'The skill' : `The skill ${skill.trim()}`;
  return `${what} is accepted${version > 1 ? ` (version ${version})` : ''}. Aqua reads it the next time it starts.`;
}

/** Why a name is not one a skill can have, or '' when it is fine. */
export function nameProblem(name: string): string {
  const n = name.trim();
  if (n === '') return 'Give the skill a name.';
  if (!NAME_SHAPE.test(n)) return 'A skill name has only a to z, 0 to 9, - and _ (at most 48 characters).';
  return '';
}

// ---- the calls -------------------------------------------------------------------------------------------------------------------------

const LIST: Wording = { fallback: 'The skills inbox could not be read.' };
const APPROVE: Wording = { fallback: 'The skill could not be accepted.' };
const REJECT: Wording = { fallback: 'The proposal could not be rejected.' };

export type Approval = { kind: 'accepted'; skill: string; version: number } | { kind: 'collision' };

/** What to tell the person when the name they are accepting under is one of their own skills. */
export function collisionSentence(name: string): string {
  const n = name.trim();
  return n === '' ? 'You already have a skill with that name, written by you.' : `You already have a skill called ${n}, written by you.`;
}

function checkedId(id: string): string {
  if (typeof id !== 'string' || !ID_SHAPE.test(id)) throw new CoreError('That proposal cannot be answered.', 400);
  return id;
}

export const skillsInboxApi = {
  /** The proposals, split into the ones that wait and the ones decided before. */
  list: async (): Promise<Inbox> => {
    const inbox = mapInbox(answerBody(await callCore('/api/v1/skills/proposals', undefined, LIST)));
    if (!inbox) throw new CoreError(UNREADABLE_ANSWER);
    return inbox;
  },
  /**
   * Accepts a proposal as it is, or with another name (`name`), or over the person's own skill of the same name (`replace`). When the name
   * is one of the person's own skills nothing is written and the answer is a `collision`, so that the screen can ask what to do.
   */
  approve: async (id: string, changes: { name?: string; replace?: boolean } = {}): Promise<Approval> => {
    const safe = checkedId(id);
    const body: Record<string, unknown> = {};
    if (changes.name !== undefined) {
      const problem = nameProblem(changes.name);
      if (problem !== '') throw new CoreError(problem, 400);
      body.name = changes.name.trim();
    }
    if (changes.replace === true) body.replace = true;
    const answer = await callCore(`/api/v1/skills/proposals/${encodeURIComponent(safe)}/approve`, jsonInit('POST', body), APPROVE);
    if (!answer.ok && answer.status === 409 && isObject(answer.body) && answer.body.collision === true) return { kind: 'collision' };
    const done = answerBody(answer);
    if (!isObject(done) || typeof done.skill !== 'string') throw new CoreError(UNREADABLE_ANSWER);
    return { kind: 'accepted', skill: done.skill, version: typeof done.version === 'number' && Number.isFinite(done.version) ? Math.floor(done.version) : 1 };
  },
  reject: async (id: string): Promise<void> => {
    const safe = checkedId(id);
    answerBody(await callCore(`/api/v1/skills/proposals/${encodeURIComponent(safe)}/reject`, jsonInit('POST'), REJECT));
  },
};
