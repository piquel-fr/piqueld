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

test('a deployment\'s timeline lays its steps out by stage, and shows the one picked', async ({ page, account }) => {
  void account;
  const { app, env, deployment } = await deployed(page);
  const operation = { ...deployment.operation, state: 'succeeded', started_at_ms: 0, finished_at_ms: 40_000 };
  const step = (phase: string, resource: string, start: number, end: number, expected: number) => ({
    phase, resource, state: 'succeeded', action_id: `${phase}-${resource}`, attempt: 1,
    started_at_ms: start, finished_at_ms: end, retries: 0, expected_ms: expected,
  });
  const timeline = {
    operation,
    steps: [
      step('building_git', 'web', 0, 20_000, 25_000),
      step('ensure_service', 'web', 20_000, 21_000, 1_000),
      // Twice as long as usual.
      step('wait_for_service', 'web', 21_000, 40_000, 9_000),
    ],
    cursor: 42,
  };
  await serveDeployments(page, env, () => ({ items: [{ ...deployment, operation }], next_cursor: null }));
  const id = deployment.operation.id as string;
  await page.route(url => url.pathname === `/api/v1/operations/${id}/timeline/stream`, (route: Route) =>
    route.fulfill({ contentType: 'text/event-stream', body: `event: timeline\ndata: ${JSON.stringify(timeline)}\n\n` }));

  await page.goto(`/dashboard/applications/${app}/environments/${env}?tab=deployments&deployment=${id}`);
  const card = deploymentCards(page);
  await card.getByRole('tab', { name: 'Timeline', exact: true }).click();
  await expect(card.locator('.waterfall-stage')).toHaveText(['Prepare', 'Roll out']);
  const steps = card.getByRole('list', { name: 'Steps' }).getByRole('button');
  await expect(steps).toHaveCount(3);
  await expect(steps.nth(0)).toContainText('Build web');
  await expect(steps.locator('.waterfall-duration')).toHaveText(['20.0 s', '1.0 s', '19.0 s']);
  await expect(steps.nth(2).locator('.waterfall-duration')).toHaveClass(/slow/);

  // The last step is shown until another is picked.
  const detail = card.getByRole('region', { name: 'Step detail' });
  await expect(detail.getByRole('heading')).toHaveText('Wait for service web');
  await steps.nth(0).click();
  await expect(detail.getByRole('heading')).toHaveText('Build web');
  await expect(detail).toContainText('25.0 s');
  await detail.getByRole('button', { name: 'Follow live', exact: true }).click();
  await expect(detail.getByRole('heading')).toHaveText('Wait for service web');
});
