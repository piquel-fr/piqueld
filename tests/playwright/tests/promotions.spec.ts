import { test, expect } from '../fixtures.js';
import type { Page, Route } from '@playwright/test';

/** The manifest of `shop`: `web` mounts each environment's own API key. */
const manifest = {
  api_version: 'piqueld.dev/v1alpha1', kind: 'Application',
  metadata: { name: 'shop' },
  spec: {
    environments: {
      staging: { variables: { api_key: 'staging-api-key' } },
      production: { variables: { api_key: 'production-api-key' } },
    },
    services: [{
      name: 'web',
      source: { type: 'image', image: 'nginx:alpine' },
      secrets: [{ name: '${{ vars.api_key }}', target: '/run/secrets/api' }],
    }],
  },
};

/** Calls the API from the signed-in page, returning the response's `data`. */
async function call(page: Page, method: string, path: string, body?: unknown, headers: Record<string, string> = {}) {
  return page.evaluate(async ({ method, path, body, headers }) => {
    const raw = typeof body === 'string';
    const response = await fetch(`/api/v1/${path}`, {
      method,
      headers: body === undefined ? headers : { 'Content-Type': raw ? 'application/octet-stream' : 'application/json', ...headers },
      body: body === undefined ? undefined : raw ? body : JSON.stringify(body),
    });
    const value = await response.json();
    if (!response.ok) throw new Error(JSON.stringify(value));
    return value.data;
  }, { method, path, body, headers });
}

/** `shop` with `staging` building the saved manifest and `production` promoted from it. */
async function createPromotion(page: Page) {
  const saved = await call(page, 'POST', 'applications/apply', { manifest, expected_generation: 0 });
  const app = saved.application_id as string;
  const staging = (await call(page, 'POST', `applications/${app}/environments`, {
    name: 'staging', expected_generation: saved.generation,
  })).id as string;
  const { generation } = await call(page, 'GET', `applications/${app}`);
  await call(page, 'PUT', `environments/${app}/source`, { promote_from: staging, expected_generation: generation });
  return { app, production: app, staging };
}

/** A release of `shop`'s saved manifest, as the daemon would list it; the
 * fixture never deploys, so none is recorded. */
async function release(page: Page, app: string) {
  const { application } = await call(page, 'GET', `applications/${app}`);
  return {
    id: 'rel-0123456789abcdef0123456789abcdef', application_id: app,
    content_hash: `sha256:${'b'.repeat(64)}`, created_at_ms: Date.now(),
    release: {
      template: application,
      sources: { web: { type: 'image', requested: 'nginx:alpine', digest_reference: `docker.io/library/nginx@sha256:${'a'.repeat(64)}` } },
    },
    fingerprint: { web: { 'source.image': 'nginx:alpine' } },
    availability: { state: 'present' },
    promotions: [],
  };
}

/** A plan of promoting `shop`'s release from `staging`'s deployment into
 * production, lacking `secrets`, built on a real plan of staging. */
async function promotionPlan(page: Page, app: string, staging: string, secrets: unknown[]) {
  const plan = await call(page, 'POST', `applications/plan?environment=${staging}`, { manifest });
  plan.release = {
    release: await release(page, app),
    origin: { type: 'promotion', environment: staging, deployment: 'operation-0123456789abcdef' },
    new_volumes: [],
    secrets,
  };
  return plan;
}

const environmentTab = (page: Page, name: string) =>
  page.getByRole('navigation', { name: 'Environment sections' }).getByRole('button', { name, exact: true });
const applicationTab = (page: Page, name: string) =>
  page.getByRole('navigation', { name: 'Application sections' }).getByRole('button', { name, exact: true });
const head = (page: Page) => page.locator('.detail-head');

