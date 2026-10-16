// Typed client for aqua-core's Agent Learning system (/api/v1/learning/*). The dev proxy adds the
// tokens. Reading needs any token; every change (settings, approving, deprecating, deleting,
// revalidating, reindexing, clearing) needs the UI token, which the proxy sends.

import { mapRecipeList, recipeErrorSentence, RecipeError, serverSentence, UNREADABLE_ANSWER, type RecipeList } from './recipes.ts';

export interface LearningSettings {
  learning_enabled: boolean;
  retrieval_enabled: boolean;
  auto_memory_creation: boolean;
  autonomous_experimentation: boolean;
  similarity_threshold: number;
  memory_confidence_threshold: number;
  retrieval_candidates: number;
  max_experiences_in_prompt: number;
  max_attempts: number;
  max_execution_secs: number;
  token_budget: number;
  cost_budget_usd: number;
  max_tool_calls: number;
  max_background_jobs: number;
  require_human_approval: boolean;
  stale_after_days: number;
  auto_practice_enabled: boolean;
  practice_max_per_day: number;
  practice_min_interval_mins: number;
  practice_idle_mins: number;
  practice_max_steps: number;
}

export interface LearningStatus {
  learning_enabled: boolean;
  retrieval_enabled: boolean;
  auto_memory_creation: boolean;
  autonomous_experimentation: boolean;
  current_activity: string;
  jobs_queued: number;
  jobs_running: number;
  jobs_dead: number;
  last_learning_at: number;
  last_success_at: number;
  worker_running: boolean;
  vector_store: { ok: boolean; provider: string; indexed: number; needs_reindex: number; error: string | null };
  embedding: { ok: boolean; model: string; version: number; dimensions: number; error: string | null };
}

export interface LearningStats {
  tasks_analyzed: number;
  tasks_successful: number;
  tasks_failed: number;
  experiences_created: number;
  experiences_updated: number;
  experiences_rejected: number;
  experiences_deprecated: number;
  experiences_reused: number;
  successful_reuses: number;
  failed_reuses: number;
  retrievals: number;
  retrieval_hits: number;
  retrieval_hit_rate: number;
  average_similarity: number;
  average_quality: number;
  duplicates_prevented: number;
  total_experiences: number;
  task_success_rate: number;
  reuse_success_rate: number;
  practice_runs: number;
  practice_verified: number;
  by_status: Record<string, number>;
  jobs: Record<string, number>;
  last_learning_at: number;
  last_success_at: number;
}

export interface Evaluation {
  runs_with_memory: number;
  runs_without_memory: number;
  success_rate_with_memory: number;
  success_rate_without_memory: number;
  avg_execution_ms_with_memory: number;
  avg_execution_ms_without_memory: number;
  avg_tool_calls_with_memory: number;
  avg_tool_calls_without_memory: number;
  avg_tokens_with_memory: number;
  avg_tokens_without_memory: number;
  memory_related_failures: number;
  retrieval_hit_rate: number;
  reuse_success_rate: number;
  note: string;
}

export type ExperienceStatus = 'CANDIDATE' | 'VERIFIED' | 'TRUSTED' | 'FREQUENTLY_REUSED' | 'FAILED' | 'STALE' | 'DEPRECATED' | 'INVALID';
export type ExperienceType = 'SOLUTION' | 'PROCEDURE' | 'PATTERN' | 'ERROR_FIX' | 'PROJECT_KNOWLEDGE' | 'CONSTRAINT' | 'BEST_PRACTICE';

export const STATUSES: ExperienceStatus[] = ['CANDIDATE', 'VERIFIED', 'TRUSTED', 'FREQUENTLY_REUSED', 'FAILED', 'STALE', 'DEPRECATED', 'INVALID'];
export const TYPES: ExperienceType[] = ['SOLUTION', 'PROCEDURE', 'PATTERN', 'ERROR_FIX', 'PROJECT_KNOWLEDGE', 'CONSTRAINT', 'BEST_PRACTICE'];

