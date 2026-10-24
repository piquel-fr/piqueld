// Helpers for the documentation screenshots: each one brings the dashboard to
// a page, or state, a guide shows; `shoot` then saves it into docs/images.
import type { Page } from '@playwright/test';
import { test as base, expect, auth, fillRegistration, applicationTab, rewrite, type Cli, type Managed } from '../fixtures.js';
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
    // face, which the dashboard doesn't synthesize. The configuration
    // bypasses the dashboard's CSP for this style.
    await page.addInitScript(() => document.addEventListener('DOMContentLoaded', () => {
      const style = document.createElement('style');
      style.textContent = ':root { font-family: "Liberation Sans", sans-serif !important; }';
      document.head.append(style);
    }));
    // The fixture runs no ingress gateway, which reads as unhealthy. Report
    // it as a daemon with the default configuration does: disabled.
    await rewrite(page, '/api/v1/system/readiness', data => {
      Object.assign(data.ingress, { healthy: true, message: 'Ingress is disabled in daemon TOML; its listeners are stopped' });
    });
    await use(page);
  },
});

/** Saves the page as docs/images/`name`.png, `height` pixels tall, once
 * `freeze` has made its content the same on every run. */
export async function shoot(page: Page, name: string, height = 800) {
  await page.setViewportSize({ width: 1280, height });
  // Stop the dashboard's timers, so no refresh redraws what `freeze` replaced.
  await page.clock.pauseAt(Date.now() + 1_000);
  await freeze(page);
  await expect(page).toHaveScreenshot(`${name}.png`.split('/'));
  await page.clock.resume();
}

/** Replaces the text that differs on every run: each random ID or digest by
 * one derived from its order on the page, and each date by a fixed one. */
async function freeze(page: Page) {
  const identifiers = /\b[0-9a-f]{32,64}\b/g;
  const found = await page.evaluate(pattern => [...new Set(document.body.innerText.match(new RegExp(pattern, 'g')))], identifiers.source);
  const stable = Object.fromEntries(found.map((value, index) =>
    [value, createHash('sha256').update(`piqueld ${index}`).digest('hex').slice(0, value.length)]));
  await page.evaluate(({ identifiers, stable }) => {
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      node.nodeValue = node.nodeValue!
        .replace(new RegExp(identifiers, 'g'), value => stable[value] ?? value)
        .replace(/\b\d{1,2}\/\d{1,2}\/\d{4}, \d{1,2}:\d{2}:\d{2} [AP]M\b/g, '10/24/2026, 9:41:07 AM');
    }
  }, { identifiers: identifiers.source, stable });
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
  const { token } = await auth<Managed>(page, 'manage', { action: 'create_token', grants: [{ permission: 'admin' }], name: 'Screenshots', days: null });
  const applied = await cli.run(['app', 'apply', '--file', file, '--deploy', '--yes'], token);
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
