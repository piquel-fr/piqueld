import { test, expect, call, card, webManifest, applicationTab, environmentTab } from '../fixtures.js';
import type { Page, Request } from '@playwright/test';

/** Whether `request` is a `method` request for exactly `path`. */
const sent = (request: Request, method: string, path: string) =>
  request.method() === method && new URL(request.url()).pathname === `/api/v1/${path}`;

/** Accepts the next confirmation once its message contains `text`. */
function confirmNext(page: Page, text: string) {
  page.once('dialog', async dialog => {
    expect(dialog.message()).toContain(text);
    await dialog.accept();
  });
}

test('replacing a stored value keeps an access list changed elsewhere, and access saves on its own', async ({ page, account }) => {
  void account;
  const saved = await call(page, 'POST', 'applications/apply', { manifest: webManifest('vault'), expected_generation: 0 });
  const app = saved.application_id as string;
  const path = `applications/${app}/secrets/api-token`;
  await call(page, 'PUT', `${path}?environments=${app}`, 'first', { 'X-Expected-Generation': '0' });
  await page.goto(`/dashboard/applications/${app}`);
  await applicationTab(page, 'Secrets').click();
  const store = card(page, 'Secret store');
  const row = store.getByRole('row').filter({ hasText: 'api-token' });
  await expect(row).toContainText('production');

  // Editing loads the secret's access list.
  await row.getByRole('button', { name: 'Edit', exact: true }).click();
  const every = store.getByLabel('Every environment, including ones created later', { exact: true });
  await expect(every).not.toBeChecked();
  await expect(store.getByLabel('production', { exact: true })).toBeChecked();

  // Declining the replacement sends nothing and keeps the typed value.
  const value = store.getByLabel('Value', { exact: true });
  const save = store.getByRole('button', { name: 'Save secret', exact: true });
  await value.fill('second');
  const writes: string[] = [];
  const record = (request: Request) => { if (sent(request, 'PUT', path)) writes.push(request.url()); };
  page.on('request', record);
  page.once('dialog', dialog => dialog.dismiss());
  await save.click();
  await expect(value).toHaveValue('second');
  page.off('request', record);
  expect(writes).toEqual([]);

  // Every environment may mount it since the list was loaded; the form
  // didn't change access, so the replacement keeps that.
  await call(page, 'PUT', `${path}/access`, { environments: 'all', previews: false });
  confirmNext(page, 'Replace api-token?');
  const replaced = page.waitForRequest(request => sent(request, 'PUT', path));
  await save.click();
  expect(new URL((await replaced).url()).searchParams.has('environments')).toBe(false);
  await expect(value).toHaveValue('');
  await expect(row).toContainText('every environment');
  await expect(row.getByRole('cell').nth(1)).toHaveText('2');
  await expect(every).toBeChecked();

  // Saving access alone keeps the value's version.
  await every.uncheck();
  await store.getByLabel('production', { exact: true }).check();
  const restricted = page.waitForRequest(request => sent(request, 'PUT', `${path}/access`));
  await store.getByRole('button', { name: 'Save access only', exact: true }).click();
  await restricted;
  await expect(store.getByText('Access saved. It applies to later deployments.', { exact: true })).toBeVisible();
  await expect(row).not.toContainText('every environment');
  await expect(row.getByRole('cell').nth(1)).toHaveText('2');

  // Once deleted, the name no longer loads its access: a new secret of that
  // name starts from the default.
  confirmNext(page, 'Delete api-token from every environment');
  await row.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(store.getByText('Secret deleted.', { exact: true })).toBeVisible();
  await expect(row).toHaveCount(0);
  await expect(every).toBeChecked();
  await expect(store.getByRole('button', { name: 'Save access only', exact: true })).toBeDisabled();
});

