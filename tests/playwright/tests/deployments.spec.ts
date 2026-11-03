import { test, expect, call, environmentTab, webManifest } from '../fixtures.js';
import type { Page, Route } from '@playwright/test';

type Deployment = { operation: Record<string, unknown> } & Record<string, unknown>;

/** `notes`, deployed once, and that deployment as the daemon lists it. The
 * fixture never deploys, so tests derive the deployments they list from it. */
async function deployed(page: Page) {
  const saved = await call(page, 'POST', 'applications/apply', { manifest: webManifest('notes'), expected_generation: 0 });
  const app = saved.application_id as string;
  await call(page, 'POST', `environments/${app}/deploy?expected_generation=${saved.generation}`);
  const deployment = (await call(page, 'GET', `environments/${app}/deployments`)).items[0] as Deployment;
  return { app, deployment };
}

/** Serves `pages(cursor)` as `environment`'s deployment pages. */
async function serveDeployments(page: Page, environment: string, pages: (cursor: string | null) => { items: Deployment[]; next_cursor: string | null }) {
  await page.route(url => url.pathname === `/api/v1/environments/${environment}/deployments`, async (route: Route) => {
    const cursor = new URL(route.request().url()).searchParams.get('cursor');
    await route.fulfill({ json: { data: pages(cursor) } });
  });
}

test('older deployments load once, and stay listed as the newest page is polled', async ({ page, account }) => {
  void account;
  const { app, deployment } = await deployed(page);
  // Deployment `n`, with time-ordered IDs as the daemon pages them.
  const numbered = (n: number, current = false) => ({
    ...deployment,
    operation: { ...deployment.operation, id: `operation-${String(n).padStart(32, '0')}`, generation: n },
    current_target: current,
  });
  let newest = [numbered(3, true), numbered(2)];
  // The older page repeats deployment 2, which moved down meanwhile.
  await serveDeployments(page, app, cursor => cursor === 'older'
    ? { items: [numbered(2), numbered(1)], next_cursor: null }
    : { items: newest, next_cursor: 'older' });

  await page.goto(`/dashboard/applications/${app}/environments/${app}`);
  await environmentTab(page, 'Deployments').click();
  const history = page.getByRole('region', { name: 'Deployment history' });
  const titles = history.locator('.expander-summary strong');
  await expect(titles).toHaveText(['Deployment #3', 'Deployment #2']);
  const older = history.getByRole('button', { name: 'Load older deployments', exact: true });
  await older.click();
  await expect(titles).toHaveText(['Deployment #3', 'Deployment #2', 'Deployment #1']);
  await expect(older).toBeHidden();

  // A newer deployment becomes the current target: polling adds it and
  // keeps the older pages, which stay exhausted, and moves the flag off #3.
  newest = [numbered(4, true)];
  await expect(titles).toHaveText(['Deployment #4', 'Deployment #3', 'Deployment #2', 'Deployment #1']);
  await expect(history.getByText('Current target', { exact: true })).toHaveCount(1);
  await expect(history.locator('.expander', { hasText: 'Deployment #4' })).toContainText('Current target');
  await expect(older).toBeHidden();
});

test('a linked deployment opens with its attempts, the running one first, and older ones on request', async ({ page, account }) => {
  void account;
  const { app, deployment } = await deployed(page);
  const id = deployment.operation.id as string;
  const running = { ...deployment, operation: { ...deployment.operation, attempt: 3, state: 'running' }, succeeded_at_ms: null };
  await serveDeployments(page, app, () => ({ items: [running], next_cursor: null }));
  // Retained outcomes of the attempts before it, one per page.
  const outcome = (attempt: number, error: Record<string, string | null>) =>
    ({ ...deployment.operation, attempt, state: 'failed', error_code: null, error_message: null, ...error });
  await page.route(url => url.pathname === `/api/v1/environments/${app}/deployments/${id}/attempts`, async (route: Route) => {
    const older = new URL(route.request().url()).searchParams.get('cursor') === 'older';
    await route.fulfill({ json: { data: older
      ? { items: [outcome(1, { error_message: 'registry unreachable' })], next_cursor: null }
      : { items: [outcome(2, { error_code: 'image_pull_failed', error_message: 'pull access denied' })], next_cursor: 'older' } } });
  });

  await page.goto(`/dashboard/applications/${app}/environments/${app}?tab=deployments&deployment=${id}`);
  const card = page.locator('.expander', { hasText: 'Deployment #' });
  await expect(card.getByText('Operation ID', { exact: true })).toBeVisible();
  await card.getByRole('tab', { name: 'Attempts', exact: true }).click();
  const rows = card.locator('tbody tr');
  await expect(rows.locator('td.num')).toHaveText(['3', '2']);
  await expect(rows.nth(1)).toContainText('image_pull_failed: pull access denied');
  const older = card.getByRole('button', { name: 'Older attempts', exact: true });
  await older.click();
  await expect(rows.locator('td.num')).toHaveText(['3', '2', '1']);
  await expect(rows.nth(2)).toContainText('registry unreachable');
  await expect(older).toBeHidden();
});
