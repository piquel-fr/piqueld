import { test, expect, apply, call, rewrite, webManifest } from '../fixtures.js';
import type { Page, Route } from '@playwright/test';
/** The value of the overview's `label` metric tile. */
const metric = (page: Page, label: string) => page.locator('.metric', { hasText: label }).locator('strong');
/** A refused API response with `message`. */
const refusal = (message: string) => ({ status: 503, json: { code: 'unavailable', message, request_id: 'request' } });

test('the overview sums up applications by their least healthy environment', async ({ page, account }) => {
  void account;
  const shop = await apply(page, webManifest('shop'));
  const notes = await apply(page, webManifest('notes'));
  const blog = await apply(page, webManifest('blog'));
  const deployed = await call(page, 'POST', `environments/${shop.env}/deploy?expected_generation=${shop.generation}`);
  await call(page, 'POST', `environments/${notes.env}/deploy?expected_generation=${notes.generation}`);
  const staging = await call(page, 'POST', `applications/${shop.app}/environments`, { name: 'staging', expected_generation: shop.generation });
  // The fixture never deploys: report each environment's state instead.
  const state = (id: string, value: string, message: string | null = null) =>
    rewrite(page, `/api/v1/environments/${id}/status`, data => { data.state = value; data.message = message; });
  await state(shop.env, 'ready');
  await state(staging.id as string, 'failed', 'image pull failed');
  await state(notes.env, 'ready');
  await state(blog.env, 'degraded');
  // One application's unreadable deployments leave the others listed.
  await page.route(url => url.pathname === `/api/v1/environments/${blog.env}/deployments`,
    (route: Route) => route.fulfill(refusal('deployment history unavailable')));

  await page.goto('/dashboard/');
  await expect(metric(page, 'Applications')).toHaveText('3');
  await expect(metric(page, 'Healthy')).toHaveText('1');
  await expect(metric(page, 'Needs attention')).toHaveText('2');
  const recent = page.locator('section', { has: page.getByRole('heading', { name: 'Recent deployments', exact: true }) });
  await expect(recent.getByRole('alert')).toHaveText('blog: deployment history unavailable');
  await expect(recent.getByRole('row')).toHaveCount(3);
  await expect(recent.getByRole('link', { name: 'shop', exact: true }))
    .toHaveAttribute('href', `/dashboard/applications/${shop.app}/environments/${shop.env}/deployments/${deployed.operation_id}`);

  await page.getByRole('complementary').getByRole('link', { name: 'Applications', exact: true }).click();
  const row = (name: string) => page.locator('a.list-row', { hasText: name });
  await expect(row('shop')).toContainText('staging: image pull failed');
  await expect(row('shop').locator('.badge')).toHaveText('Failed');
  await expect(row('shop')).toContainText('Deployed');
  await expect(row('notes').locator('.badge')).toHaveText('Converged');
  await expect(row('blog').locator('.badge')).toHaveText('Degraded');
  await expect(row('blog')).toContainText('Never deployed');
});

test('a failed refresh keeps the last successful view, marked stale, until one succeeds', async ({ page, account }) => {
  void account;
  await apply(page, webManifest('shop'));
  let failure: 'refused' | 'unreachable' | null = 'refused';
  await page.route(url => url.pathname === '/api/v1/system/status', (route: Route) => {
    if (failure === 'refused') return route.fulfill(refusal('maintenance window'));
    if (failure === 'unreachable') return route.abort('connectionrefused');
    return route.fallback();
  });
  const main = page.getByRole('main');
  const connection = page.getByRole('complementary').locator('.connection');
  const stale = page.getByText('Showing the last successful view; the latest refresh failed.', { exact: true });
  const refresh = async (next: typeof failure) => {
    failure = next;
    await page.getByRole('button', { name: 'Refresh', exact: true }).click();
  };

  // Without an earlier view there is nothing stale to show.
  await page.goto('/dashboard/');
  await expect(main.getByRole('alert').first()).toContainText(/Refresh failed\s*maintenance window/);
  await expect(connection).toHaveText('Request failed');
  await expect(stale).toBeHidden();
  await expect(metric(page, 'Applications')).toHaveText('0');

  await refresh(null);
  await expect(metric(page, 'Applications')).toHaveText('1');
  await expect(connection).toHaveText('Reachable');
  await expect(main.getByText('Refresh failed', { exact: true })).toHaveCount(0);

  await refresh('refused');
  await expect(main.getByRole('alert').first()).toContainText(/Refresh failed\s*maintenance window/);
  await expect(stale).toBeVisible();
  await expect(metric(page, 'Applications')).toHaveText('1');

  await refresh('unreachable');
  await expect(main.getByRole('alert').first()).toContainText('Daemon unreachable');
  await expect(connection).toHaveText('Unreachable');
  await expect(stale).toBeVisible();

  await refresh(null);
  await expect(connection).toHaveText('Reachable');
  await expect(stale).toBeHidden();
  await expect(main.getByText('Daemon unreachable', { exact: true })).toHaveCount(0);
});
