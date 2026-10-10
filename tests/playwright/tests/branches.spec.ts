import { test, expect } from '../fixtures.js';
import type { Page, Route } from '@playwright/test';

/** A repository-backed application following `main`, optionally with a `staging` environment on `release`. */
async function createRepositoryApplication(page: Page, staging = false) {
  return page.evaluate(async (staging) => {
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
        metadata: { name: 'notes' },
        spec: {
          manifest: { path: 'app.toml', repository: { url: 'https://example.com/notes.git', branch: 'main' } },
          services: [{ name: 'web', source: { type: 'image', image: 'nginx:alpine' } }],
        },
      },
      expected_generation: 0,
    });
    const sibling = staging
      ? await call(`applications/${saved.application_id}/environments`, {
        name: 'staging', branch: 'release', expected_generation: saved.generation,
      })
      : null;
    return { app: saved.application_id as string, staging: sibling?.id as string };
  }, staging);
}

/** Rewrites the JSON `data` of GET responses whose path is exactly `path`. */
async function rewrite(page: Page, path: string, edit: (data: Record<string, any>) => void) {
  await page.route(url => url.pathname === path, async (route: Route) => {
    if (route.request().method() !== 'GET') return route.continue();
    const response = await route.fetch();
    const body = await response.json();
    edit(body.data);
    await route.fulfill({ response, json: body });
  });
}

const applicationTab = (page: Page, name: string) =>
  page.getByRole('navigation', { name: 'Application sections' }).getByRole('button', { name, exact: true });
const environmentTab = (page: Page, name: string) =>
  page.getByRole('navigation', { name: 'Environment sections' }).getByRole('button', { name, exact: true });
const title = (page: Page) => page.locator('.detail-title h1');

test('each environment of a repository-backed application follows its own branch', async ({ page, account }) => {
  void account;
  const { app } = await createRepositoryApplication(page);
  await page.goto(`/dashboard/applications/${app}?tab=environments`);
  await page.getByRole('button', { name: 'New environment', exact: true }).click();
  const creator = page.getByRole('dialog', { name: 'Create environment' });
  // New environments default to the branch `spec.manifest` names.
  await expect(creator.getByLabel('Branch', { exact: true })).toHaveValue('main');
  await creator.getByLabel('Environment name', { exact: true }).fill('staging');
  await creator.getByLabel('Branch', { exact: true }).fill('release');
  await creator.getByRole('button', { name: 'Create environment', exact: true }).click();
  await expect(title(page)).toHaveText('staging');
  const staging = new URL(page.url()).pathname.split('/').pop()!;

  const branch = page.getByLabel('Branch', { exact: true });
  const change = page.getByRole('button', { name: 'Change branch', exact: true });
  await expect(branch).toHaveValue('release');
  await expect(change).toBeDisabled();
  // Surrounding whitespace names the same branch.
  await branch.fill(' release ');
  await expect(change).toBeDisabled();
  await branch.fill('next');
  const changed = page.waitForResponse(response => response.request().method() === 'PUT' && response.url().includes(`/environments/${staging}/branch`));
  await change.click();
  expect((await changed).status()).toBe(200);
  await expect(change).toBeDisabled();

  await page.goto(`/dashboard/applications/${app}?tab=environments`);
  const environments = page.getByRole('list', { name: 'Environments' });
  await expect(environments.getByRole('listitem').filter({ hasText: 'staging' })).toContainText('branch next');
  await expect(environments.getByRole('listitem').filter({ hasText: 'production' })).toContainText('branch main');
});