test('deploying and promoting start from a list of environments with their own actions', async ({ page, account }) => {
  void account;
  const { app, production, staging } = await createPromotion(page);

  // The application lists every environment with its own action, a promoted one naming its source.
  await page.goto(`/dashboard/applications/${app}`);
  await head(page).getByRole('button', { name: 'Deploy', exact: true }).click();
  const deploy = page.getByRole('dialog', { name: 'Deploy an environment' });
  await expect(deploy.getByRole('listitem').locator('.title')).toHaveText(['production', 'staging']);
  await expect(deploy.getByRole('listitem').getByRole('button')).toHaveText(['Promote from staging', 'Deploy']);
  await expect(deploy).not.toContainText('promoted from');
  await expect(deploy).not.toContainText('saved manifest');
  await deploy.getByRole('button', { name: 'Close dialog', exact: true }).click();

  // A source environment promotes into the environments that follow it, before deploying itself.
  await page.goto(`/dashboard/applications/${app}/environments/${staging}`);
  await expect(head(page).getByRole('button', { name: /^(Preview|Promote|Deploy)$/ })).toHaveText(['Preview', 'Promote', 'Deploy']);
  await head(page).getByRole('button', { name: 'Promote', exact: true }).click();
  const promote = page.getByRole('dialog', { name: 'Promote into an environment' });
  await expect(promote.getByRole('listitem')).toHaveCount(1);
  await expect(promote.getByRole('listitem').getByRole('button')).toHaveText('Promote from staging');
  await expect(promote).toContainText('production');

  // A promoted environment never builds: it promotes from its source.
  await page.goto(`/dashboard/applications/${app}/environments/${production}`);
  await expect(head(page).getByRole('button', { name: 'Promote from staging', exact: true })).toBeVisible();
  await expect(head(page).getByRole('button', { name: /^(Preview|Deploy)$/ })).toHaveCount(0);
});

test('promoting shows the plan and promotes exactly what was reviewed', async ({ page, account }) => {
  void account;
  const { app, production, staging } = await createPromotion(page);
  let plan = await promotionPlan(page, app, staging, [{ problem: 'missing', secret: 'production-api-key' }]);
  const plans: unknown[] = [];
  await page.route(url => url.pathname === `/api/v1/environments/${production}/promote/plan`, async (route: Route) => {
    plans.push(route.request().postDataJSON());
    await route.fulfill({ json: { data: plan } });
  });

  await page.goto(`/dashboard/applications/${app}/environments/${production}`);
  const promote = head(page).getByRole('button', { name: 'Promote from staging', exact: true });
  await promote.click();
  const dialog = page.getByRole('dialog', { name: 'Promotion preview' });
  await expect(dialog).toContainText('production-api-key');
  await expect(dialog.getByRole('button', { name: 'Promote release', exact: true })).toBeDisabled();
  await dialog.getByRole('button', { name: 'Close dialog', exact: true }).click();

  // Once nothing is missing, confirming pins the reviewed deployment and revision.
  plan = await promotionPlan(page, app, staging, []);
  let promoted: Record<string, unknown> | undefined;
  await page.route(url => url.pathname === `/api/v1/environments/${production}/promote`, async (route: Route) => {
    promoted = route.request().postDataJSON();
    await route.fulfill({
      status: 202,
      json: { data: {
        operation_id: 'operation-0123456789abcdf0', environment_id: production, generation: plan.generation,
        release_id: plan.release.release.id, origin: plan.release.origin,
      } },
    });
  });
  await promote.click();
  await dialog.getByRole('button', { name: 'Promote release', exact: true }).click();
  await expect(dialog).toBeHidden();
  expect(promoted).toMatchObject({ deployment: 'operation-0123456789abcdef', expected_generation: plan.generation });
  expect(plans).toHaveLength(2);
  for (const request of plans) expect(request).not.toHaveProperty('release');

  // An earlier release is deployed again from the Releases tab, through the same plan.
  await page.route(url => url.pathname === `/api/v1/applications/${app}/releases`, async (route: Route) => {
    await route.fulfill({ json: { data: { items: [plan.release.release], next_cursor: null } } });
  });
  await page.goto(`/dashboard/applications/${app}`);
  await applicationTab(page, 'Releases').click();
  await page.getByRole('button', { name: /^rel-0123456789abcdef0123456789abcdef/ }).click();
  await page.getByRole('button', { name: 'Deploy to production', exact: true }).click();
  await expect(dialog.getByRole('button', { name: 'Deploy release', exact: true })).toBeEnabled();
  expect(plans.at(-1)).toMatchObject({ release: plan.release.release.id });
});

