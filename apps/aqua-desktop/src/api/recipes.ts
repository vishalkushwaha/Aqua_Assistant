// The recipes Aqua has learned, as the Agent Learning page shows them (GET /api/v1/learning/recipes; forgetting one is
// DELETE /api/v1/learning/recipes/{id}; both need the UI token, which the shell or the dev proxy adds).
//
// A recipe is a way of doing a task that went well. The list shows what it is for and how it was used, never what it does: the
// core sends no step, argument or value (core/src/api_gateway/recipes_api.rs). Everything here is a plain function, so a test
// can call it without a page. Text from the core (a goal, an error) is only ever handed to React as a text child.

/** One recipe as the page uses it. */
export interface LearnedRecipe {
  id: number;
  /** The goal as stored: personal details removed, and values the recipe takes from a goal shown as `{{file1}}` and the like. */
  goal: string;
  steps: number;
  /** 1 for a recipe made before versions, 2 for one with placeholders and checks; 0 when it cannot be read. */
  version: number;
  timesUsed: number;
  /** Seconds since 1970 of the last use (or of learning it), or null when the core does not say. */
  lastUsed: number | null;
  /** 0 to 1, or null when the core does not say. */
  successRate: number | null;
  /** Whether any step changes something (writes, clicks, types, opens). */
  writes: boolean;
  /** Learned in a run that had read text from outside. */
  tainted: boolean;
  params: number;
  /** False for a stored recipe that cannot be read any more: it is listed so that it can be forgotten. */
  readable: boolean;
}

export interface RecipeList {
  recipes: LearnedRecipe[];
  /** Whether the core makes and replays the newer recipes (AQUA_RECIPES_V2). */
  recipesV2: boolean;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function count(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? Math.floor(value) : 0;
}

/**
 * One row of the list, or null when it is not a recipe the page could forget (no whole-number id). A field that is missing
 * becomes the plain default (no goal, zero steps, zero uses, no time), with one exception: a readable recipe that does not say
 * whether a step changes something is shown as one that does, as the core does when it is unsure. A recipe that cannot be read
 * has no steps to change anything.
 */
export function mapRecipe(raw: unknown): LearnedRecipe | null {
  if (!isObject(raw)) return null;
  const id = raw.id;
  if (typeof id !== 'number' || !Number.isSafeInteger(id)) return null;
  const readable = raw.readable !== false;
  const lastUsed = typeof raw.last_used === 'number' && Number.isFinite(raw.last_used) && raw.last_used > 0 ? raw.last_used : null;
  const rate = raw.success_rate;
  return {
    id,
    goal: typeof raw.goal === 'string' ? raw.goal : '',
    steps: count(raw.steps),
    version: count(raw.version),
    timesUsed: count(raw.times_used),
    lastUsed,
    successRate: typeof rate === 'number' && Number.isFinite(rate) ? Math.min(1, Math.max(0, rate)) : null,
    writes: readable && raw.holds_writes !== false,
    tainted: raw.tainted === true,
    params: count(raw.params),
    readable,
  };
}

/** Most recently used (or learned) first; recipes with no time last; the newest id first among equals. */
export function recentFirst(recipes: LearnedRecipe[]): LearnedRecipe[] {
  return [...recipes].sort((a, b) => (b.lastUsed ?? 0) - (a.lastUsed ?? 0) || b.id - a.id);
}

/** The answer of the list route as the page uses it, most recent first, or null when it is not a list at all. */
export function mapRecipeList(body: unknown): RecipeList | null {
  if (!isObject(body) || !Array.isArray(body.recipes)) return null;
  const recipes: LearnedRecipe[] = [];
  for (const row of body.recipes) {
    const recipe = mapRecipe(row);
    if (recipe) recipes.push(recipe);
  }
  return { recipes: recentFirst(recipes), recipesV2: body.recipes_v2 === true };
}

const MAX_GOAL_CHARS = 160;

/** The goal for the list: as stored, cut when very long, and a stand-in when the core sent none. */
export function goalText(goal: string): string {
  const text = goal.trim();
  if (text === '') return '(no goal recorded)';
  const letters = Array.from(text);
  return letters.length > MAX_GOAL_CHARS ? `${letters.slice(0, MAX_GOAL_CHARS - 1).join('')}…` : text;
}

export function stepsText(steps: number): string {
  return steps === 1 ? '1 step' : `${steps} steps`;
}

export function usesText(timesUsed: number, successRate: number | null): string {
  if (timesUsed <= 0) return 'not used yet';
  const times = timesUsed === 1 ? 'used once' : `used ${timesUsed} times`;
  return successRate === null ? times : `${times}, worked ${Math.round(successRate * 100)}%`;
}

const localDate = (unix: number) => new Date(unix * 1000).toLocaleDateString();

/** "just now", "5 min ago", "3 h ago", and from two days on the date. `nowSecs` is given so the answer does not depend on the clock. */
export function agoText(unix: number, nowSecs: number, formatDate: (unix: number) => string = localDate): string {
  const s = Math.max(0, nowSecs - unix);
  if (s < 90) return 'just now';
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  if (s < 86_400 * 2) return `${Math.round(s / 3600)} h ago`;
  return formatDate(unix);
}

/** The one line under a goal: steps, version, uses and when. A recipe that cannot be read says so instead. */
export function factsText(recipe: LearnedRecipe, nowSecs: number, formatDate?: (unix: number) => string): string {
  if (!recipe.readable) return 'Could not be read (an old or damaged recipe). It can still be forgotten.';
  const parts = [stepsText(recipe.steps), `version ${recipe.version}`, usesText(recipe.timesUsed, recipe.successRate)];
  if (recipe.lastUsed !== null) parts.push(`${recipe.timesUsed > 0 ? 'last used' : 'learned'} ${agoText(recipe.lastUsed, nowSecs, formatDate)}`);
  return parts.join(' · ');
}

/** What the "writes" mark says when the pointer rests on it. */
export const WRITES_HINT = 'At least one step of this recipe changes something: it writes a file, clicks, types or opens something.';

/** An error whose message is already a sentence for the person reading it: the page shows `message` and nothing else. */
export class RecipeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'RecipeError';
  }
}

