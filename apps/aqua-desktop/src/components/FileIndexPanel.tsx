import { useCallback, useEffect, useState } from 'react';
import { fileIndexApi, type IndexState } from '../api/fileIndex';
import { PageHeader, Pill, Section, UnavailableState, type Tone } from './ui';

const INPUT_STYLE = {
  flex: 1,
  minWidth: 0,
  padding: '9px 12px',
  borderRadius: 8,
  border: '1px solid var(--glass-border)',
  background: 'rgba(0,0,0,0.3)',
  color: 'var(--text-primary)',
  outline: 'none',
} as const;

function stateText(s: IndexState['status']): { tone: Tone; text: string } {
  switch (s.state) {
    case 'disabled':
      return { tone: 'neutral', text: 'Off' };
    case 'scanning':
      return { tone: 'attention', text: s.complete ? 'Updating' : 'Scanning your computer' };
    case 'idle':
      return { tone: 'ok', text: 'Up to date' };
    case 'paused':
      return { tone: 'neutral', text: `Waiting: ${s.paused_reason ?? 'paused'}` };
    case 'error':
      return { tone: 'problem', text: 'Problem' };
    default:
      return { tone: 'neutral', text: 'Starting' };
  }
}

const KB = 1024;
function size(bytes: number): string {
  if (bytes >= KB * KB) return `${(bytes / KB / KB).toFixed(1)} MB`;
  return `${Math.max(1, Math.round(bytes / KB))} KB`;
}

function FolderList({ title, hint, items, onAdd, onRemove, placeholder }: { title: string; hint: string; items: string[]; onAdd: (p: string) => void; onRemove: (p: string) => void; placeholder: string }) {
  const [draft, setDraft] = useState('');
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
      <div>
        <strong style={{ fontSize: 14 }}>{title}</strong>
        <p style={{ fontSize: 13, color: 'var(--text-secondary)', lineHeight: 1.45 }}>{hint}</p>
      </div>
      {items.length > 0 && (
        <ul style={{ listStyle: 'none', display: 'flex', flexDirection: 'column', gap: 6 }}>
          {items.map((p) => (
            <li key={p} style={{ display: 'flex', gap: 10, alignItems: 'center', justifyContent: 'space-between', fontSize: 13.5 }}>
              <span style={{ overflowWrap: 'anywhere' }}>{p}</span>
              <button className="btn-ghost" onClick={() => onRemove(p)} style={{ padding: '4px 12px' }}>
                Remove
              </button>
            </li>
          ))}
        </ul>
      )}
      <form
        style={{ display: 'flex', gap: 8 }}
        onSubmit={(e) => {
          e.preventDefault();
          if (draft.trim()) {
            onAdd(draft.trim());
            setDraft('');
          }
        }}
      >
        <input value={draft} onChange={(e) => setDraft(e.target.value)} placeholder={placeholder} style={INPUT_STYLE} aria-label={title} />
        <button className="btn-ghost" type="submit">
          Add
        </button>
      </form>
    </div>
  );
}

