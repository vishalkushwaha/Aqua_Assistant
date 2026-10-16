import { useCallback, useEffect, useRef, useState, type CSSProperties } from 'react';
import { learningApi } from '../api/learning';
import { errorSentence, factsText, goalText, WRITES_HINT, type LearnedRecipe, type RecipeList as Recipes } from '../api/recipes';

const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const writesMark: CSSProperties = { fontSize: 11, padding: '2px 8px', borderRadius: 999, border: '1px solid var(--state-attention)', color: 'var(--state-attention)', whiteSpace: 'nowrap', cursor: 'help' };
const small: CSSProperties = { padding: '3px 10px' };

/**
 * The recipes Aqua has learned: what each is for, how many steps it has, how often it was used, and whether a step changes
 * something. No step and no value is shown (the core does not send them). Forgetting one asks first, here in the panel.
 * Everything that comes from the core (a goal, an error) is a text child, never markup.
 */
export function RecipeList({ refreshKey = 0 }: { refreshKey?: number }) {
  const [data, setData] = useState<Recipes | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<number | null>(null);
  const [busy, setBusy] = useState<number | null>(null);
  // Only the newest answer is used: a slow list must not overwrite the list that was read after a recipe was forgotten.
  const latest = useRef(0);

  const load = useCallback(async () => {
    const mine = ++latest.current;
    try {
      const list = await learningApi.recipes();
      if (mine !== latest.current) return;
      setData(list);
      setError(null);
    } catch (e) {
      if (mine !== latest.current) return;
      setError(errorSentence(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load, refreshKey]);

  const forget = async (recipe: LearnedRecipe) => {
    setBusy(recipe.id);
    setError(null);
    setNotice(null);
    try {
      const { forgotten } = await learningApi.forgetRecipe(recipe.id);
      setNotice(forgotten ? 'Forgot that recipe.' : 'That recipe was already gone.');
      await load();
    } catch (e) {
      setError(errorSentence(e));
    } finally {
      setBusy(null);
      setConfirming(null);
    }
  };

  const now = Date.now() / 1000;
  return (
    <div className="glass-panel" style={{ padding: 24, borderRadius: 12, display: 'flex', flexDirection: 'column', gap: 12 }}>
      <div>
        <h3 style={{ margin: 0, fontSize: 16, color: 'var(--text-primary)', fontWeight: 600 }}>Learned recipes</h3>
        <p style={{ ...muted, marginTop: 4, maxWidth: '70ch', lineHeight: 1.5 }}>
          A recipe is a way of doing a task that went well, kept so that Aqua can do it again. This list shows what each one is for and how it has been used, never its steps or the values in them. A recipe marked "writes" has a step that changes something. Forgetting a recipe cannot be undone.
        </p>
      </div>
      {error && <div role="alert" style={{ color: 'var(--state-problem)', fontSize: 13 }}>{error}</div>}
      {notice && <div role="status" style={{ color: 'var(--state-ok)', fontSize: 13 }}>{notice}</div>}
      {data === null && !error && <div style={muted}>Loading…</div>}
      {data !== null && data.recipes.length === 0 && (
        <div style={muted}>Aqua has not learned any recipes yet. When a task goes well, Aqua can keep the way it did it, and it is listed here.</div>
      )}
      {data !== null && data.recipes.length > 0 && (
        <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
          {data.recipes.map((r, i) => (
            <li key={r.id} style={{ display: 'flex', gap: 12, alignItems: 'center', justifyContent: 'space-between', flexWrap: 'wrap', padding: '10px 0', borderTop: i === 0 ? 'none' : '1px solid var(--glass-border)' }}>
              <div style={{ flex: '1 1 280px', minWidth: 0 }}>
                <div style={{ fontSize: 13.5, fontWeight: 600, overflowWrap: 'anywhere' }}>{goalText(r.goal)}</div>
                <div style={muted}>{factsText(r, now)}</div>
              </div>
              <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
                {r.writes && <span style={writesMark} title={WRITES_HINT}>writes</span>}
                {confirming === r.id ? (
                  <>
                    <span style={{ fontSize: 13 }}>Forget this recipe?</span>
                    <button type="button" className="btn-danger" style={small} disabled={busy !== null} onClick={() => void forget(r)}>Yes, forget it</button>
                    <button type="button" className="btn-ghost" style={small} disabled={busy !== null} onClick={() => setConfirming(null)}>Keep</button>
                  </>
                ) : (
                  <button type="button" className="btn-ghost" style={small} disabled={busy !== null} aria-label={`Forget the recipe: ${goalText(r.goal)}`} onClick={() => { setNotice(null); setConfirming(r.id); }}>Forget</button>
                )}
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
