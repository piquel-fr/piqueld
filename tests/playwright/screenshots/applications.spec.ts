import type { Page } from '@playwright/test';
import { expect, applicationTab, rewrite, submitRegistration, title } from '../fixtures.js';
import { test, accountForm, application, createApplication, environment, manifest, saveChanges, shoot, Piquelctl } from './docs.js';

const guides = 'user/applications';

/** The card of the page headed `heading`. */
const card = (page: Page, heading: string) =>
  page.locator('section.card', { has: page.getByRole('heading', { name: heading, exact: true }) });

// docs/user/applications: building images from Git, deploying on push,
// environments, and previews, each continuing from the one before.
test('shipping applications', async ({ page, daemon, cli, passkeys, gitHost }) => {
  void passkeys;
  await accountForm(page, daemon.setup);
  await submitRegistration(page);
  const piquelctl = await Piquelctl.signIn(page, cli);

  // Building images from Git: welcome builds its repository's Dockerfile.
  const welcome = await gitHost.create('https://github.com/docker/welcome-to-docker.git');
  await welcome.push('main', { Dockerfile: 'FROM node:22-alpine\nWORKDIR /app\nCOPY . .\nCMD ["serve", "-s", "build"]\n' });
  await piquelctl.apply(await manifest(`${guides}/git-builds.md`));
  await application(page, 'welcome', 'Services');
  await page.locator('a.list-row', { hasText: 'web' }).click();
  await expect(title(page)).toHaveText('web');
  await shoot(page, 'applications/service-git-source', page.getByLabel('Dockerfile path', { exact: true }));
  // The fake engine builds in a few milliseconds, never the same number:
  // show the build lasting as long as a real one.
  await rewrite(page, '/api/v1/builds', data => {
    for (const build of data.items) build.finished_at_ms &&= build.started_at_ms + 51_300;
  });
  // Its output names the checkout's random directory: show it where a host
  // keeps it, at the time `shoot` shows dates at.
  await rewrite(page, '/api/v1/builds/1/logs', data => {
    for (const chunk of data.items) {
      chunk.text = chunk.text.replace(/\/\S+\/checkouts\/checkout-\w+/, '/var/lib/piqueld/checkouts/checkout-k3Zq8P');
      chunk.timestamp_ms = Date.UTC(2026, 9, 24, 9, 41, 7);
    }
  });
  await application(page, 'welcome', 'Builds');
  await page.getByRole('button', { name: /Build #1/ }).click();
  await shoot(page, 'applications/build-output', page.getByText('Retained output', { exact: true }));

  // Deploying on push: blog, created and connected to its repository in the
  // dashboard, polls it, then GitHub reports its pushes.
  const blog = await gitHost.create('https://github.com/example/blog.git');
  const site = { Dockerfile: 'FROM nginx:alpine\nCOPY index.html /usr/share/nginx/html/\n', 'index.html': '<h1>Blog</h1>\n' };
  const pushed = `${guides}/deploy-on-push.md`;
  await blog.push('main', { ...site, 'infra/piqueld.toml': await manifest(pushed, 'Keep the manifest in the repository') });
  await createApplication(page, 'blog');
  await applicationTab(page, 'Source').click();
  await page.getByLabel('Load configuration from Git on deploy', { exact: true }).check();
  await page.getByLabel('Repository', { exact: true }).fill('https://github.com/example/blog.git');
  await page.getByLabel('Branch', { exact: true }).fill('main');
  await page.getByLabel('Manifest path', { exact: true }).fill('infra/piqueld.toml');
  await shoot(page, 'applications/repository-connect', card(page, 'Repository manifest'));
  await saveChanges(page);
  await piquelctl.run(['app', 'deploy', 'blog', '--yes']);
  await application(page, 'blog', 'Source');
  await page.getByRole('combobox', { name: 'Deploy on push', exact: true }).selectOption('poll');
  await page.getByLabel('Poll interval (seconds)', { exact: true }).fill('120');
  await saveChanges(page);
  await environment(page, 'blog', 'production');
  await shoot(page, 'applications/environment-sync', card(page, 'Source'));
  await piquelctl.run(['app', 'repository', 'sync', 'blog', 'webhook', '--yes']);
  await piquelctl.run(['env', 'sync', 'blog', 'production', 'on']);
  await piquelctl.run(['app', 'repository', 'webhook', 'rotate', 'blog', '--yes']);
  await application(page, 'blog', 'Source');
  await shoot(page, 'applications/repository-webhook', card(page, 'GitHub webhook'));

  // Environments: blog again, connected with piquelctl, building on staging
  // and promoting to production.
  await piquelctl.run(['app', 'delete', 'blog', '--yes']);
  const staged = await manifest(`${guides}/environments.md`, 'Set up staging and production');
  await blog.push('main', { 'infra/piqueld.toml': staged });
  await piquelctl.run(['app', 'create', 'blog', '--yes']);
  await piquelctl.run(['app', 'repository', 'connect', 'blog', 'https://github.com/example/blog.git', 'infra/piqueld.toml', '--sync', 'poll', '--interval', '120', '--yes']);
  await piquelctl.run(['env', 'create', 'blog', 'staging', '--yes']);
  await piquelctl.run(['env', 'deploy', 'blog', 'staging', '--yes']);
  await piquelctl.run(['env', 'promote', 'blog', 'production', '--yes']);
  await environment(page, 'blog', 'production');
  await shoot(page, 'applications/environment-source', card(page, 'Source'));
  await application(page, 'blog', 'Environments');
  await shoot(page, 'applications/environments', page.getByRole('list', { name: 'Environments' }));

  // Previews: two of feat/login, the first created in the dashboard and
  // deploying local changes, and one of feat/search, merged and deleted since.
  const previewed = `${staged}\n${await manifest(`${guides}/previews.md`, 'Preview a branch')}`;
  await blog.push('feat/login', { 'infra/piqueld.toml': previewed, 'login.html': '<h1>Sign in</h1>\n' }, 'main');
  await blog.push('feat/search', { 'search.html': '<h1>Search</h1>\n' }, 'feat/login');
  await application(page, 'blog', 'Previews');
  await page.getByRole('button', { name: 'New preview', exact: true }).click();
  const create = page.getByRole('dialog', { name: 'Create preview' });
  await create.getByLabel('Branch', { exact: true }).fill('feat/login');
  await shoot(page, 'applications/preview-create');
  await create.getByRole('button', { name: 'Create preview', exact: true }).click();
  await expect(title(page)).toHaveText(/blog-feat-login/);
  await expect(page.getByRole('main')).toContainText(/succeeded/i);
  await piquelctl.run(['preview', 'create', 'blog', '--branch', 'feat/login', '--slot', 'agent-2']);
  await piquelctl.run(['preview', 'create', 'blog', '--branch', 'feat/search']);
  await blog.git('switch', 'feat/login');
  await blog.write({ 'index.html': '<h1>Blog</h1>\n<a href="/draft.html">Draft</a>\n', 'draft.html': '<h1>Draft</h1>\n' });
  await piquelctl.run(['preview', 'deploy', 'blog', '--local'], blog.directory);
  await blog.git('push', 'origin', '--delete', 'feat/search');
  await application(page, 'blog', 'Previews');
  const previews = page.getByRole('list', { name: 'Previews' });
  await expect(previews.getByRole('listitem')).toHaveCount(3);
  await expect(previews).toContainText(/gone/i);
  await shoot(page, 'applications/previews', previews);
  await previews.getByRole('listitem').filter({ hasText: /local/i }).getByRole('link').first().click();
  await expect(title(page)).toHaveText(/blog-feat-login/);
  await shoot(page, 'applications/preview-local');
});