/** The file index: what Aqua knows about the folders, programs and shortcuts on this computer, and the controls. */
export function FileIndexPanel() {
  const [st, setSt] = useState<IndexState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirmForget, setConfirmForget] = useState(false);

  const refresh = useCallback(async () => setSt(await fileIndexApi.state()), []);
  useEffect(() => {
    void refresh();
    const id = setInterval(() => void refresh(), 3000);
    return () => clearInterval(id);
  }, [refresh]);

  const act = async (p: Promise<{ ok: boolean; error?: string }>) => {
    const r = await p;
    setError(r.ok ? null : (r.error ?? 'That did not work.'));
    void refresh();
  };

  if (!st) {
    return (
      <div style={{ display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 980, margin: '0 auto' }}>
        <PageHeader title="File Search Index" subtitle="Fast, private lookup across the folders and shortcuts you choose." />
        <UnavailableState
          title="Preparing your private file index"
          description="Start Aqua Core to load indexed folders, scan progress, privacy exclusions, and search controls. Indexed content stays on this computer."
          status="File index offline"
        />
      </div>
    );
  }
  const { status: s, settings } = st;
  const state = stateText(s);
  const on = settings.enabled;

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 980, margin: '0 auto' }}>
      <PageHeader
        title="File Search Index"
        subtitle="Aqua keeps a list of the folders, programs and shortcuts on this computer, so “open the Personal AI folder” works at once. The list stays on this computer and is never sent anywhere."
        actions={
          <>
            {on && (
              <button className="btn-ghost" onClick={() => void act(fileIndexApi.pause(s.paused_reason !== 'paused by you'))}>
                {s.paused_reason === 'paused by you' ? 'Resume' : 'Pause'}
              </button>
            )}
            <button className="btn-primary" onClick={() => void act(fileIndexApi.settings({ enabled: !on }))}>
              {on ? 'Turn off' : 'Turn on'}
            </button>
          </>
        }
      />

      {error && (
        <div role="alert" className="glass-panel" style={{ padding: 14, color: 'var(--state-problem)' }}>
          {error}
        </div>
      )}
      {s.error && (
        <div role="alert" className="glass-panel" style={{ padding: 14, color: 'var(--state-problem)' }}>
          {s.error}
        </div>
      )}
      {s.rebuilt_after_damage && (
        <div className="glass-panel" style={{ padding: 14, color: 'var(--state-attention)' }}>
          The saved index was damaged, so Aqua moved it aside and is building a new one.
        </div>
      )}

      <Section title="Status" hint={on ? undefined : 'The index is off. Searches look through your main folders each time, which is slower.'}>
        <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap' }}>
          <Pill tone={state.tone} label={state.text} live={s.state === 'scanning'} />
          <Pill label="Folders" value={s.folders.toLocaleString()} />
          <Pill label="Programs and shortcuts" value={s.programs.toLocaleString()} />
          {s.index_files && <Pill label="Files" value={s.files.toLocaleString()} />}
          {s.queue > 0 && <Pill tone="attention" label="Still to look at" value={s.queue.toLocaleString()} />}
          <Pill label="Watching" value={`${s.watching} places`} title="Aqua is told when something in these places changes, so it does not have to keep looking." />
          {s.polled > 0 && <Pill tone="attention" label="Checked now and then" value={`${s.polled} places`} title="These places change too often to watch, so they are re-checked every few minutes." />}
          {s.volumes.some((v) => !v.online) && <Pill tone="attention" label="Drive not connected" value={s.volumes.filter((v) => !v.online).length} />}
        </div>
        <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap' }}>
          <Pill label="Effort so far" value={`${s.cpu_seconds.toFixed(1)} s of processor time`} title="Processor time used by the indexer since Aqua started." />
          <Pill label="Lookup" value={s.lookup_p50_ms || s.lookup_p95_ms ? `${s.lookup_p50_ms} ms typical, ${s.lookup_p95_ms} ms slowest` : 'none yet'} />
          <Pill label="Size on disk" value={size(s.db_bytes)} />
        </div>
      </Section>

      <Section title="What is included" hint="Folders, programs, installers and shortcuts are always included. Names that look like passwords or keys are not. Credentials, key files, browser profiles, caches and system folders never are.">
        <label style={{ display: 'flex', gap: 10, alignItems: 'flex-start', fontSize: 14 }}>
          <input type="checkbox" checked={settings.other_drives} onChange={(e) => void act(fileIndexApi.settings({ other_drives: e.target.checked }))} style={{ accentColor: 'var(--accent-cyan)', marginTop: 3 }} />
          <span>
            Other drives
            <span style={{ display: 'block', fontSize: 13, color: 'var(--text-secondary)' }}>Also look at the folders and programs on your other drives, such as games on D:. This is done last.</span>
          </span>
        </label>
        <label style={{ display: 'flex', gap: 10, alignItems: 'flex-start', fontSize: 14 }}>
          <input type="checkbox" checked={settings.index_documents} onChange={(e) => void act(fileIndexApi.settings({ index_documents: e.target.checked }))} style={{ accentColor: 'var(--accent-cyan)', marginTop: 3 }} />
          <span>
            Documents
            <span style={{ display: 'block', fontSize: 13, color: 'var(--text-secondary)' }}>
              PDF, Word, Excel, PowerPoint, text, e-books and similar, in your own folders and on your other drives, so “open Vishal CV.pdf” is found at once. Not in Program Files or Windows. File names can say a lot about you; turn this off if you would rather they were not listed.
            </span>
          </span>
        </label>
        <label style={{ display: 'flex', gap: 10, alignItems: 'flex-start', fontSize: 14 }}>
          <input type="checkbox" checked={settings.index_files} onChange={(e) => void act(fileIndexApi.settings({ index_files: e.target.checked }))} style={{ accentColor: 'var(--accent-cyan)', marginTop: 3 }} />
          <span>
            Every other kind of file
            <span style={{ display: 'block', fontSize: 13, color: 'var(--text-secondary)' }}>Photos, music, video, archives and anything else, in your main folders. Off unless you want it: it makes the list much bigger.</span>
          </span>
        </label>
        <FolderList
          title="Extra folders"
          hint="Folders to include that Aqua would not look in by itself. Only folders on this computer."
          items={settings.extra_roots}
          placeholder="D:\Games"
          onAdd={(p) => void act(fileIndexApi.settings({ extra_roots: [...settings.extra_roots, p] }))}
          onRemove={(p) => void act(fileIndexApi.settings({ extra_roots: settings.extra_roots.filter((x) => x !== p) }))}
        />
        <FolderList
          title="Never index"
          hint="Aqua forgets everything it knows below these folders and does not look there again."
          items={settings.exclude_paths}
          placeholder="D:\Private"
          onAdd={(p) => void act(fileIndexApi.settings({ exclude_paths: [...settings.exclude_paths, p] }))}
          onRemove={(p) => void act(fileIndexApi.settings({ exclude_paths: settings.exclude_paths.filter((x) => x !== p) }))}
        />
      </Section>

      <Section title="Start over" hint="Rebuilding looks at your computer again from the start. Forgetting deletes everything the index saved and turns it off.">
        <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap', alignItems: 'center' }}>
          <button className="btn-ghost" disabled={!on} onClick={() => void act(fileIndexApi.rebuild())}>
            Rebuild the index
          </button>
          {!confirmForget ? (
            <button className="btn-danger" onClick={() => setConfirmForget(true)}>
              Forget everything
            </button>
          ) : (
            <>
              <span style={{ fontSize: 13.5 }}>Delete the index and turn it off?</span>
              <button
                className="btn-danger solid"
                onClick={() => {
                  setConfirmForget(false);
                  void act(fileIndexApi.forget());
                }}
              >
                Yes, delete it
              </button>
              <button className="btn-ghost" onClick={() => setConfirmForget(false)}>
                Keep it
              </button>
            </>
          )}
        </div>
      </Section>
    </div>
  );
}
