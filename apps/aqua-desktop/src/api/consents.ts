// What the person said yes to, as the Consents panel shows it (GET /api/v1/consents) and taking one back
// (DELETE /api/v1/consents/{id}). Both need the UI token, which the shell or the dev proxy adds.
//
// Three kinds of thing are listed: the "approve for this session" answers that still apply (each lapses by itself), the sites allowed
// for good (they stay until taken back), and the calls refused lately (only shown). Everything here is a plain function, so a test can
// call it without a page. Text from the core (a target, a summary, a reason) is only ever handed to React as a text child.

import { agoText, AquaError, bodyOrThrow, callCore, clip, isObject, jsonInit, positiveOrNull, serverSentence, text, UNREADABLE_ANSWER, type Wording } from './plain.ts';

/** An "approve for this session" answer that still applies. */
export interface ToolGrant {
  /** What goes into `DELETE /api/v1/consents/{id}`; null when the core sent none (the row is shown, but cannot be taken back here). */
  id: string | null;
  tool: string;
  /** What the answer lets through (a path, an address, a command), already masked by the core. */
  target: string;
  grantedSecsAgo: number | null;
  expiresInSecs: number | null;
}

/** A site allowed for good. */
export interface SiteConsent {
  id: string | null;
  site: string;
}

/** A call that was refused lately. */
export interface Denial {
  atMs: number | null;
  tool: string;
  summary: string;
  /** Who refused: `user`, `timeout`, `policy`, `skill`, `hook`, `background` or `limit`. */
  by: string;
  reason: string;
}

export interface Consents {
  grants: ToolGrant[];
  sites: SiteConsent[];
  denials: Denial[];
  /** The core's own sentence about how long a yes lasts, or ''. */
  note: string;
}

const MAX_ID_CHARS = 300;

/** An id that may go into a path: a short text without control characters. */
function usableId(value: unknown): string | null {
  if (typeof value !== 'string') return null;
  const id = value.trim();
  return id !== '' && id.length <= MAX_ID_CHARS && !/[\u0000-\u001f\u007f]/.test(id) ? id : null;
}

export function mapGrant(raw: unknown): ToolGrant | null {
  if (!isObject(raw)) return null;
  return {
    id: usableId(raw.id),
    tool: text(raw.tool),
    target: text(raw.target),
    grantedSecsAgo: typeof raw.granted_secs_ago === 'number' && Number.isFinite(raw.granted_secs_ago) && raw.granted_secs_ago >= 0 ? raw.granted_secs_ago : null,
    expiresInSecs: typeof raw.expires_in_secs === 'number' && Number.isFinite(raw.expires_in_secs) && raw.expires_in_secs >= 0 ? raw.expires_in_secs : null,
  };
}

/** A site row: the site comes from `site`, or from the id (`site:<host>`) when `site` is missing. A row with neither is left out. */
export function mapSite(raw: unknown): SiteConsent | null {
  if (!isObject(raw)) return null;
  let id = usableId(raw.id);
  let site = text(raw.site).trim();
  if (site === '' && id !== null && id.startsWith('site:')) site = id.slice('site:'.length);
  if (site === '') return null;
  // The id of a site is `site:<host>` whatever the core wrote: only that form is taken back as a site.
  if (id === null || !id.startsWith('site:')) id = `site:${site}`;
  return { id, site };
}

export function mapDenial(raw: unknown): Denial | null {
  if (!isObject(raw)) return null;
  return { atMs: positiveOrNull(raw.at_ms), tool: text(raw.tool), summary: text(raw.summary), by: text(raw.by), reason: text(raw.reason) };
}

function listOf<T>(value: unknown, map: (raw: unknown) => T | null): T[] {
  if (!Array.isArray(value)) return [];
  const out: T[] = [];
  for (const row of value) {
    const mapped = map(row);
    if (mapped) out.push(mapped);
  }
  return out;
}

/** Newest answer first; one without a time last. */
function newestGrantFirst(a: ToolGrant, b: ToolGrant): number {
  return (a.grantedSecsAgo ?? Number.POSITIVE_INFINITY) - (b.grantedSecsAgo ?? Number.POSITIVE_INFINITY);
}

