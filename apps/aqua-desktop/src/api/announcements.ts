// The sentences Aqua says on its own (a reminder that came due, a timer): the app's player takes the next one with
// POST /api/v1/announcements/next and tells what became of it with POST /api/v1/announcements/{id}/ack. Both need the UI token, which the
// shell or the dev proxy adds: a script cannot make Aqua talk, and cannot take a reminder away before it is heard (core/src/api_gateway/announce_api.rs).
//
// The contract has exactly two answers to a sentence: `{"spoken": true}` takes it off the queue, `{"spoken": false}` offers it again
// (three tries in all, then the core lets it go). There is no separate answer for "only shown" or "failed". Everything here is a plain
// function, so a test can call it without a page. The sentence is only ever handed to React as a text child.

import { callCore, isObject, isWholeNumber, jsonInit, positiveOrNull, text, type Wording } from './plain.ts';

export interface Announcement {
  id: number;
  /** The sentence to say, as it is to be spoken. */
  text: string;
  /** The kind of notification it comes from (`reminder`, `failed`, ...). */
  kind: string;
  /** When it was made (unix seconds), or null when the core does not say. */
  at: number | null;
}

/** One announcement, or null when it has no whole-number id or no sentence to say. */
export function mapAnnouncement(raw: unknown): Announcement | null {
  if (!isObject(raw) || !isWholeNumber(raw.id)) return null;
  const sentence = text(raw.text).trim();
  if (sentence === '') return null;
  return { id: raw.id, text: sentence, kind: text(raw.kind), at: positiveOrNull(raw.at) };
}

/** What the core handed out, or why nothing was. A failure is a sentence for the log, not for a person: the player shows nothing for it. */
export type NextAnswer =
  | { type: 'item'; item: Announcement }
  /** The core handed out something with an id but no sentence the app can say: it is answered "not spoken" so that it is let go. */
  | { type: 'unusable'; id: number }
  | { type: 'none' }
  | { type: 'failed'; sentence: string; status: number | null };

export type AckAnswer =
  | { type: 'acked' }
  /** The core says it is not waiting any more (it was said already, or it was too old): nothing is left to answer. */
  | { type: 'gone' }
  | { type: 'failed'; sentence: string; status: number | null };

const NEXT_WORDING: Wording = { fallback: 'The next announcement could not be asked for.', status: { 404: 'This version of Aqua has no spoken announcements.' } };
const ACK_WORDING: Wording = { fallback: 'The announcement could not be answered.' };

export const announcementsApi = {
  /**
   * Asks for the next sentence to say. `busy` true says that an answer is being spoken or a person is talking: the core hands out
   * nothing then, and counts the player as there all the same. 204 means there is nothing to say now.
   */
  next: async (busy: boolean): Promise<NextAnswer> => {
    const answer = await callCore('/api/v1/announcements/next', jsonInit('POST', { busy }), NEXT_WORDING);
    if (!answer.ok) return { type: 'failed', sentence: answer.sentence, status: answer.status };
    if (answer.status === 204) return { type: 'none' };
    const item = mapAnnouncement(answer.body);
    if (item) return { type: 'item', item };
    // A 200 whose body is not a sentence: with an id it can be let go; without one there is nothing to answer.
    if (isObject(answer.body) && isWholeNumber(answer.body.id)) return { type: 'unusable', id: answer.body.id };
    return { type: 'failed', sentence: 'Aqua sent an announcement the app could not read.', status: answer.status };
  },
  /** Tells what became of a sentence: `spoken` true takes it off the queue, false offers it again (three tries in all). */
  ack: async (id: number, spoken: boolean): Promise<AckAnswer> => {
    if (!isWholeNumber(id)) return { type: 'failed', sentence: ACK_WORDING.fallback, status: null };
    const answer = await callCore(`/api/v1/announcements/${id}/ack`, jsonInit('POST', { spoken }), ACK_WORDING);
    if (answer.ok) return { type: 'acked' };
    if (answer.status === 404) return { type: 'gone' };
    return { type: 'failed', sentence: answer.sentence, status: answer.status };
  },
};
