// Helpers for the documentation screenshots: each one brings the dashboard to
// a page, or state, a guide shows; `shoot` then saves it into docs/images.
import type { Locator, Page } from '@playwright/test';
import { test as base, expect, adminToken, fillRegistration, applicationTab, rewrite, type Cli } from '../fixtures.js';
import { createHash } from 'node:crypto';
import { readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

const docs = new URL('../../../docs/', import.meta.url);

/** The fixtures of the browser tests, with the page's clock under the
 * test's control, which `shoot` needs, and the page drawn as on a host. */
export const test = base.extend({
  page: async ({ page }, use) => {
    await page.clock.install();
    // The browser image has no system UI font: its fallback has no bold
    // face, which the dashboard doesn't synthesize. The stylesheet itself
    // sets one, so the dashboard's CSP still applies.
    await page.route(url => url.pathname.startsWith('/dashboard/') && url.pathname.endsWith('.css'), async route => {
      const response = await route.fetch();
      const body = `${await response.text()}\n:root { font-family: "Liberation Sans", sans-serif !important; }\n`;
      await route.fulfill({ response, body });
    });
    // The fixture runs no ingress gateway, which reads as unhealthy. Report
    // it as a daemon with the default configuration does: disabled.
    await rewrite(page, '/api/v1/system/readiness', data => {
      if (!data.ingress.enabled) Object.assign(data.ingress, { healthy: true, message: 'Ingress is disabled in daemon TOML; its listeners are stopped' });
    });
    await use(page);
  },
});

/** Saves the page as docs/images/`name`.png, cut 12 pixels below the
 * bottom of `through`, or 800 pixels tall without it, once `freeze` has
 * made its content the same on every run. */
export async function shoot(page: Page, name: string, through?: Locator) {
  const box = await through?.boundingBox();
  await page.setViewportSize({ width: 1280, height: box ? Math.ceil(box.y + box.height) + 12 : 800 });
  // Stop the dashboard's timers, without running any, so no refresh redraws
  // what `freeze` replaced. Fixing the time first makes pausing at it skip
  // nothing, however long the calls take.
  const now = Date.now();
  await page.clock.setFixedTime(now);
  await page.clock.pauseAt(now);
  const frozen = await freeze(page);
  await expect(page).toHaveScreenshot(`${name}.png`.split('/'));
  // A refresh already under way when the clock stopped redraws it anyway.
  expect(frozen, 'a refresh redrew the page while it was saved').toEqual(expect.arrayContaining(await varying(page)));
  // Catch up with the time spent paused, so later pages read the daemon's
  // new timestamps as just now.
  await page.clock.setSystemTime(Date.now());
  await page.clock.resume();
}

// The text that differs on every run: random IDs and digests, and dates.
const identifier = String.raw`\b[0-9a-f]{32,64}\b`;
const date = String.raw`\b\d{1,2}\/\d{1,2}\/\d{4}, \d{1,2}:\d{2}:\d{2} [AP]M\b`;
const fixedDate = '10/24/2026, 9:41:07 AM';

/** Each distinct ID, digest and date the page shows. */
async function varying(page: Page): Promise<string[]> {
  return page.evaluate(pattern => [...new Set(document.body.innerText.match(new RegExp(pattern, 'g')))], `${identifier}|${date}`);
}

/** Replaces the text that differs on every run: each random ID or digest by
 * one derived from its order on the page, and each date by a fixed one.
 * Returns the replacements. */
async function freeze(page: Page): Promise<string[]> {
  const identifiers = (await varying(page)).filter(value => new RegExp(`^${identifier}$`).test(value));
  const stable = Object.fromEntries(identifiers.map((value, index) =>
    [value, createHash('sha256').update(`piqueld ${index}`).digest('hex').slice(0, value.length)]));
  await page.evaluate(({ pattern, stable, fixedDate }) => {
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      node.nodeValue = node.nodeValue!.replace(new RegExp(pattern, 'g'), (value, id?: string) => id ? stable[id] ?? id : fixedDate);
    }
  }, { pattern: `(${identifier})|${date}`, stable, fixedDate });
  return [...Object.values(stable), fixedDate];
}

/** The "Create your account" page of setup `link`, filled in for alice. */
export async function accountForm(page: Page, link: string) {
  await fillRegistration(page, link, 'alice', { display: 'Alice', passkey: 'Laptop' });
}

/** Deploys the first manifest of `guide`, a page under docs/, with
 * `piquelctl app apply`, as the guide does, authenticated by a token of the
 * signed-in account. */
export async function deployFrom(page: Page, cli: Cli, guide: string) {
  const markdown = await readFile(new URL(guide, docs), 'utf8');
  const manifest = markdown.match(/```toml\n([\s\S]*?)```/)?.[1];
  if (!manifest) throw new Error(`${guide} shows no TOML manifest`);
  const file = join(cli.daemon.directory, 'manifest.toml');
  await writeFile(file, manifest);
  const applied = await cli.run(['app', 'apply', '--file', file, '--deploy', '--yes'], await adminToken(page, 'Screenshots'));
  expect(applied.code, applied.stderr).toBe(0);
}

/** The Overview, once every application is healthy and the latest deployment succeeded. */
export async function overview(page: Page) {
  await page.goto('/dashboard/');
  const metric = (name: string) => page.locator('.metric', { hasText: name }).locator('strong').textContent();
  await expect.poll(async () => await metric('Healthy') === await metric('Applications')).toBe(true);
  await expect(page.getByRole('region', { name: 'Recent deployments' }).getByRole('row').nth(1)).toContainText(/succeeded/i);
}

/** Environment `name` of application `app`, reached as a reader would, once
 * its services converged. */
export async function environment(page: Page, app: string, name: string) {
  await page.getByRole('link', { name: 'Applications', exact: true }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Applications');
  await page.getByRole('region', { name: 'Applications', exact: true }).getByRole('link').filter({ hasText: app }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText(app);
  await applicationTab(page, 'Environments').click();
  await page.getByRole('main').getByRole('link').filter({ hasText: name }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText(name);
  await expect(page.getByRole('region', { name: 'Observed services' })).toContainText(/converged/i);
}
