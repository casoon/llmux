// Typed client for the llmux read-only Stats API (#19).
//
// The dashboard is shipped as a static build and (once embedded into the binary, #20)
// served same-origin with the API, so requests default to relative `/api/stats/*`.
// For local development against a separately running llmux, override the base via
// `?api=http://host:port`, a persisted `localStorage["llmux_api_base"]`, or a
// `<meta name="llmux-api-base" content="...">` tag.

export type RouteResult =
  | 'allowed'
  | 'degraded'
  | 'cached'
  | 'fallback'
  | 'rejected'
  | 'forced';

export interface Overview {
  status: string;
  total_requests: number;
  requests_per_minute: number;
  cache_hit_rate: number;
  error_count: number;
  p95_latency_ms: number;
  cost_today: number;
  cost_month: number;
  budget_pressure: number;
  daily_max_usd: number | null;
  monthly_max_usd: number | null;
}

export interface RequestRow {
  id: number;
  time: string;
  tool: string | null;
  session: string | null;
  project: string | null;
  task_type: string | null;
  model: string | null;
  provider: string | null;
  tier: number;
  used_fallback: boolean;
  degraded: boolean;
  estimated_cost_usd: number;
  real_cost_usd: number;
  prompt_tokens: number;
  completion_tokens: number;
  latency_ms: number;
  status: number;
  cache_hit: boolean;
  forced: boolean;
  attempts: number;
  attempt_trail: string | null;
  stop_reason: string | null;
  error: string | null;
  result: RouteResult;
}

export interface ModelStat {
  provider: string;
  model: string;
  requests: number;
  real_cost_usd: number;
  avg_tier: number;
  success_rate: number;
  error_rate: number;
  fallback_rate: number;
  cache_hit_rate: number;
}

export interface PolicyStats {
  allowed: number;
  rejected: number;
  degraded: number;
  fallback: number;
  cached: number;
  forced: number;
  forced_rejected: number;
  local_only: number;
  top_rejection_reasons: { reason: string; count: number }[];
}

export interface ProjectStat {
  project: string;
  requests: number;
  real_cost_usd: number;
  rejected: number;
  forced: number;
  local_only: number;
}

export interface BudgetBucket {
  hour: string;
  cost: number;
  requests: number;
}

export interface BudgetSeries {
  buckets: BudgetBucket[];
  daily_max_usd: number | null;
  monthly_max_usd: number | null;
  spent_today: number;
}

/** Resolves the API base URL (see module header for the override precedence). */
export function apiBase(): string {
  if (typeof window === 'undefined') return '';
  const qp = new URLSearchParams(window.location.search).get('api');
  if (qp) {
    try {
      window.localStorage.setItem('llmux_api_base', qp);
    } catch {
      /* private mode */
    }
    return qp.replace(/\/$/, '');
  }
  try {
    const stored = window.localStorage.getItem('llmux_api_base');
    if (stored) return stored.replace(/\/$/, '');
  } catch {
    /* ignore */
  }
  const meta = document
    .querySelector('meta[name="llmux-api-base"]')
    ?.getAttribute('content');
  return (meta ?? '').replace(/\/$/, '');
}

async function getJson<T>(path: string, signal?: AbortSignal): Promise<T> {
  const res = await fetch(`${apiBase()}${path}`, {
    headers: { accept: 'application/json' },
    signal,
  });
  if (!res.ok) {
    throw new Error(`${path} → ${res.status} ${res.statusText}`);
  }
  return (await res.json()) as T;
}

export const api = {
  overview: (signal?: AbortSignal) => getJson<Overview>('/api/stats/overview', signal),
  requests: (limit = 100, signal?: AbortSignal) =>
    getJson<{ requests: RequestRow[] }>(`/api/stats/requests?limit=${limit}`, signal).then(
      (r) => r.requests,
    ),
  models: (signal?: AbortSignal) =>
    getJson<{ models: ModelStat[] }>('/api/stats/models', signal).then((r) => r.models),
  policy: (signal?: AbortSignal) => getJson<PolicyStats>('/api/stats/policy', signal),
  projects: (signal?: AbortSignal) =>
    getJson<{ projects: ProjectStat[] }>('/api/stats/projects', signal).then(
      (r) => r.projects,
    ),
  budgetSeries: (signal?: AbortSignal) =>
    getJson<BudgetSeries>('/api/stats/budget-series', signal),
};
