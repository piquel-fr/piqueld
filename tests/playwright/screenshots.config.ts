import { defineConfig } from '@playwright/test';
import type { Options } from './fixtures.js';

// The documentation screenshots (`just docs-screenshots`), apart from the
// test suite: a fixed 1280 px wide window at one device pixel per CSS pixel,
// with dates in one locale and time zone, so each run draws the same pixels.
// Each screenshot is a snapshot in docs/images, rewritten only when the page
// looks different: Chromium's antialiasing alone varies between runs.
export default defineConfig<Options>({
  testDir: './screenshots',
  snapshotPathTemplate: '{testDir}/../../../docs/images/{arg}{ext}',
  updateSnapshots: 'changed',
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: [['list']],
  use: {
    browserName: 'chromium',
    fakeDocker: true,
    viewport: { width: 1280, height: 800 },
    deviceScaleFactor: 1,
    colorScheme: 'dark',
    locale: 'en-US',
    timezoneId: 'UTC',
    // Lets screenshots/docs.ts set the font despite the dashboard's CSP.
    bypassCSP: true,
    trace: 'retain-on-failure',
  },
});