test('a refused secret write clears the value and waits for a refresh', async ({ page, account }) => {
  void account;
  const saved = await call(page, 'POST', 'applications/apply', { manifest: webManifest('vault'), expected_generation: 0 });
  const app = saved.application_id as string;
  const path = `applications/${app}/secrets/api-token`;
  await call(page, 'PUT', `${path}?environments=all`, 'first', { 'X-Expected-Generation': '0' });
  await page.goto(`/dashboard/applications/${app}`);
  await applicationTab(page, 'Secrets').click();
  const store = card(page, 'Secret store');
  const row = store.getByRole('row').filter({ hasText: 'api-token' });
  await expect(row.getByRole('cell').nth(1)).toHaveText('1');

  // Replaced elsewhere, so the shown version is stale.
  await call(page, 'PUT', path, 'second', { 'X-Expected-Generation': '1' });
  await row.getByRole('button', { name: 'Edit', exact: true }).click();
  const value = store.getByLabel('Value', { exact: true });
  await value.fill('third');
  confirmNext(page, 'Replace api-token?');
  const refused = page.waitForResponse(response => sent(response.request(), 'PUT', path));
  await store.getByRole('button', { name: 'Save secret', exact: true }).click();
  expect((await refused).status()).toBe(409);
  await expect(store.getByText('Refresh before another secret change. Any submitted value has been cleared.', { exact: false })).toBeVisible();
  await expect(value).toHaveValue('');
  await expect(value).toBeDisabled();
  await expect(row.getByRole('button', { name: 'Edit', exact: true })).toBeDisabled();

  await store.getByRole('button', { name: 'Refresh', exact: true }).click();
  await expect(row.getByRole('cell').nth(1)).toHaveText('2');
  await expect(value).toBeEnabled();
  await expect(row.getByRole('button', { name: 'Edit', exact: true })).toBeEnabled();
});

test.describe('with deployments', () => {
  test.use({ fakeDocker: true });

  test('an environment regenerates a generated secret, and a refused deletion waits for a refresh', async ({ page, account }) => {
    void account;
    const manifest = webManifest('keys', {
      services: [{
        name: 'web',
        source: { type: 'image', image: 'nginx:alpine' },
        secrets: [{ name: 'session-key', target: '/run/secrets/session' }],
      }],
      secrets: [{ name: 'session-key', generate: { type: 'random', bytes: 32 } }],
    });
    const saved = await call(page, 'POST', 'applications/apply', { manifest, expected_generation: 0 });
    const app = saved.application_id as string;
    await call(page, 'POST', `environments/${app}/deploy?expected_generation=${saved.generation}`);
    // Its first deployment generates the value.
    await expect.poll(async () => (await call(page, 'GET', `environments/${app}/secrets`)).length, { timeout: 20_000 }).toBe(1);

    await page.goto(`/dashboard/applications/${app}/environments/${app}`);
    await environmentTab(page, 'Secrets').click();
    await expect(card(page, 'Mounted secrets').getByRole('row').filter({ hasText: 'session-key' })).toContainText('generated');
    const generated = card(page, 'Generated secrets');
    const row = generated.getByRole('row').filter({ hasText: 'session-key' });
    await expect(row.getByRole('cell').nth(1)).toHaveText('1');

    confirmNext(page, 'Generate a new value for session-key?');
    await row.getByRole('button', { name: 'Regenerate', exact: true }).click();
    await expect(page.getByText('New value generated. Deploy to use it.', { exact: true })).toBeVisible();
    await expect(row.getByRole('cell').nth(1)).toHaveText('2');

    // The running deployment still mounts it.
    confirmNext(page, 'Delete the generated value of session-key');
    const refused = page.waitForResponse(response => sent(response.request(), 'DELETE', `environments/${app}/secrets/session-key`));
    await row.getByRole('button', { name: 'Delete', exact: true }).click();
    expect((await refused).status()).toBe(409);
    await expect(page.getByText('Refresh, then retry if cleanup is pending.', { exact: false })).toBeVisible();
    await expect(row.getByRole('button', { name: 'Regenerate', exact: true })).toBeDisabled();
    await card(page, 'Mounted secrets').getByRole('button', { name: 'Refresh', exact: true }).click();
    await expect(row.getByRole('button', { name: 'Regenerate', exact: true })).toBeEnabled();
    await expect(row.getByRole('cell').nth(1)).toHaveText('2');
  });
});
