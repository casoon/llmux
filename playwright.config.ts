import { defineConfig, devices } from '@playwright/test';

// Serves the static dashboard build (dist/dashboard) and runs the smoke test against it.
// The Stats API is mocked per-test via page.route, so no llmux backend is required.
export default defineConfig({
  testDir: './e2e',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: process.env.CI ? 1 : undefined,
  reporter: 'line',
  projects: [
    {
      name: 'dashboard',
      use: { ...devices['Desktop Chrome'], baseURL: 'http://localhost:5173' },
      testMatch: 'dashboard/**/*.spec.ts',
    },
  ],
  webServer: [
    {
      command: 'npx serve dist/dashboard -l 5173 -s',
      port: 5173,
      reuseExistingServer: !process.env.CI,
    },
  ],
});
