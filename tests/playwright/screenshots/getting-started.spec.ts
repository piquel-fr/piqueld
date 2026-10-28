import { submitRegistration, applicationTab, expect } from '../fixtures.js';
import { test, accountForm, createApplication, environment, newApplication, overview, saveChanges, shoot } from './docs.js';

// docs/user/getting-started: the first account, then the first application,
// created in the dashboard.
test('getting started', async ({ page, daemon, passkeys }) => {
  void passkeys;
  await accountForm(page, daemon.setup);
  await shoot(page, 'getting-started/create-account');
  await submitRegistration(page);

  await newApplication(page, 'hello');
  await shoot(page, 'getting-started/new-application');
  await createApplication(page, 'hello');
  await applicationTab(page, 'Services').click();
  await page.getByRole('button', { name: 'Add service', exact: true }).first().click();
  const dialog = page.getByRole('dialog', { name: 'Add service' });
  await dialog.getByLabel('Service name', { exact: true }).fill('web');
  await dialog.getByLabel('Container image', { exact: true }).fill('nginx:alpine');
  await shoot(page, 'getting-started/add-service');
  await dialog.getByRole('button', { name: 'Add service', exact: true }).click();
  await expect(dialog).toBeHidden();
  await page.locator('a.list-row', { hasText: 'web' }).click();
  await page.getByRole('navigation', { name: 'Service sections' }).getByRole('button', { name: 'Health check', exact: true }).click();
  await page.getByRole('combobox', { name: 'Check type', exact: true }).selectOption({ label: 'HTTP' });
  await page.getByLabel('Port', { exact: true }).fill('80');
  await page.getByLabel('Path', { exact: true }).fill('/');
  await shoot(page, 'getting-started/health-check', page.getByLabel('Path', { exact: true }));
  await saveChanges(page);
  await page.locator('.breadcrumb').getByRole('link', { name: 'hello', exact: true }).click();
  await page.getByRole('button', { name: 'Deploy to production', exact: true }).click();

  await overview(page);
  await shoot(page, 'getting-started/overview', page.getByRole('region', { name: 'Recent deployments' }));
  await environment(page, 'hello', 'production');
  await shoot(page, 'getting-started/environment', page.getByRole('region', { name: 'Observed services' }));
});
