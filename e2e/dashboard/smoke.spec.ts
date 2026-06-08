import { expect, test, type Page } from '@playwright/test';

// Fixture payloads matching the Stats API shapes (src/logging.rs). The dashboard
// fetches these same-origin; we intercept and serve fixtures so the smoke test needs
// no running llmux backend.
const fixtures: Record<string, unknown> = {
  '/api/stats/overview': {
    status: 'online',
    total_requests: 128,
    requests_per_minute: 4.2,
    cache_hit_rate: 0.31,
    error_count: 2,
    p95_latency_ms: 1840,
    cost_today: 1.27,
    cost_month: 8.4,
    budget_pressure: 0.64,
    daily_max_usd: 2.0,
    monthly_max_usd: 50.0,
  },
  '/api/stats/requests': {
    requests: [
      {
        id: 2,
        time: new Date().toISOString(),
        tool: 'codex',
        session: 'sess-1',
        project: 'llmux',
        task_type: 'code_review',
        model: 'gpt-4.1-mini',
        provider: 'openai',
        tier: 3,
        used_fallback: false,
        degraded: false,
        estimated_cost_usd: 0.002,
        real_cost_usd: 0.0021,
        prompt_tokens: 900,
        completion_tokens: 120,
        latency_ms: 1300,
        status: 200,
        cache_hit: false,
        forced: false,
        attempts: 1,
        attempt_trail: JSON.stringify([
          { provider: 'openai', model: 'gpt-4.1-mini', key: 0, status: 200 },
        ]),
        stop_reason: 'stop',
        error: null,
        result: 'allowed',
      },
      {
        id: 1,
        time: new Date().toISOString(),
        tool: 'aider',
        session: null,
        project: 'client-api',
        task_type: 'private_sensitive',
        model: 'qwen2.5',
        provider: 'ollama',
        tier: 1,
        used_fallback: false,
        degraded: false,
        estimated_cost_usd: 0,
        real_cost_usd: 0,
        prompt_tokens: 50,
        completion_tokens: 30,
        latency_ms: 800,
        status: 200,
        cache_hit: false,
        forced: true,
        attempts: 1,
        attempt_trail: JSON.stringify([
          { provider: 'ollama', model: 'qwen2.5', key: 0, status: 200 },
        ]),
        stop_reason: 'stop',
        error: null,
        result: 'forced',
      },
    ],
  },
  '/api/stats/models': {
    models: [
      {
        provider: 'openai',
        model: 'gpt-4.1-mini',
        requests: 64,
        real_cost_usd: 0.42,
        avg_tier: 3.0,
        success_rate: 0.98,
        error_rate: 0.02,
        fallback_rate: 0.03,
        cache_hit_rate: 0.12,
      },
    ],
  },
  '/api/stats/policy': {
    allowed: 110,
    rejected: 4,
    degraded: 6,
    fallback: 3,
    cached: 40,
    forced: 9,
    forced_rejected: 1,
    local_only: 22,
    top_rejection_reasons: [{ reason: 'budget exceeded', count: 2 }],
  },
  '/api/stats/projects': {
    projects: [
      {
        project: 'llmux',
        requests: 84,
        real_cost_usd: 0.42,
        rejected: 1,
        forced: 2,
        local_only: 20,
      },
    ],
  },
  '/api/stats/budget-series': {
    buckets: Array.from({ length: 24 }, (_, i) => ({
      hour: `2026-06-08T${String(i).padStart(2, '0')}:00`,
      cost: i === 23 ? 0.21 : i % 4 === 0 ? 0.08 : 0,
      requests: i === 23 ? 12 : 1,
    })),
    daily_max_usd: 2.0,
    monthly_max_usd: 50.0,
    spent_today: 1.27,
  },
};

async function mockStats(page: Page) {
  await page.route('**/api/stats/**', async (route) => {
    const url = new URL(route.request().url());
    const body = fixtures[url.pathname];
    if (body) {
      await route.fulfill({ json: body });
    } else {
      await route.fulfill({ status: 404, json: { error: 'not found' } });
    }
  });
}

test.describe('dashboard smoke', () => {
  test('renders panels, opens inspector, and logs no console errors', async ({ page }) => {
    const consoleErrors: string[] = [];
    page.on('console', (msg) => {
      if (msg.type() === 'error' && !/favicon|Failed to load resource/.test(msg.text())) {
        consoleErrors.push(msg.text());
      }
    });
    page.on('pageerror', (err) => consoleErrors.push(err.message));

    await mockStats(page);
    await page.goto('/');

    // Title + main heading.
    await expect(page).toHaveTitle(/llmux Control Room/);
    await expect(page.locator('h1')).toHaveText('llmux Control Room');

    // Status chip reflects the live overview status.
    await expect(page.getByRole('status')).toContainText('online');

    // KPI strip hydrated from the overview fixture (requests/min = 4.2).
    await expect(page.getByText('4.2', { exact: true })).toBeVisible();

    // Route feed rows rendered from the requests fixture.
    const rows = page.locator('tbody [role="button"][aria-label^="Inspect request"]');
    await expect(rows).toHaveCount(2);
    await expect(page.getByText('gpt-4.1-mini').first()).toBeVisible();

    // Panels rendered.
    await expect(page.getByRole('heading', { name: 'Policy radar' })).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Model matrix' })).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Projects' })).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Budget pressure' })).toBeVisible();

    // Inspector opens on row click and shows real route detail.
    await rows.first().click();
    const dialog = page.getByRole('dialog');
    await expect(dialog).toBeVisible();
    await expect(dialog.getByText('Provider attempts')).toBeVisible();
    await expect(dialog.getByText('gpt-4.1-mini · openai')).toBeVisible();

    // Inspector closes on Escape.
    await page.keyboard.press('Escape');
    await expect(dialog).toBeHidden();

    expect(consoleErrors, `console errors: ${consoleErrors.join('\n')}`).toEqual([]);
  });

  test('shows an error state when the Stats API is unreachable', async ({ page }) => {
    await page.route('**/api/stats/**', (route) => route.fulfill({ status: 500, json: {} }));
    await page.goto('/');
    await expect(page.getByRole('heading', { name: 'Stats API unreachable' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Retry' })).toBeVisible();
  });
});
