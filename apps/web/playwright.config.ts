import { defineConfig } from '@playwright/test'

/**
 * Two lanes share one runner. `fixtures` drives the dev-server-only design page
 * (no server, no data). `live` drives the workbench served by an isolated
 * mink-server whose temporary home and deterministic provider are installed by
 * `e2e/global-setup.ts`, so it exercises the production form end to end without
 * touching the developer's own sessions.
 */
export default defineConfig({
  testDir: './e2e',
  timeout: 60_000,
  expect: { timeout: 15_000 },
  retries: 0,
  globalSetup: './e2e/global-setup.ts',
  globalTeardown: './e2e/global-teardown.ts',
  reporter: [['list']],
  /** Screenshot baselines the visual guard compares against. */
  snapshotPathTemplate: '{testDir}/{testFilePath}-snapshots/{arg}-{platform}{ext}',
  projects: [
    {
      name: 'fixtures',
      testMatch: /(fixtures|visual)\.spec\.ts/,
      use: { baseURL: 'http://127.0.0.1:5174', viewport: { width: 1440, height: 900 } },
    },
    {
      name: 'live',
      testMatch: /live\.spec\.ts/,
      use: { baseURL: 'http://127.0.0.1:18821', viewport: { width: 1440, height: 900 }, screenshot: 'only-on-failure' },
    },
  ],
  webServer: {
    command: 'npm run dev -- --port 5174 --strictPort',
    url: 'http://127.0.0.1:5174',
    reuseExistingServer: true,
    timeout: 60_000,
  },
})
