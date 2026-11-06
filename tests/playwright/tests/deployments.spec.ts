import { test, expect, deployed, deploymentCards, environmentTab, serveDeployments } from '../fixtures.js';
import type { Route } from '@playwright/test';

test('older deployments load once, and stay listed as the newest page is polled', async ({ page, account }) => {
  void account;
  const { app, env, deployment } = await deployed(page);
  // Deployment `n`, with time-ordered IDs as the daemon pages them.
  const numbered = (n: number, current = false) => ({
    ...deployment,
    operation: { ...deployment.operation, id: `operation-${String(n).padStart(32, '0')}`, generation: n },
    current_target: current,
  });
  let newest = [numbered(3, true), numbered(2)];
  // The older page repeats deployment 2, which moved down meanwhile.
  await serveDeployments(page, env, cursor => cursor === 'older'
    ? { items: [numbered(2), numbered(1)], next_cursor: null }
    : { items: newest, next_cursor: 'older' });

  await page.goto(`/dashboard/applications/${app}/environments/${env}`);
  await environmentTab(page, 'Deployments').click();
  const history = page.getByRole('region', { name: 'Deployment history' });
  const titles = history.locator('.expander-summary .tag', { hasText: /^Revision/ });
  await expect(titles).toHaveText(['Revision 3', 'Revision 2']);
  const older = history.getByRole('button', { name: 'Load older deployments', exact: true });
  await older.click();
  await expect(titles).toHaveText(['Revision 3', 'Revision 2', 'Revision 1']);
  await expect(older).toBeHidden();

  // A newer deployment becomes the current target: polling adds it and
  // keeps the older pages, which stay exhausted, and moves the flag off #3.
  newest = [numbered(4, true)];
  await expect(titles).toHaveText(['Revision 4', 'Revision 3', 'Revision 2', 'Revision 1']);
  await expect(history.getByText('Current target', { exact: true })).toHaveCount(1);
  await expect(history.locator('.expander', { hasText: 'Revision 4' })).toContainText('Current target');
  await expect(older).toBeHidden();
});

test('a linked deployment opens with its attempts, the running one first, and older ones on request', async ({ page, account }) => {
  void account;
  const { app, env, deployment } = await deployed(page);
  const id = deployment.operation.id as string;
  const running = { ...deployment, operation: { ...deployment.operation, attempt: 3, state: 'running' }, succeeded_at_ms: null };
  await serveDeployments(page, env, () => ({ items: [running], next_cursor: null }));
  // Retained outcomes of the attempts before it, one per page.
  const outcome = (attempt: number, error: Record<string, string | null>) =>
    ({ ...deployment.operation, attempt, state: 'failed', error_code: null, error_message: null, ...error });
  await page.route(url => url.pathname === `/api/v1/environments/${env}/deployments/${id}/attempts`, async (route: Route) => {
    const older = new URL(route.request().url()).searchParams.get('cursor') === 'older';
    await route.fulfill({ json: { data: older
      ? { items: [outcome(1, { error_message: 'registry unreachable' })], next_cursor: null }
      : { items: [outcome(2, { error_code: 'image_pull_failed', error_message: 'pull access denied' })], next_cursor: 'older' } } });
  });

  await page.goto(`/dashboard/applications/${app}/environments/${env}?tab=deployments&deployment=${id}`);
  const card = deploymentCards(page);
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
