import { test, expect } from '../fixtures.js';
import type { Page } from '@playwright/test';

async function createEnvironments(page: Page, collide = false) {
  return page.evaluate(async (collide) => {
    const call = async (path: string, body: unknown) => {
      const response = await fetch(`/api/v1/${path}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      const value = await response.json();
      if (!response.ok) throw new Error(JSON.stringify(value));
      return value.data;
    };
    const saved = await call('applications/apply', {
      manifest: {
        api_version: 'piqueld.dev/v1alpha1', kind: 'Application',
        metadata: { name: 'environments' },
        spec: { services: [{ name: 'web', source: { type: 'image', image: 'nginx:alpine' } }] },
      },
      expected_generation: 0,
    });
    const sibling = await call(`applications/${saved.application_id}/environments`, {
      name: collide ? saved.application_id : 'staging',
      expected_generation: saved.generation,
    });
    return { app: saved.application_id as string, sibling: sibling.id as string };
  }, collide);
}

const applicationTab = (page: Page, name: string) =>
  page.getByRole('navigation', { name: 'Application sections' }).getByRole('button', { name, exact: true });
const environmentTab = (page: Page, name: string) =>
  page.getByRole('navigation', { name: 'Environment sections' }).getByRole('button', { name, exact: true });
const title = (page: Page) => page.locator('.detail-title h1');

test('the application page stays whole and each environment has its own page', async ({ page, account }) => {
  void account;
  const { app, sibling } = await createEnvironments(page);
  await page.goto(`/dashboard/applications/${app}`);
  await expect(page.getByRole('combobox', { name: 'Environment', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Preview', exact: true })).toHaveCount(0);

  // With several environments, deploying starts by choosing one.
  await page.getByRole('button', { name: 'Deploy…', exact: true }).click();
  await expect(applicationTab(page, 'Environments')).toHaveAttribute('aria-current', 'page');
  const environments = page.getByRole('table', { name: 'Environments' });
  await expect(environments.getByRole('link')).toHaveText(['production', 'staging']);

  // Application history spans every environment.
  const history = page.waitForRequest(request => request.url().includes(`/api/v1/events?application_id=${app}&`));
  await applicationTab(page, 'Events').click();
  await history;
  await expect(page.locator('.event', { hasText: 'created environment staging' })).toBeVisible();
  await expect(page.locator('.event', { hasText: 'created environment production' })).toBeVisible();

  await applicationTab(page, 'Environments').click();
  const deploy = page.waitForRequest(request => request.method() === 'POST' && request.url().includes(`/environments/${sibling}/deploy?`));
  await environments.getByRole('button', { name: 'Deploy to staging', exact: true }).click();
  await deploy;
  await expect(page).toHaveURL(url => url.pathname.endsWith(`/environments/${sibling}`) && url.searchParams.get('tab') === 'deployments');
  await expect(environmentTab(page, 'Deployments')).toHaveAttribute('aria-current', 'page');
  await expect(title(page)).toHaveText('staging');
  await expect(page.getByRole('button', { name: 'Deploy to staging', exact: true })).toBeVisible();

  await page.reload();
  await expect(title(page)).toHaveText('staging');
  await page.getByRole('navigation', { name: 'Breadcrumb' }).getByRole('link', { name: 'environments', exact: true }).click();
  await expect(page).toHaveURL(url => url.pathname.endsWith(`/applications/${app}`));
  await expect(applicationTab(page, 'Environments')).toHaveAttribute('aria-current', 'page');
});

test('the dashboard creates, renames and deletes only the chosen environment', async ({ page, account }) => {
  void account;
  const { app } = await createEnvironments(page);
  await page.goto(`/dashboard/applications/${app}?tab=environments`);
  await page.getByRole('button', { name: 'New environment', exact: true }).click();
  const creator = page.getByRole('dialog', { name: 'Create environment' });
  await creator.getByLabel('Environment name', { exact: true }).fill('qa');
  await creator.getByRole('button', { name: 'Create environment', exact: true }).click();
  await expect(title(page)).toHaveText('qa');
  await expect(page.getByRole('button', { name: 'Deploy to qa', exact: true })).toBeEnabled();
  const qa = new URL(page.url()).pathname.split('/').pop()!;
  expect(qa).not.toBe(app);

  await page.getByLabel('Environment name', { exact: true }).fill('preview');
  await page.getByRole('button', { name: 'Rename environment', exact: true }).click();
  await expect(title(page)).toHaveText('preview');
  await expect(page.getByRole('button', { name: 'Deploy to preview', exact: true })).toBeEnabled();

  const remove = page.getByRole('button', { name: 'Delete environment', exact: true });
  page.once('dialog', dialog => dialog.dismiss());
  await remove.click();
  await expect(remove).toBeEnabled();
  page.once('dialog', async dialog => {
    expect(dialog.message()).toContain('The application and other environments remain');
    expect(dialog.message()).toContain('Docker volume data will be retained');
    await dialog.accept();
  });
  const deleted = page.waitForResponse(response => response.request().method() === 'DELETE' && response.url().includes(`/environments/${qa}?`));
  await remove.click();
  expect((await deleted).status()).toBe(202);
  await expect(applicationTab(page, 'Environments')).toHaveAttribute('aria-current', 'page');
  const row = page.getByRole('row').filter({ hasText: 'preview' });
  await expect(row).toContainText('Deleting');
  await expect(row.getByRole('button', { name: 'Deploy to preview', exact: true })).toBeDisabled();
  await expect(page.getByRole('row').filter({ hasText: 'production' }).getByRole('button', { name: 'Deploy to production', exact: true })).toBeEnabled();

  await row.getByRole('link', { name: 'preview', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Deploy to preview', exact: true })).toBeDisabled();
  const retried = page.waitForResponse(response => response.request().method() === 'POST' && response.url().includes(`/environments/${qa}/reconcile?`));
  await page.getByRole('button', { name: 'Retry deletion', exact: true }).click();
  expect((await retried).status()).toBe(202);
  const environments = await page.evaluate(async app => {
    const response = await fetch(`/api/v1/applications/${app}`);
    return (await response.json()).data.environments as { id: string; name: string; delete_intent: boolean }[];
  }, app);
  expect(environments.find(env => env.id === qa)).toMatchObject({ name: 'preview', delete_intent: true });
  expect(environments.find(env => env.id === app)).toMatchObject({ name: 'production', delete_intent: false });
});

test('an application with no environments can create one in the dashboard', async ({ page, account }) => {
  void account;
  const { app, sibling } = await createEnvironments(page);
  // The fixture accepts deletions but does not execute Docker cleanup. Model
  // both application reads after its two original environments have been removed.
  await page.route('**/api/v1/applications**', async route => {
    const path = new URL(route.request().url()).pathname;
    if (route.request().method() !== 'GET' || !['/api/v1/applications', `/api/v1/applications/${app}`].includes(path)) {
      await route.continue();
      return;
    }
    const response = await route.fetch();
    const body = await response.json();
    const applications = path.endsWith(`/${app}`) ? [body.data] : body.data.items;
    for (const application of applications) {
      application.environments = application.environments.filter((env: { id: string }) => ![app, sibling].includes(env.id));
    }
    await route.fulfill({ response, json: body });
  });
  await page.goto(`/dashboard/applications/${app}`);
  await page.getByRole('button', { name: 'Deploy…', exact: true }).click();
  await expect(page.getByText('No environments yet.', { exact: false })).toBeVisible();
  await page.getByRole('button', { name: 'New environment', exact: true }).click();
  const creator = page.getByRole('dialog', { name: 'Create environment' });
  await creator.getByLabel('Environment name', { exact: true }).fill('replacement');
  await creator.getByRole('button', { name: 'Create environment', exact: true }).click();
  await expect(title(page)).toHaveText('replacement');
  await expect(page.getByRole('button', { name: 'Deploy to replacement', exact: true })).toBeEnabled();
  await expect(page).toHaveURL(url => url.pathname.includes(`/applications/${app}/environments/`));
});

test('a deleted environment page cannot resolve to another environment with that name', async ({ page, account }) => {
  void account;
  const { app } = await createEnvironments(page, true);
  await page.goto(`/dashboard/applications/${app}/environments/${app}`);
  await expect(title(page)).toHaveText('production');

  // Model the application's response after deletion has finished. The sibling
  // deliberately has the deleted ID as its name, but a different actual ID.
  await page.route(`**/api/v1/applications/${app}`, async route => {
    const response = await route.fetch();
    const body = await response.json();
    body.data.environments = body.data.environments.filter((environment: { id: string }) => environment.id !== app);
    await route.fulfill({ response, json: body });
  });
  await page.reload();
  await expect(page.getByText('This environment no longer exists.', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: /^Deploy/ })).toHaveCount(0);
});
