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

const selector = (page: Page) => page.getByRole('combobox', { name: 'Environment', exact: true });

test('environment selection survives reload, history and service navigation', async ({ page, account }) => {
  void account;
  const { app, sibling } = await createEnvironments(page);
  await page.goto(`/dashboard/applications/${app}?tab=services`);
  await expect(selector(page)).toHaveValue(app);
  await selector(page).selectOption(sibling);
  await expect(page.getByText('Shared across all environments.', { exact: false }).filter({ visible: true })).toBeVisible();
  await expect(page).toHaveURL(url => url.searchParams.get('environment') === sibling && url.searchParams.get('tab') === 'services');
  await page.getByRole('navigation', { name: 'Application sections' }).getByRole('button', { name: 'Volumes', exact: true }).click();
  await page.getByRole('button', { name: 'Add volume', exact: true }).click();
  const dialog = page.waitForEvent('dialog');
  await page.evaluate(() => history.back());
  await (await dialog).dismiss();
  await expect(page).toHaveURL(url => url.searchParams.get('environment') === sibling && url.searchParams.get('tab') === 'services');
  await expect(selector(page)).toHaveValue(sibling);
  await page.getByRole('button', { name: 'Discard', exact: true }).filter({ visible: true }).click();
  await page.getByRole('navigation', { name: 'Application sections' }).getByRole('button', { name: 'Services', exact: true }).click();

  await page.reload();
  await expect(selector(page)).toHaveValue(sibling);
  await page.goBack();
  await expect(selector(page)).toHaveValue(app);
  await page.goForward();
  await expect(selector(page)).toHaveValue(sibling);

  await page.getByRole('link').filter({ hasText: 'web' }).click();
  await expect(page.getByText('Shared across all environments.', { exact: false }).filter({ visible: true })).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`/services/web\\?environment=${sibling}$`));
  await page.reload();
  await page.getByRole('navigation', { name: 'Breadcrumb' }).getByRole('link', { name: 'environments', exact: true }).click();
  await expect(selector(page)).toHaveValue(sibling);
  await expect(page).toHaveURL(url => url.searchParams.get('environment') === sibling && url.searchParams.get('tab') === 'services');

  const request = page.waitForRequest(r => r.method() === 'POST' && r.url().includes('/deploy?'));
  await page.getByRole('button', { name: 'Deploy to staging', exact: true }).click();
  expect((await request).url()).toContain(`/environments/${sibling}/deploy`);
});

test('the dashboard creates, renames and deletes only the chosen environment', async ({ page, account }) => {
  void account;
  const { app } = await createEnvironments(page);
  await page.goto(`/dashboard/applications/${app}`);
  await page.getByRole('button', { name: 'Manage environments', exact: true }).click();
  const manager = page.getByRole('dialog', { name: 'Manage environments' });
  await manager.getByLabel('Environment name', { exact: true }).fill('qa');
  await manager.getByRole('button', { name: 'Create environment', exact: true }).click();
  await expect(manager).toBeHidden();
  const target = page.getByRole('button', { name: 'Deploy to qa', exact: true });
  await expect(target).toBeEnabled();
  const qa = await selector(page).inputValue();
  expect(qa).not.toBe(app);

  await page.getByRole('button', { name: 'Manage environments', exact: true }).click();
  await manager.locator('.section-header').filter({ has: page.getByText('qa', { exact: true }) }).getByRole('button', { name: 'Rename', exact: true }).click();
  await manager.getByLabel('Environment name', { exact: true }).fill('preview');
  await manager.getByRole('button', { name: 'Rename environment', exact: true }).click();
  await expect(manager).toBeHidden();
  await expect(page.getByRole('button', { name: 'Deploy to preview', exact: true })).toBeEnabled();
  await expect(selector(page)).toHaveValue(qa);

  await page.getByRole('button', { name: 'Manage environments', exact: true }).click();
  const remove = manager.locator('.section-header').filter({ has: page.getByText('preview', { exact: true }) }).getByRole('button', { name: 'Delete', exact: true });
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
  await expect(manager).toContainText('preview (deleting)');
  const retried = page.waitForResponse(response => response.request().method() === 'POST' && response.url().includes(`/environments/${qa}/reconcile?`));
  await manager.getByRole('button', { name: 'Retry deletion', exact: true }).click();
  expect((await retried).status()).toBe(202);
  await manager.getByRole('button', { name: 'Close dialog' }).click();
  await expect(page.getByRole('button', { name: 'Deploy to preview', exact: true })).toBeDisabled();
  await expect(page.getByRole('button', { name: 'Preview', exact: true })).toBeDisabled();
  await selector(page).selectOption(app);
  await expect(page.getByRole('button', { name: 'Deploy to production', exact: true })).toBeEnabled();
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
  await expect(page.getByRole('status').filter({ hasText: 'This application has no environments.' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Deploy', exact: true })).toBeDisabled();
  await page.getByRole('button', { name: 'Manage environments', exact: true }).click();
  const manager = page.getByRole('dialog', { name: 'Manage environments' });
  await manager.getByLabel('Environment name', { exact: true }).fill('replacement');
  await manager.getByRole('button', { name: 'Create environment', exact: true }).click();
  await expect(manager).toBeHidden();
  await expect(page.getByRole('button', { name: 'Deploy to replacement', exact: true })).toBeEnabled();
  await expect(page).toHaveURL(url => url.pathname.endsWith(app) && url.searchParams.has('environment'));
});

test('a deleted selected ID cannot resolve to another environment with that name', async ({ page, account }) => {
  void account;
  const { app, sibling } = await createEnvironments(page, true);
  await page.goto(`/dashboard/applications/${app}?environment=${app}`);
  await expect(selector(page)).toHaveValue(app);

  // Model the application's response after deletion has finished. The sibling
  // deliberately has the deleted ID as its name, but a different actual ID.
  await page.route(`**/api/v1/applications/${app}`, async route => {
    const response = await route.fetch();
    const body = await response.json();
    body.data.environments = body.data.environments.filter((environment: { id: string }) => environment.id !== app);
    await route.fulfill({ response, json: body });
  });
  await page.reload();
  await expect(page.getByRole('status').filter({ hasText: 'The selected environment no longer exists. Select another environment.' })).toBeVisible();
  await selector(page).selectOption(sibling);
  await expect(selector(page)).toHaveValue(sibling);
  await expect(page.getByRole('status').filter({ hasText: 'The selected environment no longer exists. Select another environment.' })).toBeHidden();
});
