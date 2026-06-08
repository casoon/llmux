import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page } from '@playwright/test';

// Minimal Stats API mock so the page reaches its ready state before the a11y scan.
async function mockStats(page: Page) {
  const ok = (json: unknown) => ({ json });
  await page.route('**/api/stats/overview', (r) =>
    r.fulfill(
      ok({
        status: 'online',
        total_requests: 10,
        requests_per_minute: 1,
        cache_hit_rate: 0.2,
        error_count: 0,
        p95_latency_ms: 1200,
        cost_today: 0.1,
        cost_month: 1,
        budget_pressure: 0.3,
        daily_max_usd: 2,
        monthly_max_usd: 50,
      }),
    ),
  );
  await page.route('**/api/stats/requests*', (r) =>
    r.fulfill(
      ok({
        requests: [
          {
            id: 1,
            time: new Date().toISOString(),
            tool: 'codex',
            session: null,
            project: 'llmux',
            task_type: 'code_review',
            model: 'gpt-4.1-mini',
            provider: 'openai',
            tier: 3,
            used_fallback: false,
            degraded: false,
            estimated_cost_usd: 0.002,
            real_cost_usd: 0.002,
            prompt_tokens: 100,
            completion_tokens: 20,
            latency_ms: 1300,
            status: 200,
            cache_hit: false,
            forced: false,
            attempts: 1,
            attempt_trail: '[{"provider":"openai","model":"gpt-4.1-mini","status":200}]',
            stop_reason: 'stop',
            error: null,
            result: 'allowed',
          },
        ],
      }),
    ),
  );
  await page.route('**/api/stats/models', (r) => r.fulfill(ok({ models: [] })));
  await page.route('**/api/stats/policy', (r) =>
    r.fulfill(
      ok({
        allowed: 8,
        rejected: 0,
        degraded: 0,
        fallback: 0,
        cached: 2,
        forced: 0,
        forced_rejected: 0,
        local_only: 1,
        top_rejection_reasons: [],
      }),
    ),
  );
  await page.route('**/api/stats/projects', (r) => r.fulfill(ok({ projects: [] })));
  await page.route('**/api/stats/budget-series', (r) =>
    r.fulfill(ok({ buckets: [], daily_max_usd: 2, monthly_max_usd: 50, spent_today: 0.1 })),
  );
}

test.describe('dashboard accessibility', () => {
  test('has no WCAG A/AA violations on the loaded view', async ({ page }) => {
    await mockStats(page);
    await page.goto('/');
    await expect(page.getByRole('status')).toContainText('online');

    const results = await new AxeBuilder({ page })
      .withTags(['wcag2a', 'wcag2aa', 'wcag22aa'])
      .analyze();
    expect(results.violations).toEqual([]);
  });

  test('request rows are keyboard operable and the drawer traps Escape', async ({ page }) => {
    await mockStats(page);
    await page.goto('/');

    const row = page.locator('tbody [role="button"][aria-label^="Inspect request"]').first();
    await row.focus();
    await expect(row).toBeFocused();
    await page.keyboard.press('Enter');
    await expect(page.getByRole('dialog')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.getByRole('dialog')).toBeHidden();
  });
});
