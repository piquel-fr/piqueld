import { test, expect, auth, call, environmentTab, register, webManifest, type Managed } from '../fixtures.js';
import type { Route } from '@playwright/test';

test('an earlier deployment is restored from the deployment history', async ({ page, account }) => {
  void account;
  const saved = await call(page, 'POST', 'applications/apply', { manifest: webManifest('notes'), expected_generation: 0 });
  const app = saved.application_id as string;
  const accepted = await call(page, 'POST', `environments/${app}/deploy?expected_generation=${saved.generation}`);
  const earlier = accepted.operation_id as string;
  // The fixture never deploys: show that deployment as succeeded, and a
  // restore of it as the current one.
  await page.route(url => url.pathname === `/api/v1/environments/${app}/deployments`, async (route: Route) => {
    const { data } = await (await route.fetch()).json();
    const deployment = data.items.find((item: { operation: { id: string } }) => item.operation.id === earlier);
    const succeeded = { ...deployment, succeeded_at_ms: Date.now(), current_target: false, last_successful: true };
    const restore = {
      ...deployment,
      operation: { ...deployment.operation, id: 'operation-0123456789abcdf0' },
      origin: { type: 'restore', deployment: earlier },
      current_target: true,
    };
    await route.fulfill({ json: { data: { items: [restore, succeeded], next_cursor: null } } });
  });
  let restored: unknown;
  await page.route(url => url.pathname === `/api/v1/environments/${app}/restore`, async (route: Route) => {
    restored = route.request().postDataJSON();
    await route.fulfill({
      status: 202,
      json: { data: { operation_id: 'operation-0123456789abcdf1', environment_id: app, generation: saved.generation } },
    });
  });

  await page.goto(`/dashboard/applications/${app}/environments/${app}`);
  await environmentTab(page, 'Deployments').click();
  const cards = page.locator('.expander', { hasText: 'Deployment #' });
  await expect(cards).toHaveCount(2);
  // The current target has nothing to restore.
  await cards.nth(0).getByRole('button', { name: /Deployment #/ }).click();
  await expect(cards.nth(0)).toContainText(`Restored from deployment ${earlier}`);
  await expect(cards.nth(0).getByRole('button', { name: 'Restore', exact: true })).toHaveCount(0);

  await cards.nth(1).getByRole('button', { name: /Deployment #/ }).click();
  page.once('dialog', dialog => dialog.accept());
  await cards.nth(1).getByRole('button', { name: 'Restore', exact: true }).click();
  await expect(page.getByText('Restore accepted. Follow its progress below.', { exact: true })).toBeVisible();
  expect(restored).toEqual({ deployment: earlier });

  // Restoring needs apps:deploy: an account that only reads isn't offered it.
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [
    { permission: 'apps:read', applications: [app] },
  ] })).invitation_url;
  await register(page, link, 'bob');
  await page.goto(`/dashboard/applications/${app}/environments/${app}`);
  await environmentTab(page, 'Deployments').click();
  await cards.nth(1).getByRole('button', { name: /Deployment #/ }).click();
  await expect(cards.nth(1)).toContainText('Origin');
  await expect(cards.nth(1).getByRole('button', { name: 'Restore', exact: true })).toHaveCount(0);
});

test('the current deployment is offered for restore after a newer one failed', async ({ page, account }) => {
  void account;
  const saved = await call(page, 'POST', 'applications/apply', { manifest: webManifest('notes'), expected_generation: 0 });
  const app = saved.application_id as string;
  const accepted = await call(page, 'POST', `environments/${app}/deploy?expected_generation=${saved.generation}`);
  const earlier = accepted.operation_id as string;
  // A newer deployment failed before replacing the earlier one, which stays
  // the current target.
  await page.route(url => url.pathname === `/api/v1/environments/${app}/deployments`, async (route: Route) => {
    const { data } = await (await route.fetch()).json();
    const deployment = data.items.find((item: { operation: { id: string } }) => item.operation.id === earlier);
    const current = { ...deployment, succeeded_at_ms: Date.now(), current_target: true, last_successful: true };
    const failed = {
      ...deployment,
      operation: { ...deployment.operation, id: 'operation-0123456789abcdf0', state: 'failed' },
      succeeded_at_ms: null,
      current_target: false,
      last_successful: false,
    };
    await route.fulfill({ json: { data: { items: [failed, current], next_cursor: null } } });
  });

  await page.goto(`/dashboard/applications/${app}/environments/${app}`);
  await environmentTab(page, 'Deployments').click();
  const cards = page.locator('.expander', { hasText: 'Deployment #' });
  await expect(cards).toHaveCount(2);
  await cards.nth(1).getByRole('button', { name: /Deployment #/ }).click();
  await expect(cards.nth(1).getByRole('button', { name: 'Restore', exact: true })).toBeVisible();
});
