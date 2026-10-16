import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react';
import { errorSentence } from '../api/coreRequest';
import {
  decidedSentence,
  describeSuggestion,
  factLine,
  factsById,
  factText,
  memoryApi,
  NOT_THIS_HINT,
  PRIVATE_HINT,
  REMEMBER_HINT,
  type Fact,
  type MemoryFacts,
  type Suggestions,
} from '../api/memory';
import { falkorApi } from '../api/falkorApi';
import { PageHeader, Section, Switch } from './ui';
import { alertText, input, mark, muted, noticeText, row, rowBorder, small } from './screenStyles';

/**
 * What Aqua remembers about the person, and what its memory suggests. Facts show their text, when they were learned and where from, and
 * each can be forgotten (the question is asked here in the panel). A suggestion is shown on its own with the exact change it would make:
 * Remember makes it, Not this changes nothing and Aqua does not suggest it again. The page stores nothing: it shows what the core last said,
 * and reads it again after every answer. Everything that comes from the core (a fact, a reason) is a text child, never markup.
 */
export function MemoryPage() {
  const [memory, setMemory] = useState<MemoryFacts | null>(null);
  const [memoryError, setMemoryError] = useState<string | null>(null);
  const [suggestions, setSuggestions] = useState<Suggestions | null>(null);
  const [suggestionsError, setSuggestionsError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [draft, setDraft] = useState('');
  const [confirming, setConfirming] = useState<number | 'all' | null>(null);
  const [busy, setBusy] = useState(false);
  // Only the newest answer is used: a slow read must not overwrite a read made after the person answered something.
  const factsSeq = useRef(0);
  const suggestionsSeq = useRef(0);

  const loadFacts = useCallback(async () => {
    const mine = ++factsSeq.current;
    try {
      const list = await memoryApi.facts();
      if (mine !== factsSeq.current) return;
      setMemory(list);
      setMemoryError(null);
    } catch (e) {
      if (mine !== factsSeq.current) return;
      setMemoryError(errorSentence(e));
    }
  }, []);

  const loadSuggestions = useCallback(async () => {
    const mine = ++suggestionsSeq.current;
    try {
      const list = await memoryApi.suggestions();
      if (mine !== suggestionsSeq.current) return;
      setSuggestions(list);
      setSuggestionsError(null);
    } catch (e) {
      if (mine !== suggestionsSeq.current) return;
      setSuggestionsError(errorSentence(e));
    }
  }, []);

  useEffect(() => {
    void loadFacts();
    void loadSuggestions();
  }, [loadFacts, loadSuggestions]);

  // One change at a time. Whatever came of it, the page reads the core again: it shows what is so, not what was asked for.
  const act = async (run: () => Promise<string | void>) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const message = await run();
      if (message) setNotice(message);
    } catch (e) {
      setError(errorSentence(e));
    } finally {
      setBusy(false);
      setConfirming(null);
    }
    await Promise.all([loadFacts(), loadSuggestions()]);
  };

  const remember = (e: FormEvent) => {
    e.preventDefault();
    const textToRemember = draft.trim();
    if (!textToRemember) return;
    void act(async () => {
      await memoryApi.remember(textToRemember);
      let storedInGraph = true;
      try {
        await falkorApi.query(
          `MERGE (u:User {id: 'usr_owner'})
           CREATE (m:Memory {id: 'mem_' + toString(timestamp()), text: $text, created_at: timestamp()})
           CREATE (u)-[:REMEMBERS]->(m)`,
          { text: textToRemember }
        );
      } catch { storedInGraph = false; }
      setDraft('');
      return storedInGraph
        ? 'Remembered in Aqua and FalkorDB Graph Memory.'
        : 'Remembered in Aqua. FalkorDB was unavailable, so this memory is not in the graph yet.';
    });
  };

  const forget = (fact: Fact) =>
    act(async () => {
      const { forgotten } = await memoryApi.forget(fact.id);
      return forgotten ? 'Forgot that.' : 'That was already gone.';
    });

  const forgetAll = () =>
    act(async () => {
      const { deleted } = await memoryApi.forgetAll();
      return deleted === 1 ? 'Forgot the 1 thing Aqua remembered.' : `Forgot the ${deleted} things Aqua remembered.`;
    });

  const now = Date.now() / 1000;
  const facts = memory?.facts ?? [];
  const known = factsById(facts);
  const waiting = suggestions?.suggestions ?? [];
  const current = waiting[0];
  const words = current ? describeSuggestion(current, known) : null;

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 980, margin: '0 auto' }}>
      <PageHeader
        title="Memory"
        subtitle="What Aqua remembers about you, where each thing came from, and what it suggests changing. Nothing becomes a fact until you say so, and forgetting is forgetting: the entry is deleted."
        actions={
          <button type="button" className="btn-ghost" disabled={busy} onClick={() => void act(() => memoryApi.lookForSuggestions())}>
            Look for suggestions now
          </button>
        }
      />

      {error && <div role="alert" style={alertText}>{error}</div>}
      {notice && <div role="status" style={noticeText}>{notice}</div>}

      <Section title="Suggestions" hint="Aqua notices when the same thing is remembered twice, when a fact holds a date like “tomorrow”, and when you correct an answer. It only suggests: you decide.">
        {suggestionsError && <div role="alert" style={alertText}>{suggestionsError}</div>}
        {suggestions === null && !suggestionsError && <div style={muted}>Loading…</div>}
        {suggestions !== null && !current && <div style={muted}>Nothing to look at. When Aqua finds something to tidy, it is shown here.</div>}
        {current && words && (
          <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
            <div style={muted}>Suggestion 1 of {waiting.length}</div>
            <div style={{ fontSize: 14.5, fontWeight: 600 }}>{words.title}</div>
            {words.lines.map((line, i) => (
              <div key={i} style={{ fontSize: 13.5, overflowWrap: 'anywhere' }}>{line}</div>
            ))}
            {words.because !== '' && <div style={muted}>{words.because}</div>}
            <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap', alignItems: 'center' }}>
              <button type="button" className="btn-primary" disabled={busy} onClick={() => void act(async () => { await memoryApi.decide(current.id, true); return decidedSentence(true); })}>
                Remember
              </button>
              <button type="button" className="btn-ghost" disabled={busy} onClick={() => void act(async () => { await memoryApi.decide(current.id, false); return decidedSentence(false); })}>
                Not this
              </button>
            </div>
            <div style={muted}>{REMEMBER_HINT} {NOT_THIS_HINT}</div>
          </div>
        )}
      </Section>

      <Section title="What Aqua remembers" hint="Each entry is one short line. A password or a key is never kept.">
        {memoryError && <div role="alert" style={alertText}>{memoryError}</div>}
        {memory !== null && (
          <Switch
            label="Incognito"
            hint="While this is on, Aqua remembers nothing new. What it already remembers stays."
            checked={memory.incognito}
            disabled={busy}
            onChange={(on) => void act(async () => ((await memoryApi.setIncognito(on)) ? 'Aqua is not remembering anything for now.' : 'Aqua remembers again.'))}
          />
        )}
        <form onSubmit={remember} style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          <input
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            placeholder="Something Aqua should remember, for example: my sister is called Asha"
            aria-label="Something for Aqua to remember"
            style={input}
          />
          <button type="submit" className="btn-primary" disabled={busy || draft.trim() === ''}>
            Remember this
          </button>
        </form>
        {memory === null && !memoryError && <div style={muted}>Loading…</div>}
        {memory !== null && facts.length === 0 && <div style={muted}>Aqua does not remember anything about you yet. Say “remember that …” to Aqua, or add something above.</div>}
        {facts.length > 0 && (
          <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
            {facts.map((f, i) => (
              <li key={f.id} style={{ ...row, borderTop: i === 0 ? 'none' : rowBorder }}>
                <div style={{ flex: '1 1 280px', minWidth: 0 }}>
                  <div style={{ fontSize: 13.5, fontWeight: 600, overflowWrap: 'anywhere' }}>{factText(f.text)}</div>
                  <div style={muted}>{factLine(f, now)}</div>
                </div>
                <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
                  {f.sensitive && <span style={mark} title={PRIVATE_HINT}>private</span>}
                  {confirming === f.id ? (
                    <>
                      <span style={{ fontSize: 13 }}>Forget this?</span>
                      <button type="button" className="btn-danger" style={small} disabled={busy} onClick={() => void forget(f)}>Yes, forget it</button>
                      <button type="button" className="btn-ghost" style={small} disabled={busy} onClick={() => setConfirming(null)}>Keep</button>
                    </>
                  ) : (
                    <button type="button" className="btn-ghost" style={small} disabled={busy} aria-label={`Forget: ${factText(f.text)}`} onClick={() => { setNotice(null); setConfirming(f.id); }}>
                      Forget
                    </button>
                  )}
                </div>
              </li>
            ))}
          </ul>
        )}
        {facts.length > 0 && (
          <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap', alignItems: 'center' }}>
            {confirming === 'all' ? (
              <>
                <span style={{ fontSize: 13.5 }}>Forget everything Aqua remembers about you? This cannot be undone.</span>
                <button type="button" className="btn-danger" disabled={busy} onClick={() => void forgetAll()}>Yes, forget everything</button>
                <button type="button" className="btn-ghost" disabled={busy} onClick={() => setConfirming(null)}>Keep it</button>
              </>
            ) : (
              <button type="button" className="btn-danger" disabled={busy} onClick={() => { setNotice(null); setConfirming('all'); }}>Forget everything</button>
            )}
          </div>
        )}
      </Section>
    </div>
  );
}
