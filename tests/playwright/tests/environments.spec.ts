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
  await expect(page).toHaveURL(new RegExp(`/services/web\\?environment=${sibling}$`));
  await page.reload();
  await page.getByRole('navigation', { name: 'Breadcrumb' }).getByRole('link', { name: 'environments', exact: true }).click();
  await expect(selector(page)).toHaveValue(sibling);
  await expect(page).toHaveURL(url => url.searchParams.get('environment') === sibling && url.searchParams.get('tab') === 'services');

  const request = page.waitForRequest(r => r.method() === 'POST' && r.url().includes('/deploy?'));
  await page.getByRole('button', { name: 'Deploy', exact: true }).click();
  expect((await request).url()).toContain(`/environments/${sibling}/deploy`);
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