test('an environment builds its own source or receives promoted releases, as one choice', async ({ page, account }) => {
  void account;
  const { app, production } = await createPromotion(page);
  await page.goto(`/dashboard/applications/${app}/environments/${production}`);
  const card = page.locator('section.card', { has: page.getByRole('heading', { name: 'Source', exact: true }) });
  await expect(card).toContainText('Receives releases promoted from staging');
  const save = card.getByRole('button', { name: 'Save source', exact: true });
  await expect(save).toBeDisabled();

  const changes: unknown[] = [];
  page.on('request', request => {
    if (request.method() === 'PUT' && request.url().includes(`/environments/${production}/source`)) {
      changes.push(request.postDataJSON().promote_from);
    }
  });
  await card.getByRole('combobox', { name: 'Deploys', exact: true }).selectOption({ label: 'Builds its own source' });
  await expect(card).toContainText('each deployment builds the application\'s saved manifest');
  await save.click();
  await expect(card).toContainText('Builds the application\'s saved manifest.');
  await expect(save).toBeDisabled();

  await card.getByRole('combobox', { name: 'Deploys', exact: true }).selectOption({ label: 'Releases promoted from staging' });
  await save.click();
  await expect(card).toContainText('Receives releases promoted from staging');
  expect(changes).toEqual([null, expect.stringMatching(/^env-/)]);
});

test('an environment\'s Secrets tab edits the stored secrets it may mount', async ({ page, account }) => {
  void account;
  const { app, production, staging } = await createPromotion(page);
  // Production's own key, which staging may not mount and doesn't.
  await call(page, 'PUT', `applications/${app}/secrets/production-api-key?environments=${production}`, 'production', { 'X-Expected-Generation': '0' });

  await page.goto(`/dashboard/applications/${app}/environments/${staging}`);
  await environmentTab(page, 'Secrets').click();
  const mounted = page.locator('section.card', { has: page.getByRole('heading', { name: 'Mounted secrets', exact: true }) });
  await expect(mounted.getByRole('row').filter({ hasText: 'staging-api-key' })).toContainText('not set');
  const store = page.locator('section.card', { has: page.getByRole('heading', { name: 'Secret store', exact: true }) });
  await expect(store).toContainText('No stored secret this environment may mount.');
  await expect(store).not.toContainText('production-api-key');

  // A new secret may be mounted by this environment only.
  const saved = page.waitForRequest(request => request.method() === 'PUT' && request.url().includes(`/applications/${app}/secrets/staging-api-key`));
  await store.getByLabel('Secret name', { exact: true }).fill('staging-api-key');
  await store.getByLabel('Value', { exact: true }).fill('staging');
  await store.getByRole('button', { name: 'Save secret', exact: true }).click();
  expect(new URL((await saved).url()).searchParams.get('environments')).toBe(staging);
  await expect(store.getByRole('row').filter({ hasText: 'staging-api-key' })).toContainText('stored');
  await expect(mounted.getByRole('row').filter({ hasText: 'staging-api-key' })).toContainText('version 1');

  // Before its first promotion, a promoted environment has nothing to mount yet.
  await page.goto(`/dashboard/applications/${app}/environments/${production}`);
  await environmentTab(page, 'Secrets').click();
  await expect(page.getByText('Its source has no release to promote yet.', { exact: true })).toBeVisible();
});
