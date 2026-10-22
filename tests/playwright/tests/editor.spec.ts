import { test, expect, auth, call, proof, applicationTab, environmentTab, title, type Ceremony, type Managed } from '../fixtures.js';
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
  await page.getByRole('navigation', { name: 'Service sections' }).getByRole('button', { name: 'Environment', exact: true }).click();
  await page.getByRole('button', { name: 'Add variable', exact: true }).click();
  await page.getByLabel('Key', { exact: true }).fill('PORT');
  await page.getByLabel('Value', { exact: true }).fill('8080');
  await save(page);
  // A section's draft compares only its own fields, also after another section saved.
  const sections = page.getByRole('navigation', { name: 'Service sections' });
  await sections.getByRole('button', { name: 'Source & scaling', exact: true }).click();
  await page.getByLabel('Replicas', { exact: true }).fill('4');
  await sections.getByRole('button', { name: 'Environment', exact: true }).click();
  await page.getByLabel('Value', { exact: true }).fill('9090');
  await visible(page, 'Save changes').click();
  await expect(visible(page, 'Save changes')).toHaveCount(0);
  await sections.getByRole('button', { name: 'Source & scaling', exact: true }).click();
  await page.getByLabel('Replicas', { exact: true }).fill('3');
  await expect(visible(page, 'Save changes')).toHaveCount(0);
  await page.locator('.breadcrumb').getByRole('link', { name: 'browser-test', exact: true }).click();
  await expect(page.locator('a.list-row', { hasText: 'web' })).toContainText('3 replicas');
});

test('mounts an inline file whose content is kept verbatim', async ({ page, account }) => {
  void account;
  await createApplication(page);
  await addService(page, 'web', 'nginx:1.27');
  await page.locator('a.list-row', { hasText: 'web' }).click();
  await page.getByRole('navigation', { name: 'Service sections' }).getByRole('button', { name: 'Files', exact: true }).click();
  await page.getByRole('button', { name: 'Add file', exact: true }).click();
  await page.getByLabel('Container path', { exact: true }).fill('/etc/prometheus/prometheus.yml');
  await page.getByLabel('Mode', { exact: true }).fill('0640');
  await page.getByLabel('Content', { exact: true }).fill('target: ${{ vars.missing }} {{ .x }}');
  await save(page);
  await page.reload();
  await page.getByRole('navigation', { name: 'Service sections' }).getByRole('button', { name: 'Files', exact: true }).click();
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
  await expect(environmentTab(page, 'Deployments')).toHaveAttribute('aria-current', 'page');
  const deployment = page.locator('.expander', { hasText: 'Deployment #' });
  await expect(deployment).toBeVisible();
  await deployment.getByRole('button', { name: /Deployment #/ }).click();
  await expect(deployment.getByText('Operation ID', { exact: true })).toBeVisible();
  await deployment.getByRole('button', { name: 'Snapshot', exact: true }).click();
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
  await job(1).getByRole('button', { name: 'Move up', exact: true }).click();
  await save(page);
  await page.reload();
  await applicationTab(page, 'Jobs').click();
  await expect(job(0).getByLabel('Name', { exact: true })).toHaveValue('migrate');
  await expect(job(0).getByLabel('Command element', { exact: true }).nth(1)).toHaveValue('--all');
  await expect(job(0).getByLabel('Timeout (seconds)', { exact: true })).toHaveValue('60');
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
  await deployment.getByRole('button', { name: 'Snapshot', exact: true }).click();
  await expect(deployment).toContainText('migrate on web, up to 60s: migrate --all');
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
