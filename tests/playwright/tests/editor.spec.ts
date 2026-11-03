import { test, expect, auth, call, proof, webManifest, applicationTab, environmentTab, title, type Ceremony, type Managed } from '../fixtures.js';
import type { Page } from '@playwright/test';

/** Save buttons exist for every settings group; only the edited group's is shown. */
const visible = (page: Page, name: string) => page.getByRole('button', { name, exact: true }).locator('visible=true');

async function createApplication(page: Page, name = 'browser-test') {
  await page.goto('/dashboard/applications');
  await page.getByRole('button', { name: 'New application', exact: true }).click();
  await page.getByLabel('Application name', { exact: true }).fill(name);
  await page.getByRole('button', { name: 'Create application', exact: true }).click();
  await expect(page).toHaveURL(/\/dashboard\/applications\/[^/]+$/);
  await expect(title(page)).toHaveText(name);
}
async function addService(page: Page, name: string, image: string) {
  await applicationTab(page, 'Services').click();
  await page.getByRole('button', { name: 'Add service', exact: true }).first().click();
  const dialog = page.getByRole('dialog', { name: 'Add service' });
  await dialog.getByLabel('Service name', { exact: true }).fill(name);
  await dialog.getByLabel('Container image', { exact: true }).fill(image);
  await dialog.getByRole('button', { name: 'Add service', exact: true }).click();
  await expect(dialog).toBeHidden();
  await expect(page.locator('a.list-row', { hasText: name })).toContainText(image);
}
async function save(page: Page) {
  await visible(page, 'Save changes').click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(visible(page, 'Save changes')).toHaveCount(0);
}

test('creates an application, saves a rename, and retains it after reload', async ({ page, account }) => {
  void account;
  await createApplication(page);
  await page.getByRole('button', { name: 'Rename application', exact: true }).click();
  await page.getByLabel('Application name', { exact: true }).fill('renamed-application');
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(title(page)).toHaveText('renamed-application');
  await page.reload();
  await expect(title(page)).toHaveText('renamed-application');
  await page.goto('/dashboard/applications');
  const row = page.locator('a.list-row', { hasText: 'renamed-application' });
  await expect(row).toContainText('Not deployed');
  await expect(row).toContainText('Never deployed');
});

test('adds a service and edits it on the service page', async ({ page, account }) => {
  void account;
  await createApplication(page);
  await addService(page, 'web', 'nginx:1.27');
  const row = page.locator('a.list-row', { hasText: 'web' });
  await expect(row).toContainText('1 replicas');
  await row.click();
  await expect(page).toHaveURL(/\/services\/web$/);
  await expect(title(page)).toHaveText('web');
  await expect(visible(page, 'Save changes')).toHaveCount(0);
  await page.getByLabel('Replicas', { exact: true }).fill('3');
  await expect(visible(page, 'Save changes')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Remove service', exact: true })).toBeDisabled();
  await save(page);
  await expect(page.getByRole('button', { name: 'Remove service', exact: true })).toBeEnabled();
  await page.reload();
  await expect(page.getByLabel('Replicas', { exact: true })).toHaveValue('3');
  await page.getByRole('tablist', { name: 'Service sections' }).getByRole('tab', { name: 'Environment', exact: true }).click();
  await page.getByRole('button', { name: 'Add variable', exact: true }).click();
  await page.getByLabel('Key', { exact: true }).fill('PORT');
  await page.getByLabel('Value', { exact: true }).fill('8080');
  await save(page);
  // A section's draft compares only its own fields, also after another section saved.
  const sections = page.getByRole('tablist', { name: 'Service sections' });
  await sections.getByRole('tab', { name: 'Source & scaling', exact: true }).click();
  await page.getByLabel('Replicas', { exact: true }).fill('4');
  await sections.getByRole('tab', { name: 'Environment', exact: true }).click();
  await page.getByLabel('Value', { exact: true }).fill('9090');
  await visible(page, 'Save changes').click();
  await expect(visible(page, 'Save changes')).toHaveCount(0);
  await sections.getByRole('tab', { name: 'Source & scaling', exact: true }).click();
  await page.getByLabel('Replicas', { exact: true }).fill('3');
  await expect(visible(page, 'Save changes')).toHaveCount(0);
  await page.locator('.breadcrumb').getByRole('link', { name: 'browser-test', exact: true }).click();
  await expect(page.locator('a.list-row', { hasText: 'web' })).toContainText('3 replicas');
});

test('renames a service from its page and opens the renamed page', async ({ page, account }) => {
  void account;
  await createApplication(page);
  await addService(page, 'web', 'nginx:1.27');
  await page.locator('a.list-row', { hasText: 'web' }).click();
  // Renaming remounts the page, so other unsaved edits block it.
  const rename = page.getByRole('button', { name: 'Rename service', exact: true });
  await page.getByLabel('Replicas', { exact: true }).fill('3');
  await expect(rename).toBeDisabled();
  await visible(page, 'Discard').click();
  await rename.click();
  await page.getByLabel('Service name', { exact: true }).fill('frontend');
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page).toHaveURL(/\/services\/frontend$/);
  await expect(title(page)).toHaveText('frontend');
  await page.reload();
  await expect(title(page)).toHaveText('frontend');
});