/** The answer of the list route as the panel uses it, or null when it is not a list of consents at all. */
export function mapConsents(body: unknown): Consents | null {
  if (!isObject(body)) return null;
  if (!Array.isArray(body.tool_grants) && !Array.isArray(body.sites) && !Array.isArray(body.recent_denials)) return null;
  return {
    grants: listOf(body.tool_grants, mapGrant).sort(newestGrantFirst),
    sites: listOf(body.sites, mapSite).sort((a, b) => a.site.localeCompare(b.site)),
    denials: listOf(body.recent_denials, mapDenial).sort((a, b) => (b.atMs ?? 0) - (a.atMs ?? 0)),
    note: clip(text(body.note).trim(), 240),
  };
}

/** How long a session yes lasts from now: "lapses in 12 min". Empty when the core did not say. */
export function lapsesText(expiresInSecs: number | null): string {
  if (expiresInSecs === null) return '';
  if (expiresInSecs < 30) return 'lapses in a few seconds';
  if (expiresInSecs < 90) return 'lapses in about a minute';
  return `lapses in ${Math.round(expiresInSecs / 60)} min`;
}

/** When a session yes was given: "said yes just now", "said yes 5 min ago". Empty when the core did not say. */
export function givenText(grantedSecsAgo: number | null): string {
  if (grantedSecsAgo === null) return '';
  if (grantedSecsAgo < 90) return 'said yes just now';
  if (grantedSecsAgo < 5400) return `said yes ${Math.round(grantedSecsAgo / 60)} min ago`;
  return `said yes ${Math.round(grantedSecsAgo / 3600)} h ago`;
}

/** The line under a session yes: when it was given and when it lapses. */
export function grantFacts(grant: ToolGrant): string {
  return [givenText(grant.grantedSecsAgo), lapsesText(grant.expiresInSecs)].filter((s) => s !== '').join(' · ');
}

/** Who refused a call, in words. A reason the app does not know is shown as the core wrote it. */
export function refusedByText(by: string): string {
  switch (by) {
    case 'user': return 'you said no';
    case 'timeout': return 'nobody answered in time';
    case 'policy': return 'the policy refused it';
    case 'skill': return "a skill's rules refused it";
    case 'hook': return 'a hook refused it';
    case 'background': return 'it ran in the background, where nobody could be asked';
    case 'limit': return 'three refusals in a row stopped the run';
    case '': return 'refused';
    default: return `refused (${clip(by, 40)})`;
  }
}

/** The line under a refused call: who refused and when. */
export function denialFacts(denial: Denial, nowSecs: number): string {
  const when = denial.atMs === null ? '' : agoText(denial.atMs / 1000, nowSecs);
  return [refusedByText(denial.by), when].filter((s) => s !== '').join(' · ');
}

/** The sentence under a successful take-back (the id was a site, or a session answer). */
export const TAKEN_BACK = 'Taken back. The next call of that kind asks again.';
export const ALREADY_LAPSED = 'That yes had lapsed already, so there was nothing to take back.';

const LIST_WORDING: Wording = { fallback: 'The list of what you said yes to could not be read.', status: { 404: 'This version of Aqua cannot list what you said yes to.' } };
const TAKE_BACK_WORDING: Wording = { fallback: 'That yes could not be taken back.' };

export const consentsApi = {
  /** What was said yes to and what was refused lately (UI token). */
  list: async (): Promise<Consents> => {
    const consents = mapConsents(bodyOrThrow(await callCore('/api/v1/consents', undefined, LIST_WORDING)));
    if (!consents) throw new AquaError(UNREADABLE_ANSWER);
    return consents;
  },
  /**
   * Takes one yes back (UI token): `site:<host>` for a site, or the id a session answer was listed with. `takenBack` is false when the
   * core says there is no such yes (it lapsed): nothing is left to take back, which is not an error.
   */
  takeBack: async (id: string): Promise<{ takenBack: boolean }> => {
    const usable = usableId(id);
    if (usable === null) throw new AquaError('That yes cannot be taken back from here.');
    // The id goes into one path segment whole: `site:10.0.0.0/8` has a slash and a colon in it.
    const answer = await callCore(`/api/v1/consents/${encodeURIComponent(usable)}`, jsonInit('DELETE'), TAKE_BACK_WORDING);
    // The core's own 404 comes with a sentence ("there is no such consent"); a 404 without one is a core that has no such route.
    if (!answer.ok && answer.status === 404 && serverSentence(answer.body) !== '') return { takenBack: false };
    bodyOrThrow(answer);
    return { takenBack: true };
  },
};
