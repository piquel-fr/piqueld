import { test, expect, call, card, head, environmentId, resolveEnvironment, webManifest, applicationTab, environmentTab, title } from '../fixtures.js';
import type { Page, Request } from '@playwright/test';

/** `environments`, its `production` environment `env`, and a second
 * environment, `staging` or, to `collide`, one named after `env`'s ID. */
async function createEnvironments(page: Page, collide = false) {
  const saved = await call(page, 'POST', 'applications/apply', { manifest: webManifest('environments'), expected_generation: 0 });
  const app = saved.application_id as string;
  const env = await environmentId(page, app);
  const sibling = await call(page, 'POST', `applications/${app}/environments`, {
    name: collide ? env : 'staging',
    expected_generation: saved.generation,
  });
  return { app, env, sibling: sibling.id as string };
}

/** A repository-backed application following `main`, and its `production`
 * environment `env`. */
async function createRepositoryApplication(page: Page) {
  const saved = await call(page, 'POST', 'applications/apply', {
    manifest: webManifest('notes', {
      manifest: { path: 'app.toml', repository: { url: 'https://example.com/notes.git', branch: 'main' } },
    }),
    expected_generation: 0,
  });
  const app = saved.application_id as string;
  return { app, env: await environmentId(page, app), generation: saved.generation as number };
}

test('the application page stays whole and each environment has its own page', async ({ page, account }) => {
  void account;
  const { app, sibling } = await createEnvironments(page);
  await page.goto(`/dashboard/applications/${app}`);
  await expect(page.getByRole('combobox', { name: 'Environment', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Preview', exact: true })).toHaveCount(0);

  // With several environments, deploying starts by choosing one.
  await page.locator('.detail-head').getByRole('button', { name: 'Deploy', exact: true }).click();
  const chooser = page.getByRole('dialog', { name: 'Deploy an environment' });
  await expect(chooser.getByRole('button', { name: /^Deploy to / })).toHaveCount(2);
  await chooser.getByRole('button', { name: 'Close dialog', exact: true }).click();
  await applicationTab(page, 'Environments').click();
  const environments = page.getByRole('list', { name: 'Environments' });
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
  await expect(environmentTab(page, 'Deployments')).toHaveAttribute('aria-selected', 'true');
  await expect(title(page)).toHaveText('staging');
  await expect(page.locator('.detail-head').getByRole('button', { name: 'Deploy', exact: true })).toBeVisible();

  await page.reload();
  await expect(title(page)).toHaveText('staging');
  await page.getByRole('navigation', { name: 'Breadcrumb' }).getByRole('link', { name: 'environments', exact: true }).click();
  await expect(page).toHaveURL(url => url.pathname.endsWith(`/applications/${app}`));
  await expect(applicationTab(page, 'Environments')).toHaveAttribute('aria-selected', 'true');
});

test('the dashboard creates, renames and deletes only the chosen environment', async ({ page, account }) => {
  void account;
  const { app, env } = await createEnvironments(page);
  await page.goto(`/dashboard/applications/${app}?tab=environments`);
  await page.getByRole('button', { name: 'New environment', exact: true }).click();
  const creator = page.getByRole('dialog', { name: 'Create environment' });
  await creator.getByLabel('Environment name', { exact: true }).fill('qa');
  await creator.getByRole('button', { name: 'Create environment', exact: true }).click();
  await expect(title(page)).toHaveText('qa');
  await expect(page.locator('.detail-head').getByRole('button', { name: 'Deploy', exact: true })).toBeEnabled();
  const qa = new URL(page.url()).pathname.split('/').pop()!;
  expect(qa).not.toBe(env);

  await page.getByLabel('Environment name', { exact: true }).fill('preview');
  await page.getByRole('button', { name: 'Rename environment', exact: true }).click();
  await expect(title(page)).toHaveText('preview');
  await expect(page.locator('.detail-head').getByRole('button', { name: 'Deploy', exact: true })).toBeEnabled();

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
  await expect(applicationTab(page, 'Environments')).toHaveAttribute('aria-selected', 'true');
  const row = page.getByRole('listitem').filter({ hasText: 'preview' });
  await expect(row).toContainText('Deleting');
  await expect(row.getByRole('button', { name: 'Deploy to preview', exact: true })).toBeDisabled();
  await expect(page.getByRole('listitem').filter({ hasText: 'production' }).getByRole('button', { name: 'Deploy to production', exact: true })).toBeEnabled();

  // The whole row opens the environment, not only its name.
  await row.click({ position: { x: 400, y: 10 } });
  await expect(title(page)).toHaveText('preview');
  await expect(page.locator('.detail-head').getByRole('button', { name: 'Deploy', exact: true })).toBeDisabled();
  const retried = page.waitForResponse(response => response.request().method() === 'POST' && response.url().includes(`/environments/${qa}/reconcile?`));
  await page.getByRole('button', { name: 'Retry deletion', exact: true }).click();
  expect((await retried).status()).toBe(202);
  const { environments } = await call(page, 'GET', `applications/${app}`) as {
    environments: { id: string; name: string; delete_intent: boolean }[];
  };
  expect(environments.find(environment => environment.id === qa)).toMatchObject({ name: 'preview', delete_intent: true });
  expect(environments.find(environment => environment.id === env)).toMatchObject({ name: 'production', delete_intent: false });
});

test('an application with no environments can create one in the dashboard', async ({ page, account }) => {
  void account;
  const { app, env, sibling } = await createEnvironments(page);
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
      application.environments = application.environments.filter((environment: { id: string }) => ![env, sibling].includes(environment.id));
    }
    await route.fulfill({ response, json: body });
  });
  await page.goto(`/dashboard/applications/${app}`);
  await page.locator('.detail-head').getByRole('button', { name: 'Deploy', exact: true }).click();
  await expect(page.getByText('No environments yet.', { exact: false })).toBeVisible();
  await page.getByRole('button', { name: 'New environment', exact: true }).click();
  const creator = page.getByRole('dialog', { name: 'Create environment' });
  await creator.getByLabel('Environment name', { exact: true }).fill('replacement');
  await creator.getByRole('button', { name: 'Create environment', exact: true }).click();
  await expect(title(page)).toHaveText('replacement');
  await expect(page.locator('.detail-head').getByRole('button', { name: 'Deploy', exact: true })).toBeEnabled();
  await expect(page).toHaveURL(url => url.pathname.includes(`/applications/${app}/environments/`));
});

