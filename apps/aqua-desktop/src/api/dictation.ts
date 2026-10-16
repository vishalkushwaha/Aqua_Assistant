// Dictation: a clip of speech turned into clean text, and command mode, which rewrites a selection as the person said (section 5.7 of
// documentation/UI_CONTRACT.md; core/src/api_gateway/dictation_api.rs and core/src/dictation/). Every route here needs the UI token, which
// the shell or the dev proxy adds. The key that starts dictation and the typing into the other window are the shell's, not this app's:
// this module only asks for text and reads the answer, and the preview popup shows it before anything is typed.
//
//   POST /api/v1/dictation/transcribe {wav_base64}                 -> {heard, text, applied, note, latency_ms}
//   POST /api/v1/dictation/rewrite {selection, instruction, style} -> {preview: {text, diff, length_change}, applied: false}
//
// The clip is not kept by the core. Nothing here stores the text, the audio or the answer.

import { answerBody, callCore, CoreError, isObject, jsonInit, stringList, UNREADABLE_ANSWER, type Wording } from './coreRequest.ts';

export type DictationMode = 'dictation' | 'rewrite';

/** What a clip came to. `text` is what would be typed; `heard` is what the speech model wrote before the tidy-up. */
export interface Transcription {
  heard: string;
  text: string;
  /** What the tidy-up did to `heard`, in the core's words ("removed fillers", "capitalised"). */
  applied: string[];
  /** Something the person should know (no speech in the clip, a level that was not used), or null. */
  note: string | null;
}

/** One stretch of a rewrite: unchanged, taken out, or put in. */
export interface DiffPart {
  op: 'same' | 'delete' | 'insert';
  text: string;
}

export interface Rewrite {
  text: string;
  diff: DiffPart[];
  /** How many characters longer (or, below zero, shorter) the new text is, or null when the core does not say. */
  lengthChange: number | null;
}

/** A transcription, or null when the answer has no text at all (a missing `heard` is the text itself, a missing list is empty). */
export function mapTranscription(body: unknown): Transcription | null {
  if (!isObject(body) || typeof body.text !== 'string') return null;
  const note = typeof body.note === 'string' && body.note.trim() !== '' ? body.note.trim() : null;
  return {
    heard: typeof body.heard === 'string' ? body.heard : body.text,
    text: body.text,
    applied: stringList(body.applied),
    note,
  };
}

function mapPart(raw: unknown): DiffPart | null {
  if (!isObject(raw) || typeof raw.text !== 'string') return null;
  if (raw.op !== 'same' && raw.op !== 'delete' && raw.op !== 'insert') return null;
  return { op: raw.op, text: raw.text };
}

/** A rewrite preview, or null when the answer holds no new text. A part of the diff that is not a part is left out. */
export function mapRewrite(body: unknown): Rewrite | null {
  if (!isObject(body) || !isObject(body.preview) || typeof body.preview.text !== 'string') return null;
  const diff: DiffPart[] = [];
  if (Array.isArray(body.preview.diff)) {
    for (const part of body.preview.diff) {
      const p = mapPart(part);
      if (p) diff.push(p);
    }
  }
  const change = body.preview.length_change;
  return { text: body.preview.text, diff, lengthChange: typeof change === 'number' && Number.isFinite(change) ? Math.trunc(change) : null };
}

// ---- wording ---------------------------------------------------------------------------------------------------------------------------

/** What the tidy-up changed, as lines for the popup. */
export function tidyUpLines(t: Transcription): string[] {
  const lines: string[] = [];
  if (t.applied.length > 0) lines.push(`Changed from what was heard: ${t.applied.join(', ')}.`);
  else if (t.heard.trim() !== '') lines.push('Nothing was changed from what was heard.');
  if (t.note !== null) lines.push(t.note);
  return lines;
}

/** What the popup adds once the person has changed the text in the box: from then on it is no longer what the core wrote. */
export const EDITED_LINE = 'You have edited the text since.';

/** What a rewrite changed, in words: how much longer or shorter, and how many stretches changed. */
export function rewriteLines(r: Rewrite): string[] {
  const lines: string[] = [];
  const changes = r.diff.filter((p) => p.op !== 'same').length;
  if (r.diff.length > 0) lines.push(changes === 0 ? 'The text is the same.' : changes === 1 ? '1 stretch of the text changes.' : `${changes} stretches of the text change.`);
  if (r.lengthChange !== null && r.lengthChange !== 0) {
    const n = Math.abs(r.lengthChange);
    lines.push(`${r.lengthChange > 0 ? 'Longer' : 'Shorter'} by ${n} character${n === 1 ? '' : 's'}.`);
  }
  return lines;
}

/** Whether there is anything to type: text with more than white space in it. */
export function canType(text: string): boolean {
  return text.trim() !== '';
}

/** What the popup says the mode is. */
export function modeText(mode: DictationMode): string {
  return mode === 'rewrite' ? 'Rewriting your selection' : 'Dictation';
}

export function typeLabel(mode: DictationMode): string {
  return mode === 'rewrite' ? 'Replace the selection' : 'Type it';
}

// ---- the calls -------------------------------------------------------------------------------------------------------------------------

const TRANSCRIBE: Wording = {
  fallback: 'The recording could not be turned into text.',
  status: {
    413: 'This recording is longer than the limit set for dictation.',
    503: 'The speech engine is busy or still starting. Try again in a moment.',
  },
};
const REWRITE: Wording = {
  fallback: 'The selection could not be rewritten.',
  status: {
    404: 'Rewriting a selection is switched off in this version of Aqua.',
    502: 'The language model did not answer. Try again.',
    503: 'The language model is busy or still starting. Try again in a moment.',
  },
};

export const dictationApi = {
  /** Turns a recording (a 16 kHz mono 16-bit WAV, base64) into clean text. The core does not keep the clip. */
  transcribe: async (wavBase64: string): Promise<Transcription> => {
    if (typeof wavBase64 !== 'string' || wavBase64 === '') throw new CoreError('There is no recording to turn into text.', 400);
    const result = mapTranscription(answerBody(await callCore('/api/v1/dictation/transcribe', jsonInit('POST', { wav_base64: wavBase64 }), TRANSCRIBE)));
    if (!result) throw new CoreError(UNREADABLE_ANSWER);
    return result;
  },
  /** Command mode: a selection rewritten as the person said, as a preview. Nothing is applied by this call. */
  rewrite: async (selection: string, instruction: string, style?: string): Promise<Rewrite> => {
    if (selection.trim() === '') throw new CoreError('Select some text first.', 400);
    if (instruction.trim() === '') throw new CoreError('Say how the text should change.', 400);
    const body: Record<string, string> = { selection, instruction };
    if (style !== undefined && style.trim() !== '') body.style = style.trim();
    const result = mapRewrite(answerBody(await callCore('/api/v1/dictation/rewrite', jsonInit('POST', body), REWRITE)));
    if (!result) throw new CoreError(UNREADABLE_ANSWER);
    return result;
  },
};