test('an environment page reads the manifest last fetched from its branch', async ({ page, account }) => {
  void account;
  const { app, staging } = await createRepositoryApplication(page, true);
  await page.goto(`/dashboard/applications/${app}/environments/${staging}`);
  await expect(title(page)).toHaveText('staging');
  // Nothing has been fetched from `release` yet.
  const preview = page.getByRole('button', { name: 'Preview', exact: true });
  await expect(preview).toBeDisabled();
  await expect(preview).toHaveAttribute('title', 'Deploy this environment to fetch its manifest from its branch first');
  await expect(page.getByText('Deploy this environment to fetch its manifest from its branch.', { exact: true })).toBeVisible();

  // Model a fetch from `release`, whose manifest runs `worker` instead of the
  // `web` service the application last fetched, and that warned on deploying.
  await rewrite(page, `/api/v1/environments/${staging}/detail`, data => {
    data.manifest = structuredClone(data.application.application);
    data.manifest.spec.services[0].name = 'worker';
  });
  await rewrite(page, `/api/v1/environments/${staging}/deployments`, data => {
    for (const deployment of data.items) {
      deployment.warnings = [{ code: 'manifest_connection_ignored', message: 'The fetched manifest names another repository.' }];
    }
  });
  const deployed = page.waitForResponse(response => response.request().method() === 'POST' && response.url().includes(`/environments/${staging}/deploy?`));
  await page.locator('.detail-head').getByRole('button', { name: 'Deploy to staging', exact: true }).click();
  expect((await deployed).status()).toBe(202);
  await page.reload();

  await expect(preview).toBeEnabled();
  const planned = page.waitForRequest(request => request.method() === 'POST' && request.url().includes('/applications/plan'));
  await preview.click();
  expect((await planned).postDataJSON().manifest.spec.services.map((service: { name: string }) => service.name)).toEqual(['worker']);
  const dialog = page.getByRole('dialog', { name: 'Deployment preview' });
  await expect(dialog).toContainText('services.worker');
  await page.keyboard.press('Escape');
  await expect(dialog).toBeHidden();

  await environmentTab(page, 'Logs').click();
  const service = page.getByRole('combobox', { name: 'Service', exact: true });
  await expect(service.locator('option')).toHaveText(['All services', 'worker']);

  await environmentTab(page, 'Deployments').click();
  const deployment = page.locator('.expander', { hasText: 'Deployment #' });
  await deployment.getByRole('button', { name: /Deployment #/ }).click();
  await deployment.getByRole('button', { name: 'Snapshot', exact: true }).click();
  await expect(deployment).toContainText('manifest_connection_ignored: The fetched manifest names another repository.');
});

test('a repository-backed application page reloads what environments fetch', async ({ page, account }) => {
  void account;
  const { app } = await createRepositoryApplication(page);
  await page.clock.install();
  await page.goto(`/dashboard/applications/${app}`);
  await applicationTab(page, 'Volumes').click();
  await expect(page.getByLabel('Volume name', { exact: true })).toHaveCount(0);

  // Model a fetch that added a volume: the saved manifest changes, its
  // revision does not, and the polled listing reports the newer update.
  let generation = 0;
  await rewrite(page, '/api/v1/applications', data => {
    for (const row of data.items.filter((row: { id: string }) => row.id === app)) {
      row.updated_at_ms += 1_000;
      row.generation += generation;
    }
  });
  await rewrite(page, `/api/v1/applications/${app}`, data => {
    data.application.spec.volumes = [{ name: 'cache' }];
    data.spec_hash = 'fetched';
    data.updated_at_ms += 1_000;
  });
  await page.clock.runFor(16_000);
  // The read-only forms are rebuilt from the fetched manifest.
  await expect(page.getByLabel('Volume name', { exact: true })).toHaveValue('cache');
  const conflict = page.getByText('Configuration changed elsewhere.', { exact: false });
  await expect(conflict).toHaveCount(0);

  // A newer revision is someone else's edit: it is reported, not adopted.
  generation = 1;
  await page.clock.runFor(16_000);
  await expect(conflict).toBeVisible();
});

test('a preview created from the dashboard opens on its own page, also from its row, and is deleted from it', async ({ page, account }) => {
  void account;
  const { app } = await createRepositoryApplication(page);
  await page.goto(`/dashboard/applications/${app}?tab=previews`);
  await page.getByRole('button', { name: 'New preview', exact: true }).click();
  const creator = page.getByRole('dialog', { name: 'Create preview' });
  await creator.getByLabel('Branch', { exact: true }).fill('feat/login');
  await creator.getByLabel('Slot (optional)', { exact: true }).fill('agent-2');
  await creator.getByRole('button', { name: 'Create preview', exact: true }).click();
  // The page is named by the slug and opens on the deployment it started.
  await expect(title(page)).toHaveText('notes-feat-login-agent-2-503aa8');
  await expect(environmentTab(page, 'Deployments')).toHaveAttribute('aria-current', 'page');
  const preview = new URL(page.url()).pathname.split('/').pop()!;
  expect(preview).toMatch(/^preview-/);
  // Its whole row in the list opens it too.
  await page.goto(`/dashboard/applications/${app}?tab=previews`);
  await page.getByRole('list', { name: 'Previews' }).getByRole('listitem').click();
  await expect(page).toHaveURL(`/dashboard/applications/${app}/previews/${preview}`);

  await environmentTab(page, 'Overview').click();
  await expect(page.getByText('feat/login', { exact: true })).toBeVisible();
  page.once('dialog', dialog => dialog.accept());
  const deleted = page.waitForResponse(response => response.request().method() === 'DELETE' && response.url().endsWith(`/previews/${preview}`));
  await page.getByRole('button', { name: 'Delete preview', exact: true }).click();
  expect((await deleted).status()).toBe(202);
  await expect(page).toHaveURL(`/dashboard/applications/${app}?tab=previews`);
});

test('Prune gone submits the previews whose branch is gone', async ({ page, account }) => {
  void account;
  const { app } = await createRepositoryApplication(page);
  const preview = await page.evaluate(async (app) => {
    const response = await fetch(`/api/v1/applications/${app}/previews`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ branch: 'old/experiment' }),
    });
    return (await response.json()).data.preview.id as string;
  }, app);
  // The test repository cannot be read, so the listing reports the branch gone.
  await page.route(`**/api/v1/applications/${app}/previews`, async route => {
    const response = await route.fetch();
    const body = await response.json();
    for (const view of body.data) view.branch = { state: 'gone' };
    await route.fulfill({ response, json: body });
  });
  const pruned = page.waitForRequest(request => request.url().endsWith(`/applications/${app}/previews/prune`));
  await page.route(`**/api/v1/applications/${app}/previews/prune`, route => route.fulfill({ json: { data: [] } }));

  await page.goto(`/dashboard/applications/${app}?tab=previews`);
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Prune gone', exact: true }).click();
  expect((await pruned).postDataJSON()).toEqual({ previews: [preview] });
});