test('a deleted environment page cannot resolve to another environment with that name', async ({ page, account }) => {
  void account;
  const { app, env } = await createEnvironments(page, true);
  await page.goto(`/dashboard/applications/${app}/environments/${env}`);
  await expect(title(page)).toHaveText('production');

  // Model the application's response after deletion has finished. The sibling
  // deliberately has the deleted ID as its name, but a different actual ID.
  await page.route(`**/api/v1/applications/${app}`, async route => {
    const response = await route.fetch();
    const body = await response.json();
    body.data.environments = body.data.environments.filter((environment: { id: string }) => environment.id !== env);
    await route.fulfill({ response, json: body });
  });
  await page.reload();
  await expect(page.getByText('This environment no longer exists.', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: /^Deploy/ })).toHaveCount(0);
});

test('a service Logs tab offers each environment once the application has several', async ({ page, account }) => {
  void account;
  const saved = await call(page, 'POST', 'applications/apply', { manifest: webManifest('single'), expected_generation: 0 });
  const app = saved.application_id as string;
  await page.clock.install();
  await page.goto(`/dashboard/applications/${app}/services/web`);
  await page.getByRole('tablist', { name: 'Service sections' }).getByRole('tab', { name: 'Logs', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Service logs' })).toBeVisible();

  await call(page, 'POST', `applications/${app}/environments`, { name: 'staging', expected_generation: saved.generation });
  // The next dashboard refresh lists the new environment.
  await page.clock.runFor(16_000);
  await expect(page.getByText('Logs are read per environment.', { exact: false })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Service logs' })).toHaveCount(0);
  await expect(page.getByRole('link', { name: 'staging', exact: true })).toBeVisible();
});

test('a service receives telemetry once its environment is granted it', async ({ page, account }) => {
  void account;
  const saved = await call(page, 'POST', 'applications/apply', { manifest: webManifest('collector'), expected_generation: 0 });
  const app = saved.application_id as string;
  await page.goto(`/dashboard/applications/${app}/services/web`);
  await page.getByRole('tablist', { name: 'Service sections' }).getByRole('tab', { name: 'Telemetry', exact: true }).click();
  await page.getByLabel('Receive telemetry', { exact: true }).check();
  await page.getByRole('button', { name: 'Save changes', exact: true }).locator('visible=true').click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  const { application } = await call(page, 'GET', `applications/${app}`) as {
    application: { spec: { services: { receives_telemetry?: boolean }[] } };
  };
  expect(application.spec.services[0].receives_telemetry).toBe(true);

  const env = await environmentId(page, app);
  await page.goto(`/dashboard/applications/${app}/environments/${env}`);
  const grant = page.getByRole('button', { name: 'Grant telemetry', exact: true });
  page.once('dialog', async dialog => {
    expect(dialog.message()).toContain('every application');
    await dialog.accept();
  });
  const granted = page.waitForResponse(response => response.request().method() === 'PUT' && response.url().includes(`/environments/${env}/telemetry`));
  await grant.click();
  expect((await granted).status()).toBe(200);
  await expect(page.getByRole('button', { name: 'Revoke telemetry', exact: true })).toBeEnabled();
  const environment = await call(page, 'GET', `environments/${env}`) as { telemetry: boolean };
  expect(environment.telemetry).toBe(true);
});

test('a deployed environment stops and starts from its page', async ({ page, account }) => {
  void account;
  const saved = await call(page, 'POST', 'applications/apply', { manifest: webManifest('stopping'), expected_generation: 0 });
  const app = saved.application_id as string;
  const env = await environmentId(page, app);
  await call(page, 'POST', `environments/${env}/deploy?expected_generation=${saved.generation}`);
  // Stop shows once the environment is deployed.
  await resolveEnvironment(page, env, saved.generation);
  await page.goto(`/dashboard/applications/${app}/environments/${env}`);

  page.once('dialog', async dialog => {
    expect(dialog.message()).toContain('routes are withdrawn');
    await dialog.accept();
  });
  const stopped = page.waitForResponse(response => response.request().method() === 'POST' && response.url().includes(`/environments/${env}/stop`));
  await page.getByRole('button', { name: 'Stop', exact: true }).click();
  expect((await stopped).status()).toBe(202);
  await expect(page.getByRole('button', { name: 'Start', exact: true })).toBeEnabled();
  expect((await call(page, 'GET', `environments/${env}`)).run_state).toBe('stopped');

  const started = page.waitForResponse(response => response.request().method() === 'POST' && response.url().includes(`/environments/${env}/start`));
  await page.getByRole('button', { name: 'Start', exact: true }).click();
  expect((await started).status()).toBe(202);
  await expect(page.getByRole('button', { name: 'Stop', exact: true })).toBeEnabled();
  expect((await call(page, 'GET', `environments/${env}`)).run_state).toBe('running');
});

test('a new environment is promoted from a live environment chosen in the dialog', async ({ page, account }) => {
  void account;
  const { app, env, generation } = await createRepositoryApplication(page);
  // Staging, being deleted, can no longer be promoted from.
  const staging = await call(page, 'POST', `applications/${app}/environments`, { name: 'staging', expected_generation: generation });
  await call(page, 'DELETE', `environments/${staging.id}?expected_generation=${generation + 1}`);
  await page.goto(`/dashboard/applications/${app}?tab=environments`);
  await page.getByRole('button', { name: 'New environment', exact: true }).click();
  const creator = page.getByRole('dialog', { name: 'Create environment' });
  const from = creator.getByRole('combobox', { name: 'Promote from', exact: true });
  await expect(from.locator('option')).toHaveText(['None: build its own source', 'production']);
  await expect(creator.getByLabel('Branch', { exact: true })).toBeVisible();
  await from.selectOption({ label: 'production' });
  // A promoted environment never builds, so it follows no branch.
  await expect(creator.getByLabel('Branch', { exact: true })).toHaveCount(0);
  await expect(creator).toContainText('only deploys releases promoted from the chosen environment');

  await creator.getByLabel('Environment name', { exact: true }).fill('canary');
  const created = page.waitForRequest(request => request.method() === 'POST' && new URL(request.url()).pathname === `/api/v1/applications/${app}/environments`);
  await creator.getByRole('button', { name: 'Create environment', exact: true }).click();
  const body = (await created).postDataJSON();
  expect(body).toMatchObject({ name: 'canary', promote_from: env });
  expect(body.branch ?? null).toBeNull();
  await expect(title(page)).toHaveText('canary');
  await expect(card(page, 'Source')).toContainText('Receives releases promoted from production');
});

test('an unsaved source blocks deploying, and an invalid commit is refused before it is sent', async ({ page, account }) => {
  void account;
  const { app, env } = await createRepositoryApplication(page);
  await page.clock.install();
  await page.goto(`/dashboard/applications/${app}/environments/${env}`);
  const source = card(page, 'Source');
  const deploy = head(page).getByRole('button', { name: 'Deploy', exact: true });
  const save = source.getByRole('button', { name: 'Save source', exact: true });
  const commit = source.getByLabel('Commit (optional)', { exact: true });
  await expect(deploy).toBeEnabled();
  const changes: string[] = [];
  const record = (request: Request) => {
    if (request.method() === 'PUT' && request.url().includes(`/environments/${env}/branch`)) changes.push(request.url());
  };
  page.on('request', record);

  await commit.fill('abc1234');
  await expect(deploy).toBeDisabled();
  await save.click();
  await expect(page.getByRole('alert').getByText(/git_commit_invalid/)).toBeVisible();
  await expect(deploy).toBeDisabled();
  page.off('request', record);
  expect(changes).toEqual([]);

  // A source changed elsewhere keeps the typed draft.
  const { generation } = await call(page, 'GET', `applications/${app}`);
  await call(page, 'PUT', `environments/${env}/branch`, { branch: 'next', expected_generation: generation });
  // The next dashboard refresh shows it.
  await page.clock.runFor(16_000);
  await expect(source).toContainText('on branch next');
  await expect(source.getByLabel('Branch', { exact: true })).toHaveValue('main');
  await expect(commit).toHaveValue('abc1234');
  await expect(deploy).toBeDisabled();

  // Showing the saved source again leaves nothing to save.
  await source.getByLabel('Branch', { exact: true }).fill('next');
  await commit.fill(' ');
  await expect(save).toBeDisabled();
  await expect(deploy).toBeEnabled();
});

test('a dashboard listing requested before a save does not undo it', async ({ page, account }) => {
  void account;
  const { app, env } = await createEnvironments(page);
  await page.clock.install();
  // Every listing is the one from before the rename, as a poll in flight
  // during the save would return.
  let before: unknown;
  let captured!: () => void;
  const capture = new Promise<void>(done => { captured = done; });
  await page.route(url => url.pathname === '/api/v1/applications', async route => {
    before ??= await (await route.fetch()).json();
    captured();
    await route.fulfill({ json: before });
  });
  const listed = () => page.waitForRequest(request => new URL(request.url()).pathname === '/api/v1/applications');
  await page.goto(`/dashboard/applications/${app}/environments/${env}`);
  await expect(title(page)).toHaveText('production');
  await capture;
  const renamed = page.waitForResponse(response => response.request().method() === 'POST' && new URL(response.url()).pathname === `/api/v1/environments/${env}/rename`);
  await page.getByLabel('Environment name', { exact: true }).fill('live');
  await page.getByRole('button', { name: 'Rename environment', exact: true }).click();
  expect((await renamed).status()).toBe(200);
  // Polls never overlap, so the second listing after the rename starts once
  // the first was shown.
  for (let poll = 0; poll < 2; poll += 1) {
    const request = listed();
    await page.clock.runFor(16_000);
    await request;
  }
  await expect(title(page)).toHaveText('live');
});
