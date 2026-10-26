import { defineConfig } from '@playwright/test';
import type { Options } from './fixtures.js';

// The documentation screenshots (`just docs-screenshots`), apart from the
// test suite: a fixed 1280 px wide window at one device pixel per CSS pixel,
// with dates in one locale and time zone, so each run draws the same page.
// `shoot` in screenshots/docs.ts saves each one in docs/images.
export default defineConfig<Options>({
  testDir: './screenshots',
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
    trace: 'retain-on-failure',
  },
});
