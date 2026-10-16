import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react';
import { errorSentence } from '../api/coreRequest';
import {
  confirmsForgetting,
  documentsText,
  FORGET_PHRASE,
  passagesText,
  passageWhere,
  readingLines,
  RESULTS_VISIBLE_SECONDS,
  statusText,
  vaultApi,
  type SearchResult,
  type VaultState,
} from '../api/vault';
import { PageHeader, Pill, Section } from './ui';
import { alertText, input, muted, noticeText, row, rowBorder, small } from './screenStyles';

/** How often the status is read while a reading of the folders is going (never faster than once a second). */
const READING_POLL_MS = 3000;

/**
 * The folders Aqua reads so that it can answer from the person's own files and saved pages. It lists the folders (names only), adds and
 * takes them off, starts or pauses the reading, searches what was read, and forgets everything. This vault holds documents, not secrets or
 * values. The text a search finds is text out of the person's files: it is shown only after a search the person made, as plain text, and
 * it is dropped from the page after a minute, when the person hides it, and when they leave. Nothing is logged or kept in the browser.
 */
export function VaultPage() {
  const [vault, setVault] = useState<VaultState | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [folderDraft, setFolderDraft] = useState('');
  const [removing, setRemoving] = useState<string | null>(null);
  const [forgetting, setForgetting] = useState(false);
  const [typed, setTyped] = useState('');
  const [query, setQuery] = useState('');
  const [searching, setSearching] = useState(false);
  const [result, setResult] = useState<SearchResult | null>(null);
  const [searchError, setSearchError] = useState<string | null>(null);
  // Only the newest answer is used.
  const statusSeq = useRef(0);
  const searchSeq = useRef(0);

  const load = useCallback(async () => {
    const mine = ++statusSeq.current;
    try {
      const state = await vaultApi.state();
      if (mine !== statusSeq.current) return;
      setVault(state);
      setLoadError(null);
    } catch (e) {
      if (mine !== statusSeq.current) return;
      setLoadError(errorSentence(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // While a reading goes on the status is read again now and then; when it ends, or the page is left, that stops.
  const reading = vault?.indexing === true;
  useEffect(() => {
    if (!reading) return;
    const id = setInterval(() => {
      if (!document.hidden) void load();
    }, READING_POLL_MS);
    return () => clearInterval(id);
  }, [reading, load]);

  // Found passages are text out of the person's files: they do not stay on the screen.
  useEffect(() => {
    if (result === null) return;
    const id = setTimeout(() => setResult(null), RESULTS_VISIBLE_SECONDS * 1000);
    return () => clearTimeout(id);
  }, [result]);

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
      setRemoving(null);
    }
    await load();
  };

  const addFolder = (e: FormEvent) => {
    e.preventDefault();
    void act(async () => {
      const { folder } = await vaultApi.addFolder(folderDraft);
      setFolderDraft('');
      return `Added ${folder}. Nothing is read until you start reading.`;
    });
  };

  const removeFolder = (path: string) =>
    act(async () => {
      const { forgotten, note } = await vaultApi.removeFolder(path);
      const base = forgotten === 1 ? 'Took the folder off and forgot 1 document.' : `Took the folder off and forgot ${forgotten} documents.`;
      return note ? `${base} ${note}` : base;
    });

  const forgetAll = () =>
    act(async () => {
      const { forgotten } = await vaultApi.forgetAll(typed);
      setResult(null);
      setTyped('');
      setForgetting(false);
      return forgotten === 1 ? 'Forgot 1 document, and the list of folders.' : `Forgot ${forgotten} documents, and the list of folders.`;
    });

  const startReading = () =>
    act(async () => {
      const { started } = await vaultApi.startReading();
      return started ? 'Started reading the folders.' : 'The folders are being read already.';
    });

  const setPaused = (paused: boolean) =>
    act(async () => ((await vaultApi.setPaused(paused)) ? 'Reading is paused.' : 'Reading can go on.'));

  const search = async (e: FormEvent) => {
    e.preventDefault();
    const mine = ++searchSeq.current;
    setSearching(true);
    setSearchError(null);
    setResult(null);
    try {
      const found = await vaultApi.search(query);
      if (mine !== searchSeq.current) return;
      setResult(found);
    } catch (err) {
      if (mine !== searchSeq.current) return;
      setSearchError(errorSentence(err));
    } finally {
      if (mine === searchSeq.current) setSearching(false);
    }
  };

  const folders = vault?.folders ?? [];
  const lines = vault && !vault.indexing && vault.lastReading ? readingLines(vault.lastReading) : [];

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 980, margin: '0 auto' }}>
      <PageHeader
        title="Vault"
        subtitle="The folders Aqua reads so that it can answer from your own files and saved pages. It all stays on this computer. Nothing is read until you add a folder and start reading, and files whose names look like passwords or keys are never read."
      />

      {error && <div role="alert" style={alertText}>{error}</div>}
      {notice && <div role="status" style={noticeText}>{notice}</div>}

      <Section
        title="What is in the vault"
        actions={
          vault ? (
            <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
              <button type="button" className="btn-primary" disabled={busy || vault.indexing || folders.length === 0} onClick={() => void startReading()}>
                Read the folders now
              </button>
              <button type="button" className="btn-ghost" disabled={busy} onClick={() => void setPaused(!vault.paused)}>
                {vault.paused ? 'Resume reading' : 'Pause reading'}
              </button>
            </div>
          ) : undefined
        }
      >
        {loadError && <div role="alert" style={alertText}>{loadError}</div>}
        {vault === null && !loadError && <div style={muted}>Loading…</div>}
        {vault !== null && (
          <>
            <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap' }}>
              <Pill label="Documents" value={documentsText(vault.documents)} />
              <Pill label="Held as" value={passagesText(vault.passages)} title="Each document is cut into passages of about a thousand characters, and a passage is what a search finds." />
              <Pill label="Folders" value={folders.length} />
              <Pill tone={vault.indexing ? 'attention' : vault.paused ? 'neutral' : 'ok'} label={vault.indexing ? 'Reading' : vault.paused ? 'Paused' : 'Ready'} live={vault.indexing} />
            </div>
            <div style={{ fontSize: 13.5 }}>{statusText(vault)}</div>
            {lines.length > 0 && (
              <div style={{ display: 'flex', flexDirection: 'column', gap: 2 }}>
                <div style={{ ...muted, fontWeight: 600 }}>The last reading</div>
                {lines.map((l, i) => (
                  <div key={i} style={muted}>{l}</div>
                ))}
              </div>
            )}
          </>
        )}
      </Section>

      <Section title="Folders" hint="Up to 20. Taking a folder off forgets everything read from it: the stored text is overwritten, not just hidden.">
        {vault !== null && folders.length === 0 && <div style={muted}>No folder yet.</div>}
        {folders.length > 0 && (
          <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
            {folders.map((f, i) => (
              <li key={f} style={{ ...row, borderTop: i === 0 ? 'none' : rowBorder }}>
                <div style={{ flex: '1 1 280px', minWidth: 0, fontSize: 13.5, overflowWrap: 'anywhere' }}>{f}</div>
                <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
                  {removing === f ? (
                    <>
                      <span style={{ fontSize: 13 }}>Take it off and forget what was read from it?</span>
                      <button type="button" className="btn-danger" style={small} disabled={busy} onClick={() => void removeFolder(f)}>Yes, forget it</button>
                      <button type="button" className="btn-ghost" style={small} disabled={busy} onClick={() => setRemoving(null)}>Keep it</button>
                    </>
                  ) : (
                    <button type="button" className="btn-ghost" style={small} disabled={busy} aria-label={`Take off the folder ${f}`} onClick={() => { setNotice(null); setRemoving(f); }}>
                      Take off
                    </button>
                  )}
                </div>
              </li>
            ))}
          </ul>
        )}
        <form onSubmit={addFolder} style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          <input value={folderDraft} onChange={(e) => setFolderDraft(e.target.value)} placeholder="C:\Users\you\Documents" aria-label="Full path of a folder to read" style={input} />
          <button type="submit" className="btn-ghost" disabled={busy || folderDraft.trim() === ''}>Add folder</button>
        </form>
      </Section>

      <Section title="Search what was read" hint="A keyword search: a passage is found when it has the words you type. The passages are text out of your files, so they are hidden again after a minute.">
        <form onSubmit={(e) => void search(e)} style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          <input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="invoice March" aria-label="Words to look for" style={input} />
          <button type="submit" className="btn-ghost" disabled={searching || query.trim() === ''}>Search</button>
        </form>
        {searchError && <div role="alert" style={alertText}>{searchError}</div>}
        {result !== null && result.passages.length === 0 && <div style={muted}>Nothing found. {result.note}</div>}
        {result !== null && result.passages.length > 0 && (
          <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
            {result.passages.map((p, i) => (
              <div key={i} style={{ display: 'flex', flexDirection: 'column', gap: 4, paddingTop: i === 0 ? 0 : 12, borderTop: i === 0 ? 'none' : rowBorder }}>
                <div style={{ ...muted, overflowWrap: 'anywhere' }}>{passageWhere(p)}{p.source === 'saved page' ? ' (saved page)' : ''}</div>
                <div style={{ fontSize: 13.5, lineHeight: 1.55, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', userSelect: 'text' }}>{p.quote}</div>
              </div>
            ))}
            {result.note !== '' && <div style={muted}>{result.note}</div>}
            <div>
              <button type="button" className="btn-ghost" style={small} onClick={() => setResult(null)}>Hide the results</button>
            </div>
          </div>
        )}
      </Section>

      <Section title="Forget everything" hint="Deletes every document that was read, every saved page, and the list of folders. The stored text is overwritten. This cannot be undone.">
        {!forgetting ? (
          <div>
            <button type="button" className="btn-danger" disabled={busy || vault === null} onClick={() => { setNotice(null); setForgetting(true); }}>Forget everything</button>
          </div>
        ) : (
          <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
            <span style={{ fontSize: 13.5 }}>To be sure, type “{FORGET_PHRASE}”:</span>
            <input value={typed} onChange={(e) => setTyped(e.target.value)} aria-label={`Type ${FORGET_PHRASE} to confirm`} style={{ ...input, flex: '0 1 220px' }} />
            <button type="button" className="btn-danger" disabled={busy || !confirmsForgetting(typed)} onClick={() => void forgetAll()}>Forget everything</button>
            <button type="button" className="btn-ghost" disabled={busy} onClick={() => { setForgetting(false); setTyped(''); }}>Keep it</button>
          </div>
        )}
      </Section>
    </div>
  );
}
