// Why an update was not offered. The desktop shell (src-tauri/src/updates.rs) judges the update information before an update is
// offered; when it refuses it (the information has expired, it is older than one already accepted, the installer was signed as
// another version, ...), `update_status` and `check_for_update` carry a plain sentence in `refusal`. The prompt shows that
// sentence so a person is told why no update appears, instead of being left to think there is none.
//
// The sentence is text. It comes from outside the page, so the component renders it as a text child and never as markup; these
// functions return it as it is, only trimmed and cut to a sensible length.

/** The longest refusal shown. The shell's sentences are short; anything longer is cut rather than allowed to fill the screen. */
export const MAX_REFUSAL_CHARS = 600;

/** The sentence to show for a `refusal` field, or `null` when there is none (absent, null, not a string, or only blanks). */
export function refusalSentence(value: unknown): string | null {
  if (typeof value !== 'string') return null;
  const text = value.trim();
  if (text === '') return null;
  const letters = Array.from(text);
  return letters.length > MAX_REFUSAL_CHARS ? `${letters.slice(0, MAX_REFUSAL_CHARS - 1).join('')}…` : text;
}

/**
 * What to show after a check. `shown` is what is on the screen now, `statusRefusal` is the `refusal` of `update_status`, and
 * `found` is the answer of `check_for_update`, or `undefined` when that call failed.
 *
 * - The check answered: its own `refusal` is the truth. No refusal in it (an update was found, or there is nothing newer)
 *   clears what was shown.
 * - The check failed (no network, say): nothing is learned, so what was shown stays. If nothing was shown, the shell's
 *   memory of its last refusal (`statusRefusal`) is used, so a refusal is not lost because the next check failed.
 */
export function refusalToShow(shown: string | null, statusRefusal: unknown, found: { refusal?: unknown } | null | undefined): string | null {
  if (found !== undefined && found !== null) return refusalSentence(found.refusal);
  return shown ?? refusalSentence(statusRefusal);
}
