import { useCallback, useEffect, useRef, useState } from 'react';
import { approvalsApi, QUESTIONS_HIDDEN, type ApprovalAnswer, type ApprovalQuestion, type ApprovalScope } from '../api/approvals';
import { isKeyProblem } from '../api/plainErrors';
import { ApprovalCard } from './ApprovalCard';

const POLL_MS = 1500;
const HIDDEN_POLL_MS = 4000;
/** How long the sentence about the last answer stays on screen when no other question is waiting. */
const NOTICE_MS = 6000;

/** The ids of `ids` that are still in `list`, or the same set when none went. */
function stillThere(ids: Set<string>, list: ApprovalQuestion[]): Set<string> {
  if (ids.size === 0) return ids;
  const keep = new Set<string>();
  for (const q of list) if (ids.has(q.requestId)) keep.add(q.requestId);
  return keep.size === ids.size ? ids : keep;
}

/**
 * App-wide prompt for approvals. The agent pauses on unknown sites, irreversible actions (Send, Pay, Delete…), password fields and desktop or command
 * tools until you answer here; no answer means deny. The questions come from `GET /api/v1/browser/confirmations` (the whole truth, asked every
 * 1.5 s); the card (`ApprovalCard`) shows the action, what it can do, the rule and the time left, and the answer goes to `POST /api/v1/browser/confirm`.
 */
export function BrowserConfirmationPrompt() {
  const [pending, setPending] = useState<ApprovalQuestion[]>([]);
  const [now, setNow] = useState(Date.now());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<{ id: string; sentence: string } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // Questions the core says cannot be answered any more (card: only "Close"), and those the person closed.
  const [closed, setClosed] = useState<Set<string>>(() => new Set());
  const [dismissed, setDismissed] = useState<Set<string>>(() => new Set());
  const [hidden, setHidden] = useState(false);
  // Only the newest answer of the list is used: a slow one must not bring back a question that was answered since.
  const latest = useRef(0);

  const refresh = useCallback(async () => {
    const mine = ++latest.current;
    try {
      const list = await approvalsApi.list();
      if (mine !== latest.current) return;
      setPending(list);
      setHidden(false);
      setClosed((ids) => stillThere(ids, list));
      setDismissed((ids) => stillThere(ids, list));
    } catch (e) {
      // Core offline or restarting: keep the last state and retry on the next tick. A refusal about the key is different: nothing will ever
      // show here, and a question that needs a yes would run out unseen, so that is said.
      if (mine === latest.current && isKeyProblem(e)) setHidden(true);
    }
  }, []);

  useEffect(() => {
    void refresh();
    // While the window is hidden (it hides instead of closing) nobody is looking at it, and a waiting question brings it forward
    // anyway, so it is asked less often then.
    let poll = 0;
    const arm = () => {
      window.clearInterval(poll);
      poll = window.setInterval(() => void refresh(), document.hidden ? HIDDEN_POLL_MS : POLL_MS);
    };
    arm();
    document.addEventListener('visibilitychange', arm);
    return () => {
      window.clearInterval(poll);
      document.removeEventListener('visibilitychange', arm);
    };
  }, [refresh]);

  const shown = pending.filter((q) => !dismissed.has(q.requestId));
  const req = shown[0];
  const waitingId = req?.requestId;

  // The countdown only needs a clock while a question is on screen (a tick every second re-rendered this for nothing otherwise).
  const asking = shown.length > 0;
  useEffect(() => {
    if (!asking) return;
    setNow(Date.now());
    const tick = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(tick);
  }, [asking]);

  // The sentence about the last answer goes by itself.
  useEffect(() => {
    if (notice === null) return;
    const gone = window.setTimeout(() => setNotice(null), NOTICE_MS);
    return () => window.clearTimeout(gone);
  }, [notice]);

  // The desktop app hides its window instead of closing it, so a question that arrives while it is hidden would go
  // unseen and time out as "denied". Bring the window forward once per new request (a normal browser tab does nothing).
  useEffect(() => {
    if (!waitingId) return;
    const tauri = (window as Window & { __TAURI_INTERNALS__?: { invoke: (cmd: string) => Promise<unknown> } }).__TAURI_INTERNALS__;
    void tauri?.invoke('bring_to_front').catch(() => undefined);
  }, [waitingId]);

  if (!req) {
    if (hidden) {
      return <div role="alert" className="alert" data-tone="problem" style={{ position: 'fixed', left: 16, right: 16, bottom: 16, zIndex: 1000 }}>{QUESTIONS_HIDDEN}</div>;
    }
    if (notice !== null) {
      return <div role="status" className="alert" data-tone="ok" style={{ position: 'fixed', left: 16, right: 16, bottom: 16, zIndex: 1000, maxWidth: 520, margin: '0 auto' }}>{notice}</div>;
    }
    return null;
  }

  const answer = async (decision: ApprovalAnswer, scope?: ApprovalScope) => {
    setBusy(true);
    setError(null);
    // Only the run option (card OA6-05) carries a scope; every other answer is sent exactly as it always was.
    const result = scope === 'run' ? await approvalsApi.answer(req, 'approve', 'run') : await approvalsApi.answer(req, decision);
    setBusy(false);
    if (result.ok) {
      // Answered: the card goes at once (a second click would only be told "answered already"); the list drops it on its next reading.
      setDismissed((ids) => new Set(ids).add(req.requestId));
      setNotice(result.sentence);
    } else {
      setError({ id: req.requestId, sentence: result.sentence });
      if (result.closed) setClosed((ids) => new Set(ids).add(req.requestId));
    }
    void refresh();
  };

  const close = () => {
    setDismissed((ids) => new Set(ids).add(req.requestId));
    setError(null);
    void refresh();
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="browser-confirm-title"
      style={{
        position: 'fixed', inset: 0, zIndex: 1000, display: 'flex', alignItems: 'center', justifyContent: 'center',
        background: 'rgba(3, 7, 18, 0.62)', padding: 16,
      }}
    >
      <ApprovalCard
        key={req.requestId}
        question={req}
        nowMs={now}
        queued={shown.length - 1}
        busy={busy}
        error={error !== null && error.id === req.requestId ? error.sentence : null}
        closed={closed.has(req.requestId)}
        notice={notice}
        onAnswer={(decision, scope) => void answer(decision, scope)}
        onClose={close}
      />
    </div>
  );
}