test('an application deploys on push by polling or webhook, and an environment opts in', async ({ page, account }) => {
  void account;
  const { app } = await createRepositoryApplication(page);
  await page.goto(`/dashboard/applications/${app}?tab=source`);
  const mode = page.getByRole('combobox', { name: 'Deploy on push', exact: true });
  const save = page.getByRole('button', { name: 'Save changes', exact: true });
  const saved = () => page.waitForRequest(request => request.method() === 'PUT' && new URL(request.url()).pathname === `/api/v1/applications/${app}/repository`);
  await mode.selectOption('poll');
  await page.getByLabel('Poll interval (seconds)', { exact: true }).fill('600');
  let request = saved();
  await save.click();
  expect((await request).postDataJSON().value.sync).toEqual({ mode: 'poll', interval_seconds: 600 });
  await expect(save).toBeHidden();
  await expect(page.getByText('Last sync check', { exact: true })).toBeVisible();

  await mode.selectOption('webhook');
  request = saved();
  await save.click();
  expect((await request).postDataJSON().value.sync).toEqual({ mode: 'webhook' });
  const webhook = page.locator('section.card', { has: page.getByRole('heading', { name: 'GitHub webhook' }) });
  // The fixture daemon sets no `ingress.webhook_hostname`.
  await expect(webhook).toContainText('set ingress.webhook_hostname');
  await expect(webhook).toContainText('None yet');
  const generated = page.waitForResponse(response => response.url().endsWith(`/applications/${app}/webhook/secret`));
  await webhook.getByRole('button', { name: 'Generate secret', exact: true }).click();
  const { secret } = (await (await generated).json()).data;
  await expect(webhook.locator('.secret-box')).toHaveText(secret);
  // It copies to the clipboard in one click.
  await page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
  const copy = webhook.getByRole('button', { name: 'Copy secret', exact: true });
  await copy.click();
  await expect(copy).toHaveText('Copied');
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(secret);
  await expect(webhook.getByRole('button', { name: 'Rotate secret', exact: true })).toBeVisible();
  // It is shown only once.
  await page.reload();
  await expect(webhook).toContainText('Generated');
  await expect(webhook.locator('.secret-box')).toHaveCount(0);

  await page.goto(`/dashboard/applications/${app}?tab=environments`);
  await page.getByRole('list', { name: 'Environments' }).getByRole('link', { name: 'production' }).click();
  const source = page.locator('section.card', { has: page.getByRole('heading', { name: 'Source' }) });
  // Environments opt in, then follow from their next deployment.
  await expect(source).toContainText('not opted in');
  const optedIn = page.waitForRequest(request => request.method() === 'PUT' && request.url().endsWith('/sync'));
  await source.getByRole('button', { name: 'Deploy on push', exact: true }).click();
  expect((await optedIn).postDataJSON()).toEqual({ enabled: true });
  await expect(source).toContainText('waiting for a deployment');
  await expect(source.getByRole('button', { name: 'Stop deploying on push', exact: true })).toBeVisible();
});