export const UNREADABLE_ANSWER = 'Aqua sent an answer the app could not read.';
const NOT_ASKED = 'That did not work.';

/** The core's own sentence from an error body (`{"error": "..."}`), or '' when there is none or it is not a short sentence. */
export function serverSentence(body: unknown): string {
  if (!isObject(body) || typeof body.error !== 'string') return '';
  const text = body.error.trim();
  return text !== '' && text.length <= 200 ? text : '';
}

function tidy(sentence: string): string {
  const first = sentence.charAt(0).toUpperCase() + sentence.slice(1);
  return /[.!?]$/.test(first) ? first : `${first}.`;
}

/**
 * The sentence for a failed call. `status` is the HTTP status, or null when no answer came at all. A person is not shown a status
 * line or a stack: the known cases have a sentence of their own, the others use the core's sentence when it sent one.
 */
export function recipeErrorSentence(action: 'list' | 'forget', status: number | null, serverMessage = ''): string {
  if (status === null) return 'Could not reach Aqua. Check that it is running, then try again.';
  if (status === 401) return "Aqua did not accept the app's key. Restart Aqua and try again.";
  if (status === 403) return 'Only the Aqua app can do this. Open it from the Aqua window, not from a script or another program.';
  if (status === 404 && action === 'list') return 'This version of Aqua cannot list learned recipes.';
  if (status === 503) return 'Aqua is busy or still starting. Try again in a moment.';
  if (serverMessage !== '') return tidy(serverMessage);
  return action === 'list' ? 'The learned recipes could not be read.' : 'The recipe could not be forgotten.';
}

/** What to show for anything that was thrown: a sentence we wrote, or a plain one for a failure nobody planned for. */
export function errorSentence(error: unknown): string {
  return error instanceof RecipeError ? error.message : NOT_ASKED;
}