export interface ExperienceSummary {
  id: number;
  problem_summary: string;
  experience_type: ExperienceType;
  approach_summary: string;
  status: ExperienceStatus;
  confidence: number;
  quality_score: number;
  verified: boolean;
  verification_method: string;
  created_at: number;
  last_verified_at: number;
  reuse_count: number;
  success_rate: number;
  scope: string;
  project_key: string;
  human_approved: boolean;
}

export interface ExperienceDetail {
  experience: {
    id: number;
    problem_summary: string;
    context_summary: string;
    approach: string;
    important_steps: string[];
    tools_used: string[];
    constraints: string[];
    technologies: string[];
    solution_summary: string;
    verification_method: string;
    verification_result: string;
    verified: boolean;
    failed_approach: string | null;
    failure_reason: string | null;
    correction: string | null;
    confidence: number;
    quality_score: number;
    status: ExperienceStatus;
    human_approved: boolean;
    embedding_model: string;
    embedding_version: number;
    created_at: number;
    updated_at: number;
    last_verified_at: number;
    last_used_at: number;
    retrieval_count: number;
    reuse_count: number;
    successful_reuse_count: number;
    failed_reuse_count: number;
    experience_version: number;
    scope: string;
    project_key: string;
    experience_type: ExperienceType;
    task_type: string;
  };
  usage: { id: number; used_at: number; task_summary: string; similarity: number; success: boolean | null; note: string }[];
  why_trusted: string[];
}

export interface PracticeTask {
  id: number;
  goal: string;
  family: string;
  source: 'variation' | 'paraphrase' | string;
  status: string;
  created_at: number;
  finished_at: number;
  verified: boolean;
  note: string;
  steps: number;
}

export interface PracticeStatus {
  enabled: boolean;
  max_per_day: number;
  started_last_24h: number;
  min_interval_mins: number;
  idle_mins: number;
  next_allowed_at: number;
  waiting_for: string;
  recent: PracticeTask[];
  last_request: string;
}

export interface LearningEvent {
  id: number;
  ts: number;
  event: string;
  detail: unknown;
  job_id: number | null;
}

export interface LearningJob {
  id: number;
  kind: string;
  status: string;
  attempts: number;
  max_attempts: number;
  last_error: string;
  created_at: number;
  updated_at: number;
}

async function json<T>(r: Response): Promise<T> {
  if (!r.ok) {
    let detail = '';
    try {
      const body = await r.json();
      detail = body?.error ? `: ${body.error}` : '';
    } catch {
      /* not JSON */
    }
    throw new Error(`${r.status} ${r.statusText}${detail}`);
  }
  return (await r.json()) as T;
}

const post = (url: string, body?: unknown) =>
  fetch(url, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });

export interface ListFilters {
  status?: string;
  type?: string;
  q?: string;
  limit?: number;
}

/** How Aqua starts an app it has started before, and how sure it is of that way. */
export interface LaunchSkill {
  /** The spoken name, normalised ("notepad"). */
  key: string;
  name: string;
  recipe: { kind: 'exe' | 'path' | 'apps_folder' | 'uri'; target: string };
  /** Where the recipe came from: well-known, file index or start menu. */
  source: string;
  confidence: number;
  successes: number;
  failures: number;
  fail_streak: number;
  unverified: number;
  avg_ms: number;
  last_ms: number;
  learned_unix: number;
  last_unix: number;
}

export interface LaunchSkills {
  learning_enabled: boolean;
  used_for_new_requests: boolean;
  skills: LaunchSkill[];
  totals: { skills: number; successes: number; failures: number };
}

/** A call about recipes. A refused connection becomes a sentence, like every other failure of these two calls. */
async function recipeRequest(action: 'list' | 'forget', url: string, init?: RequestInit): Promise<Response> {
  try {
    return await fetch(url, init);
  } catch {
    throw new RecipeError(recipeErrorSentence(action, null));
  }
}

/** The error for a not-ok answer: a sentence, never the status line or the body as it came. */
async function recipeFailure(action: 'list' | 'forget', r: Response): Promise<RecipeError> {
  let body: unknown = null;
  try {
    body = await r.json();
  } catch {
    /* not JSON */
  }
  return new RecipeError(recipeErrorSentence(action, r.status, serverSentence(body)));
}

