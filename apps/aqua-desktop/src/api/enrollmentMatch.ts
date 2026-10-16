// What the voice enrolment page may conclude from what the speech engine said about a recording.
//
// The page used to click its own "next" button (and at the last step "Save & Activate Profile", which replaces the enrolled voiceprint) 2.5
// seconds after any text that contained the wake phrase or the word "aqua". That text includes the page's own stand-ins for a failed
// transcription: "[STT Error: Aqua is busy with another request. Try again in a moment.]" contains "aqua", so while the core was answering
// someone else, every step "matched" and the voiceprint was replaced by audio that was never transcribed. An empty wake phrase matched
// everything as well.

/** The page's own messages that stand where the words of a recording would be. */
const NOT_WORDS = /^\[(STT Error|No STT result|No speech detected)/i;

/** Were words really heard (the speech engine answered, and found some), as opposed to an error or an empty result? */
export function heardWords(sttResult: string | undefined | null): boolean {
  const text = (sttResult ?? '').trim();
  return text.length > 0 && !NOT_WORDS.test(text);
}

const clean = (s: string) => s.toLowerCase().replace(/[^a-z0-9\s]/g, '').trim();

/** Did the person say the wake phrase (or "aqua"), judged only on words that were heard? An empty phrase matches nothing. */
export function saidWakePhrase(sttResult: string | undefined | null, wakeupWord: string): boolean {
  if (!heardWords(sttResult)) return false;
  const heard = clean(sttResult ?? '');
  const phrase = clean(wakeupWord);
  return (phrase.length > 0 && heard.includes(phrase)) || heard.includes('aqua');
}

/**
 * May the page move on by itself after this recording? Only for a step that is not the last: saving the voiceprint over the one that is
 * enrolled is the person's own click, whatever was heard.
 */
export function mayAdvanceByItself(step: number, lastStep: number, sttResult: string | undefined | null, wakeupWord: string): boolean {
  return step < lastStep && saidWakePhrase(sttResult, wakeupWord);
}