test('mounts an inline file whose content is kept verbatim', async ({ page, account }) => {
  void account;
  await createApplication(page);
  await addService(page, 'web', 'nginx:1.27');
  await page.locator('a.list-row', { hasText: 'web' }).click();
  await page.getByRole('tablist', { name: 'Service sections' }).getByRole('tab', { name: 'Files', exact: true }).click();
  await page.getByRole('button', { name: 'Add file', exact: true }).click();
  await page.getByLabel('Container path', { exact: true }).fill('/etc/prometheus/prometheus.yml');
  await page.getByLabel('Mode', { exact: true }).fill('0640');
  await page.getByLabel('Content', { exact: true }).fill('target: ${{ vars.missing }} {{ .x }}');
  await save(page);
  await page.reload();
  await page.getByRole('tablist', { name: 'Service sections' }).getByRole('tab', { name: 'Files', exact: true }).click();
  await expect(page.getByLabel('Mode', { exact: true })).toHaveValue('0640');
  await expect(page.getByLabel('Content', { exact: true })).toHaveValue('target: ${{ vars.missing }} {{ .x }}');
});

test('saves volumes and routes, previews the plan, and records a deployment', async ({ page, account }) => {
  void account;
  await createApplication(page);
  await addService(page, 'web', 'nginx:1.27');
  await applicationTab(page, 'Volumes').click();
  await page.getByRole('button', { name: 'Add volume', exact: true }).click();
  await page.getByLabel('Volume name', { exact: true }).fill('data');
  await save(page);
  await applicationTab(page, 'Routes').click();
  await page.getByRole('button', { name: 'Add route', exact: true }).click();
  await page.getByLabel('Hostname', { exact: true }).fill('shop.example.com');
  await page.getByLabel('Name', { exact: true }).fill('shop');
  await page.getByRole('combobox', { name: 'Service', exact: true }).selectOption('web');
  await page.getByLabel('HTTP port', { exact: true }).fill('8080');
  await save(page);
  await page.reload();
  await applicationTab(page, 'Routes').click();
  await expect(page.getByLabel('Hostname', { exact: true })).toHaveValue('shop.example.com');
  await expect(page.getByLabel('Name', { exact: true })).toHaveValue('shop');

  await applicationTab(page, 'Environments').click();
  await page.getByRole('button', { name: 'New environment', exact: true }).click();
  const creator = page.getByRole('dialog', { name: 'Create environment' });
  await creator.getByLabel('Environment name', { exact: true }).fill('staging');
  // Read the refusal as it passes through, since the browser need not keep
  // its body once the dashboard has handled it.
  let conflict: { status: number; body: unknown } | undefined;
  const creation = /\/environments(?:\?|$)/;
  await page.route(creation, async route => {
    if (route.request().method() !== 'POST') {
      return route.fallback();
    }
    const response = await route.fetch();
    conflict = { status: response.status(), body: await response.json() };
    await route.fulfill({ response });
  });
  await creator.getByRole('button', { name: 'Create environment', exact: true }).click();
  await expect.poll(() => conflict).toMatchObject({
    status: 409,
    body: { code: 'hostname_conflict', details: { hostname: 'shop.example.com', environment: 'production' } },
  });
  await page.unroute(creation);
  await expect(creator).toContainText('is reserved by environment production of this application');
  await expect(creator).toContainText('give each environment its own hostname');
  await creator.getByRole('button', { name: 'Close dialog' }).click();

  await page.getByRole('button', { name: 'Preview', exact: true }).click();
  const preview = page.getByRole('dialog', { name: 'Deployment preview' });
  await expect(preview).toBeVisible();
  await expect(preview).toContainText('volumes.data');
  await expect(preview).toContainText('services.web.source');
  await expect(preview).toContainText('resolve image');
  await page.keyboard.press('Escape');
  await expect(preview).toBeHidden();

  await page.locator('.detail-head').getByRole('button', { name: 'Deploy to production', exact: true }).click();
  await expect(environmentTab(page, 'Deployments')).toHaveAttribute('aria-selected', 'true');
  const deployment = page.locator('.expander', { hasText: 'Deployment #' });
  await expect(deployment).toBeVisible();
  await deployment.getByRole('button', { name: /Deployment #/ }).click();
  await expect(deployment.getByText('Operation ID', { exact: true })).toBeVisible();
  await deployment.getByRole('tab', { name: 'Snapshot', exact: true }).click();
  await expect(deployment).toContainText('nginx:1.27');
  await page.goto('/dashboard/');
  await expect(page.getByRole('heading', { name: 'Recent deployments' })).toBeVisible();
  await expect(page.locator('.table', { hasText: 'browser-test' })).toContainText('requested');
});

test('adds, reorders, and removes jobs', async ({ page, account }) => {
  void account;
  await createApplication(page);
  await addService(page, 'web', 'nginx:1.27');
  await applicationTab(page, 'Jobs').click();
  await expect(page.getByText('No jobs. Deployments roll out services directly.', { exact: true })).toBeVisible();
  const job = (index: number) => page.locator(`[data-job="${index}"]`);
  const fill = async (index: number, name: string, command: string[]) => {
    await page.getByRole('button', { name: 'Add job', exact: true }).click();
    await job(index).getByLabel('Name', { exact: true }).fill(name);
    await job(index).getByRole('combobox', { name: 'Service', exact: true }).selectOption('web');
    for (const [element, value] of command.entries()) {
      if (element > 0) await job(index).getByRole('button', { name: 'Add command element', exact: true }).click();
      await job(index).getByLabel('Command element', { exact: true }).nth(element).fill(value);
    }
  };
  await fill(0, 'seed', ['seed']);
  await fill(1, 'migrate', ['migrate', '--all']);
  await job(0).getByRole('button', { name: 'Add variable', exact: true }).click();
  await job(0).getByLabel('Key', { exact: true }).fill('SEED_PASSWORD_FILE');
  await job(0).getByLabel('Value', { exact: true }).fill('/run/secrets/seed');
  await job(0).getByRole('button', { name: 'Add secret file', exact: true }).click();
  await job(0).getByLabel('Secret name', { exact: true }).fill('seed-password');
  await job(0).getByLabel('Container path', { exact: true }).fill('/run/secrets/seed');
  await job(1).getByLabel('Timeout (seconds)', { exact: true }).fill('60');
  await job(1).getByLabel('Stop service first', { exact: true }).check();
  await job(1).getByRole('button', { name: 'Move up', exact: true }).click();
  await save(page);
  await page.reload();
  await applicationTab(page, 'Jobs').click();
  await expect(job(0).getByLabel('Name', { exact: true })).toHaveValue('migrate');
  await expect(job(0).getByLabel('Command element', { exact: true }).nth(1)).toHaveValue('--all');
  await expect(job(0).getByLabel('Timeout (seconds)', { exact: true })).toHaveValue('60');
  await expect(job(0).getByLabel('Stop service first', { exact: true })).toBeChecked();
  await expect(job(1).getByLabel('Stop service first', { exact: true })).not.toBeChecked();
  await expect(job(1).getByLabel('Name', { exact: true })).toHaveValue('seed');
  await expect(job(1).getByLabel('Timeout (seconds)', { exact: true })).toHaveValue('300');
  await expect(job(1).getByLabel('Key', { exact: true })).toHaveValue('SEED_PASSWORD_FILE');
  await expect(job(1).getByLabel('Container path', { exact: true })).toHaveValue('/run/secrets/seed');
  await expect(job(0).getByLabel('Key', { exact: true })).toHaveCount(0);

  await job(1).getByRole('button', { name: 'Remove', exact: true }).first().click();
  await save(page);
  await page.reload();
  await applicationTab(page, 'Jobs').click();
  await expect(job(0).getByLabel('Name', { exact: true })).toHaveValue('migrate');
  await expect(job(1)).toHaveCount(0);

  await page.getByRole('button', { name: 'Deploy to production', exact: true }).click();
  const deployment = page.locator('.expander', { hasText: 'Deployment #' });
  await deployment.getByRole('button', { name: /Deployment #/ }).click();
  await deployment.getByRole('tab', { name: 'Snapshot', exact: true }).click();
  await expect(deployment).toContainText('migrate on web, up to 60s: migrate --all');
});

test('declares, changes, and removes generated secrets', async ({ page, account }) => {
  void account;
  await createApplication(page);
  await applicationTab(page, 'Secrets').click();
  await expect(page.getByText('No generated secrets declared.', { exact: true })).toBeVisible();
  const secret = (index: number) => page.locator(`[data-generated-secret="${index}"]`);
  const add = async (index: number, name: string) => {
    await page.getByRole('button', { name: 'Add generated secret', exact: true }).click();
    await secret(index).getByLabel('Secret name', { exact: true }).fill(name);
  };
  await add(0, 'database-password');
  await secret(0).getByLabel('Bytes', { exact: true }).fill('64');
  await secret(0).getByRole('combobox', { name: 'Encoding', exact: true }).selectOption('base64url');
  await add(1, 'signing-key');
  await secret(1).getByRole('combobox', { name: 'Generates', exact: true }).selectOption('rsa');
  await expect(secret(1).getByLabel('Bits', { exact: true })).toHaveValue('3072');
  await expect(secret(1).getByRole('combobox', { name: 'Encoding', exact: true })).toHaveCount(0);
  await save(page);
  await page.reload();
  await applicationTab(page, 'Secrets').click();
  await expect(secret(0).getByLabel('Secret name', { exact: true })).toHaveValue('database-password');
  await expect(secret(0).getByLabel('Bytes', { exact: true })).toHaveValue('64');
  await expect(secret(0).getByRole('combobox', { name: 'Encoding', exact: true })).toHaveValue('base64url');
  await expect(secret(1).getByLabel('Secret name', { exact: true })).toHaveValue('signing-key');
  await expect(secret(1).getByLabel('Bits', { exact: true })).toHaveValue('3072');

  // Validation rejects an out-of-range size and keeps the edit.
  await secret(0).getByLabel('Bytes', { exact: true }).fill('4');
  await visible(page, 'Save changes').click();
  await expect(page.getByRole('alert').getByText(/secret_generator_invalid/)).toBeVisible();
  await visible(page, 'Discard').click();

  await secret(0).getByRole('button', { name: 'Remove', exact: true }).click();
  await save(page);
  await page.reload();
  await applicationTab(page, 'Secrets').click();
  await expect(secret(0).getByLabel('Secret name', { exact: true })).toHaveValue('signing-key');
  await expect(secret(1)).toHaveCount(0);
});

test('unsaved edits block deployment and navigation until discarded', async ({ page, account }) => {
  void account;
  await createApplication(page);
  await applicationTab(page, 'Volumes').click();
  await page.getByRole('button', { name: 'Add volume', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Deploy to production', exact: true })).toBeDisabled();
  await expect(page.getByRole('button', { name: 'Preview', exact: true })).toBeDisabled();
  page.once('dialog', dialog => dialog.dismiss());
  await page.getByRole('complementary').getByRole('link', { name: 'Applications', exact: true }).click();
  await expect(page).toHaveURL(/\/dashboard\/applications\/[^/]+$/);
  await visible(page, 'Discard').click();
  await expect(page.getByRole('button', { name: 'Deploy to production', exact: true })).toBeEnabled();
});

test('a save whose response was lost is retried once and applied once', async ({ page, account }) => {
  void account;
  await createApplication(page);
  const id = page.url().split('/').pop()!;
  const { generation } = await call(page, 'GET', `applications/${id}`);
  // The first attempt reaches the daemon, but its response never arrives.
  const keys: string[] = [];
  await page.route(url => url.pathname === `/api/v1/applications/${id}/name`, async route => {
    keys.push(route.request().headers()['idempotency-key']);
    if (keys.length > 1) return route.continue();
    await route.fetch();
    await route.abort('failed');
  });
  await page.getByRole('button', { name: 'Rename application', exact: true }).click();
  await page.getByLabel('Application name', { exact: true }).fill('renamed-once');
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(title(page)).toHaveText('renamed-once');
  await expect(page.getByRole('alert')).toHaveCount(0);
  // The replay carries the same identity, so the daemon answers it without applying it again.
  expect(keys).toHaveLength(2);
  expect(keys[1]).toBe(keys[0]);
  expect(await call(page, 'GET', `applications/${id}`)).toMatchObject({ generation: generation + 1 });
});

test('unsaved edits guard back navigation, but not downloading the manifest', async ({ page, account }) => {
  void account;
  await createApplication(page);
  const editor = page.url();
  await applicationTab(page, 'Volumes').click();
  await page.getByRole('button', { name: 'Add volume', exact: true }).click();
  await page.getByLabel('Volume name', { exact: true }).fill('kept');
  const dialogs: string[] = [];
  page.on('dialog', dialog => { dialogs.push(dialog.message()); void dialog.dismiss(); });
  const download = page.waitForEvent('download');
  await page.getByRole('link', { name: 'Manifest', exact: true }).click();
  await download;
  expect(dialogs).toEqual([]);
  // Cancelling Back restores the editor's address, with its tab and edits.
  await page.goBack();
  await expect.poll(() => dialogs).toEqual([expect.stringContaining('discard unsaved form edits')]);
  await expect(page).toHaveURL(`${editor}?tab=volumes`);
  await expect(page.getByLabel('Volume name', { exact: true })).toHaveValue('kept');
  page.removeAllListeners('dialog');
  page.once('dialog', dialog => dialog.accept());
  await page.goBack();
  await expect(page).toHaveURL(/\/dashboard\/applications$/);
});

test('creating rejects an invalid name, and deleting needs confirmation', async ({ page, account }) => {
  void account;
  await page.goto('/dashboard/applications');
  await page.getByRole('button', { name: 'New application', exact: true }).click();
  const creator = page.getByRole('dialog', { name: 'Create application' });
  await creator.getByLabel('Application name', { exact: true }).fill('Not Valid');
  await creator.getByRole('button', { name: 'Create application', exact: true }).click();
  await expect(creator.getByRole('alert')).toBeVisible();
  await expect(page).toHaveURL(/\/dashboard\/applications$/);
  await creator.getByLabel('Application name', { exact: true }).fill('doomed');
  await creator.getByRole('button', { name: 'Create application', exact: true }).click();
  await expect(title(page)).toHaveText('doomed');
  const id = page.url().split('/').pop()!;

  const remove = page.getByRole('button', { name: 'Delete application', exact: true });
  page.once('dialog', dialog => dialog.dismiss());
  await remove.click();
  await expect(remove).toBeEnabled();
  page.once('dialog', async dialog => {
    // Its environments are named, and confirmed with the request.
    expect(dialog.message()).toContain('all its environments (production)');
    await dialog.accept();
  });
  const deleted = page.waitForResponse(response => response.request().method() === 'DELETE' && response.url().includes(`/applications/${id}?`));
  await remove.click();
  expect((await deleted).status()).toBe(202);
  await expect(page).toHaveURL(/\/dashboard\/applications$/);
  expect(await call(page, 'GET', `applications/${id}`)).toMatchObject({ delete_intent: true });
});

test('removes a service from its page, whose address then reports it gone', async ({ page, account }) => {
  void account;
  const image = { type: 'image', image: 'nginx:alpine' };
  const saved = await call(page, 'POST', 'applications/apply', {
    manifest: webManifest('services', { services: [{ name: 'web', source: image }, { name: 'worker', source: image }] }),
    expected_generation: 0,
  });
  await page.goto(`/dashboard/applications/${saved.application_id}/services/web`);
  const remove = page.getByRole('button', { name: 'Remove service', exact: true });
  page.once('dialog', dialog => dialog.dismiss());
  await remove.click();
  await expect(title(page)).toHaveText('web');
  page.once('dialog', dialog => dialog.accept());
  await remove.click();
  await expect(page).toHaveURL(url => url.pathname.endsWith(`/applications/${saved.application_id}`) && url.searchParams.get('tab') === 'services');
  await expect(page.locator('a.list-row')).toHaveText([/^worker/]);
  await page.goBack();
  await expect(page.getByText('Service web is not part of the saved configuration.', { exact: true })).toBeVisible();
  await page.getByRole('link', { name: 'Back to services', exact: true }).click();
  await expect(applicationTab(page, 'Services')).toHaveAttribute('aria-selected', 'true');
});

test('saves exports and imports from the Links tab, each card on its own', async ({ page, account }) => {
  void account;
  const saved = await call(page, 'POST', 'applications/apply', { manifest: webManifest('links'), expected_generation: 0 });
  const id = saved.application_id as string;
  await page.goto(`/dashboard/applications/${id}?tab=links`);
  const card = (name: string) => page.locator('section.card').filter({ has: page.getByRole('heading', { name, exact: true }) });
  await page.getByRole('button', { name: 'Add export', exact: true }).click();
  const exported = card('Exports');
  await exported.getByRole('combobox', { name: 'Service', exact: true }).selectOption('web');
  await exported.getByLabel('Applications', { exact: true }).fill(' notes ,, billing ');
  await exported.getByLabel('Previews too', { exact: true }).check();
  await page.getByRole('button', { name: 'Add import', exact: true }).click();
  const imported = card('Imports');
  await imported.getByLabel('Application', { exact: true }).fill('auth');
  await imported.getByLabel('Service', { exact: true }).fill('api');

  // Saving the exports keeps the imports' unsaved edits.
  await exported.getByRole('button', { name: 'Save changes', exact: true }).click();
  await expect(exported.getByRole('button', { name: 'Save changes', exact: true })).toBeHidden();
  await expect(imported.getByLabel('Application', { exact: true })).toHaveValue('auth');
  await imported.getByRole('button', { name: 'Save changes', exact: true }).click();
  await expect(imported.getByRole('button', { name: 'Save changes', exact: true })).toBeHidden();

  const { application } = await call(page, 'GET', `applications/${id}`);
  expect(application.spec.exports).toEqual([{ service: 'web', applications: ['notes', 'billing'], previews: true }]);
  // An empty environment reaches the importing environment's namesake.
  expect(application.spec.imports).toEqual([{ application: 'auth', service: 'api' }]);
  await page.reload();
  await expect(exported.getByLabel('Applications', { exact: true })).toHaveValue('notes, billing');
  await expect(imported.getByLabel('Environment', { exact: true })).toHaveValue('');
});

test('reloading after a change made elsewhere keeps unsaved edits and shows the change', async ({ page, account }) => {
  void account;
  await createApplication(page);
  const id = page.url().split('/').pop()!;
  await applicationTab(page, 'Volumes').click();
  await page.getByRole('button', { name: 'Add volume', exact: true }).click();
  await page.getByLabel('Volume name', { exact: true }).fill('kept');
  await call(page, 'PUT', `applications/${id}/name?force=true`, { value: 'renamed-elsewhere' });
  const reload = page.getByRole('button', { name: 'Reload saved configuration', exact: true });
  await expect(reload).toBeVisible({ timeout: 20_000 });
  await reload.click();
  await expect(title(page)).toHaveText('renamed-elsewhere');
  await expect(page.getByLabel('Volume name', { exact: true })).toHaveValue('kept');
  await save(page);
});

test('reauthenticates after revocation without losing an unsaved editor draft', async ({ page, account }) => {
  await createApplication(page);
  await page.getByRole('button', { name: 'Rename application', exact: true }).click();
  await page.getByLabel('Application name', { exact: true }).fill('preserved-draft');
  await auth(page, 'manage', { action: 'revoke_all', user_id: account.id });
  // The dashboard's next refresh detects revocation without an explicit reload.
  const dialog = page.getByRole('dialog');
  await expect(dialog).toBeVisible({ timeout: 20_000 });
  await dialog.getByRole('button', { name: 'Sign in with a passkey', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(page.getByLabel('Application name', { exact: true })).toHaveValue('preserved-draft');
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(title(page)).toHaveText('preserved-draft');
});

test('signing in as another account after the session expired drops the previous drafts', async ({ page, account }) => {
  await createApplication(page);
  await page.getByRole('button', { name: 'Rename application', exact: true }).click();
  await page.getByLabel('Application name', { exact: true }).fill('alice-draft');
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [{ permission: 'admin' }] })).invitation_url;
  await auth(page, 'manage', { action: 'revoke_all', user_id: account.id });
  const dialog = page.getByRole('dialog');
  await expect(dialog).toBeVisible({ timeout: 20_000 });
  // Bob registers and signs in elsewhere in this browser, then continues here.
  const ceremony = await auth<Ceremony>(page, 'register/start', {
    invitation: link.split('#invite=')[1], user_id: null, username: 'bob', display_name: '', passkey_name: 'Test',
  });
  await auth(page, 'register/finish', await proof(page, ceremony, true));
  await dialog.getByRole('button', { name: 'Continue', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(title(page)).toHaveText('browser-test');
  await expect(page.getByLabel('Application name', { exact: true })).toHaveValue('browser-test');
});