export const learningApi = {
  /** The recipes Aqua has learned, most recent first (UI token). No step and no value is in the answer. */
  recipes: async (): Promise<RecipeList> => {
    const r = await recipeRequest('list', '/api/v1/learning/recipes');
    if (!r.ok) throw await recipeFailure('list', r);
    let body: unknown = null;
    try {
      body = await r.json();
    } catch {
      /* said below */
    }
    const list = mapRecipeList(body);
    if (!list) throw new RecipeError(UNREADABLE_ANSWER);
    return list;
  },
  /** Forgets one recipe (UI token). `forgotten` is false when there was no recipe with that id: it is gone already. */
  forgetRecipe: async (id: number): Promise<{ forgotten: boolean }> => {
    if (!Number.isSafeInteger(id)) throw new RecipeError(recipeErrorSentence('forget', 400));
    const r = await recipeRequest('forget', `/api/v1/learning/recipes/${id}`, { method: 'DELETE' });
    if (r.status === 404) return { forgotten: false };
    if (!r.ok) throw await recipeFailure('forget', r);
    return { forgotten: true };
  },
  launchSkills: () => fetch('/api/v1/learning/launch-skills').then((r) => json<LaunchSkills>(r)),
  forgetLaunchSkill: (key: string) => post('/api/v1/learning/launch-skills/forget', { key }).then((r) => json<{ forgotten: number }>(r)),
  forgetAllLaunchSkills: () => post('/api/v1/learning/launch-skills/forget', { all: true }).then((r) => json<{ forgotten: number }>(r)),
  settings: () => fetch('/api/v1/learning/settings').then((r) => json<LearningSettings>(r)),
  saveSettings: (s: LearningSettings) =>
    fetch('/api/v1/learning/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(s) }).then((r) => json<LearningSettings>(r)),
  status: () => fetch('/api/v1/learning/status').then((r) => json<LearningStatus>(r)),
  stats: () => fetch('/api/v1/learning/stats').then((r) => json<LearningStats>(r)),
  evaluation: () => fetch('/api/v1/learning/evaluation').then((r) => json<Evaluation>(r)),
  events: (limit = 40) => fetch(`/api/v1/learning/events?limit=${limit}`).then((r) => json<{ events: LearningEvent[] }>(r)),
  practice: () => fetch('/api/v1/learning/practice').then((r) => json<PracticeStatus>(r)),
  practiceRunNow: () => post('/api/v1/learning/practice/run-now').then((r) => json<{ requested: boolean }>(r)),
  jobs: (limit = 20) => fetch(`/api/v1/learning/jobs?limit=${limit}`).then((r) => json<{ jobs: LearningJob[] }>(r)),
  list: (f: ListFilters = {}) => {
    const p = new URLSearchParams();
    if (f.status) p.set('status', f.status);
    if (f.type) p.set('type', f.type);
    if (f.q) p.set('q', f.q);
    p.set('limit', String(f.limit ?? 100));
    return fetch(`/api/v1/learning/experiences?${p}`).then((r) => json<{ experiences: ExperienceSummary[] }>(r));
  },
  get: (id: number) => fetch(`/api/v1/learning/experiences/${id}`).then((r) => json<ExperienceDetail>(r)),
  setStatus: (id: number, action: 'approve' | 'trust' | 'reject' | 'deprecate') =>
    post(`/api/v1/learning/experiences/${id}/status`, { action }).then((r) => json<ExperienceSummary>(r)),
  revalidate: (id: number) => post(`/api/v1/learning/experiences/${id}/revalidate`, {}).then((r) => json<{ valid: boolean; status: string; reason: string }>(r)),
  remove: (id: number) => post(`/api/v1/learning/experiences/${id}/delete`).then((r) => json<{ deleted: boolean }>(r)),
  reindex: () => post('/api/v1/learning/reindex').then((r) => json<{ total: number; reindexed: number; failed: number }>(r)),
  consolidate: () => post('/api/v1/learning/consolidate').then((r) => json<{ merged: number; marked_stale: number; removed: number }>(r)),
  clear: () => post('/api/v1/learning/clear', { confirm: 'delete all experiences' }).then((r) => json<{ deleted: number }>(r)),
};
