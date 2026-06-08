// Alpine component backing the dashboard (#19). Replaces the build-time mock data with
// live Stats API calls, and owns UI state: loading/error/empty/stale, search + status +
// time-window filters, auto-refresh, and the request inspector drawer.

import {
  api,
  type BudgetSeries,
  type ModelStat,
  type Overview,
  type PolicyStats,
  type ProjectStat,
  type RequestRow,
  type RouteResult,
} from './api';

type Loaded = 'idle' | 'loading' | 'ready' | 'error';
type TimeWindow = '1h' | '24h' | 'all';

interface AttemptStep {
  provider?: string;
  model?: string;
  key?: number;
  status?: number;
  error?: string;
}

const REFRESH_MS = 10_000;
const WINDOW_MS: Record<TimeWindow, number> = {
  '1h': 3_600_000,
  '24h': 86_400_000,
  all: Number.POSITIVE_INFINITY,
};

export function dashboard() {
  return {
    state: 'loading' as Loaded,
    error: '' as string,
    stale: false,
    lastUpdated: null as Date | null,

    overview: null as Overview | null,
    requests: [] as RequestRow[],
    models: [] as ModelStat[],
    policy: null as PolicyStats | null,
    projects: [] as ProjectStat[],
    budget: null as BudgetSeries | null,

    search: '',
    resultFilter: 'all' as RouteResult | 'all',
    window: '24h' as TimeWindow,

    inspectorOpen: false,
    selected: null as RequestRow | null,

    _timer: 0 as number,

    init() {
      void this.load();
      this._timer = window.setInterval(() => void this.load(true), REFRESH_MS);
      document.addEventListener('keydown', (e) => {
        if (e.key === 'Escape' && this.inspectorOpen) this.closeInspector();
      });
    },

    destroy() {
      window.clearInterval(this._timer);
    },

    async load(background = false) {
      if (!background) this.state = 'loading';
      const results = await Promise.allSettled([
        api.overview(),
        api.requests(200),
        api.models(),
        api.policy(),
        api.projects(),
        api.budgetSeries(),
      ]);

      const [ov, rq, md, pol, pr, bs] = results;
      const anyOk = results.some((r) => r.status === 'fulfilled');

      if (!anyOk) {
        // Total failure: surface the error, but keep prior data on a background refresh.
        this.error = firstReason(results) ?? 'Stats API unreachable';
        if (background) {
          this.stale = true;
        } else {
          this.state = 'error';
        }
        return;
      }

      if (ov.status === 'fulfilled') this.overview = ov.value;
      if (rq.status === 'fulfilled') this.requests = rq.value;
      if (md.status === 'fulfilled') this.models = md.value;
      if (pol.status === 'fulfilled') this.policy = pol.value;
      if (pr.status === 'fulfilled') this.projects = pr.value;
      if (bs.status === 'fulfilled') this.budget = bs.value;

      this.error = '';
      this.stale = results.some((r) => r.status === 'rejected');
      this.state = 'ready';
      this.lastUpdated = new Date();
    },

    // ---- derived ---------------------------------------------------------

    get online(): boolean {
      return this.state === 'ready' && !this.stale;
    },

    get filteredRequests(): RequestRow[] {
      const q = this.search.trim().toLowerCase();
      const cutoff = WINDOW_MS[this.window];
      const now = Date.now();
      return this.requests.filter((r) => {
        if (this.resultFilter !== 'all' && r.result !== this.resultFilter) return false;
        if (cutoff !== Number.POSITIVE_INFINITY) {
          const t = Date.parse(r.time);
          if (!Number.isNaN(t) && now - t > cutoff) return false;
        }
        if (!q) return true;
        return [r.tool, r.project, r.model, r.task_type, r.provider]
          .filter(Boolean)
          .some((v) => String(v).toLowerCase().includes(q));
      });
    },

    get feedEmpty(): boolean {
      return this.state === 'ready' && this.filteredRequests.length === 0;
    },

    get budgetUsedPct(): number {
      return Math.round((this.overview?.budget_pressure ?? 0) * 100);
    },

    get budgetMax(): number {
      const costs = this.budget?.buckets.map((b) => b.cost) ?? [];
      return Math.max(0.000001, ...costs);
    },

    get resultFilters(): (RouteResult | 'all')[] {
      return ['all', 'allowed', 'cached', 'degraded', 'fallback', 'forced', 'rejected'];
    },

    get policySignals(): { label: string; value: number; tone: string }[] {
      const p = this.policy;
      if (!p) return [];
      return [
        { label: 'Allowed', value: p.allowed, tone: 'text-success' },
        { label: 'Cached', value: p.cached, tone: 'text-accent' },
        { label: 'Degraded', value: p.degraded, tone: 'text-warning' },
        { label: 'Fallback', value: p.fallback, tone: 'text-info' },
        { label: 'Forced', value: p.forced, tone: 'text-muted' },
        { label: 'Rejected', value: p.rejected, tone: 'text-error' },
        { label: 'Local only', value: p.local_only, tone: 'text-success' },
      ];
    },

    // ---- inspector -------------------------------------------------------

    openInspector(row: RequestRow) {
      this.selected = row;
      this.inspectorOpen = true;
    },

    closeInspector() {
      this.inspectorOpen = false;
    },

    get attemptChain(): AttemptStep[] {
      if (!this.selected?.attempt_trail) return [];
      try {
        const parsed = JSON.parse(this.selected.attempt_trail) as AttemptStep[];
        return Array.isArray(parsed) ? parsed : [];
      } catch {
        return [];
      }
    },

    // ---- formatting ------------------------------------------------------

    fmtCost(v: number | null | undefined): string {
      const n = v ?? 0;
      return n === 0 ? '$0' : `$${n.toFixed(n < 0.01 ? 4 : 2)}`;
    },

    fmtLatency(ms: number | null | undefined): string {
      const n = ms ?? 0;
      return n >= 1000 ? `${(n / 1000).toFixed(2)}s` : `${n}ms`;
    },

    fmtPct(v: number | null | undefined): string {
      return `${Math.round((v ?? 0) * 100)}%`;
    },

    fmtTime(iso: string | null | undefined): string {
      if (!iso) return '–';
      const d = new Date(iso);
      return Number.isNaN(d.getTime())
        ? String(iso)
        : d.toLocaleTimeString(undefined, { hour12: false });
    },

    localPct(p: ProjectStat): string {
      return p.requests > 0 ? `${Math.round((p.local_only / p.requests) * 100)}%` : '0%';
    },

    resultClass(result: RouteResult): string {
      const map: Record<RouteResult, string> = {
        allowed: 'text-success bg-success/8',
        degraded: 'text-warning bg-warning/8',
        cached: 'text-accent bg-accent/8',
        fallback: 'text-info bg-info/8',
        rejected: 'text-error bg-error/8',
        forced: 'text-muted bg-surface-2',
      };
      return map[result] ?? 'text-muted bg-surface-2';
    },
  };
}

function firstReason(results: PromiseSettledResult<unknown>[]): string | null {
  for (const r of results) {
    if (r.status === 'rejected') return String(r.reason?.message ?? r.reason);
  }
  return null;
}

export type DashboardComponent = ReturnType<typeof dashboard>;
