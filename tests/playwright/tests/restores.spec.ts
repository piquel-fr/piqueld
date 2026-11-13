import {
  test, expect, auth, confirmNext, deployed, deploymentRows, deploymentTab, environmentTab, openDeployment, register, serveDeployments,
  type Managed,
} from '../fixtures.js';
import type { Route } from '@playwright/test';

test('an earlier deployment is restored from the deployment history', async ({ page, account }) => {
  void account;
  const { app, env, deployment } = await deployed(page);
  const earlier = deployment.operation.id as string;
  // The fixture never deploys: show that deployment as succeeded, and a
  // restore of it as the current one.
  const succeeded = { ...deployment, succeeded_at_ms: Date.now(), current_target: false, last_successful: true };
  const restore = {
    ...deployment,
    operation: { ...deployment.operation, id: 'operation-0123456789abcdf0' },
    origin: { type: 'restore', deployment: earlier },
    succeeded_at_ms: Date.now(),
    current_target: true,
  };
  await serveDeployments(page, env, () => ({ items: [restore, succeeded], next_cursor: null }));
  let restored: unknown;
  await page.route(url => url.pathname === `/api/v1/environments/${env}/restore`, async (route: Route) => {
    restored = route.request().postDataJSON();
    await route.fulfill({
      status: 202,
      json: { data: { operation_id: 'operation-0123456789abcdf1', environment_id: env, generation: deployment.operation.generation } },
    });
  });

  await page.goto(`/dashboard/applications/${app}/environments/${env}`);
  await environmentTab(page, 'Deployments').click();
  const rows = deploymentRows(page);
  await expect(rows).toHaveCount(2);
  const restoreButton = page.getByRole('button', { name: 'Restore', exact: true });
  // The current target has nothing to restore.
  await openDeployment(page, rows.nth(0));
  await expect(page.getByRole('region', { name: 'Deployment progress' })).toContainText(`Restored from deployment ${earlier}`);
  await expect(restoreButton).toHaveCount(0);

  // Restoring the earlier one opens the restoring deployment's page.
  await page.goBack();
  await openDeployment(page, rows.nth(1));
  confirmNext(page);
  await restoreButton.click();
  await expect(page).toHaveURL(`/dashboard/applications/${app}/environments/${env}/deployments/operation-0123456789abcdf1`);
  expect(restored).toEqual({ deployment: earlier });

  // Restoring needs apps:deploy: an account that only reads isn't offered it.
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [
    { permission: 'apps:read', applications: [app] },
  ] })).invitation_url;
  await register(page, link, 'bob');
  await page.goto(`/dashboard/applications/${app}/environments/${env}/deployments/${earlier}`);
  await deploymentTab(page, 'Details').click();
  await expect(page.getByText('Origin', { exact: true })).toBeVisible();
  await expect(restoreButton).toHaveCount(0);
  // Nor its live events, which need events:read.
  await expect(page.getByRole('complementary', { name: 'Live events' })).toHaveCount(0);
});

test('the current deployment is offered for restore after a newer one failed', async ({ page, account }) => {
  void account;
  const { app, env, deployment } = await deployed(page);
  // A newer deployment failed before replacing the earlier one, which stays
  // the current target.
  const current = { ...deployment, succeeded_at_ms: Date.now(), current_target: true, last_successful: true };
  const failed = {
    ...deployment,
    operation: { ...deployment.operation, id: 'operation-0123456789abcdf0', state: 'failed' },
    succeeded_at_ms: null,
    current_target: false,
    last_successful: false,
  };
  await serveDeployments(page, env, () => ({ items: [failed, current], next_cursor: null }));

  await page.goto(`/dashboard/applications/${app}/environments/${env}`);
  await environmentTab(page, 'Deployments').click();
  const rows = deploymentRows(page);
  await expect(rows).toHaveCount(2);
  await openDeployment(page, rows.nth(1));
  await expect(page.getByRole('button', { name: 'Restore', exact: true })).toBeVisible();
});
