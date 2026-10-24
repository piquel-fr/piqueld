import type { Page } from '@playwright/test';
import { expect, applicationTab, environmentTab, rewrite, submitRegistration, title } from '../fixtures.js';
import { test, accountForm, application, createApplication, environment, manifest, saveChanges, service, shoot, Piquelctl } from './docs.js';

const guides = 'user/applications';

/** Shows every finished build or job run lasting `milliseconds`: the fake
 * engine takes a few milliseconds, never the same number. */
const lasting = (page: Page, milliseconds: number) => rewrite(page, '/api/v1/builds', data => {
  for (const build of data.items) build.finished_at_ms &&= build.started_at_ms + milliseconds;
});

/** Shows the output of build or job run `id` as captured at the time
 * `shoot` shows dates at, its text changed by `edit`. */
const output = (page: Page, id: number, edit = (text: string) => text) => rewrite(page, `/api/v1/builds/${id}/logs`, data => {
  for (const chunk of data.items) {
    chunk.text = edit(chunk.text);
    chunk.timestamp_ms = Date.UTC(2026, 9, 24, 9, 41, 7);
  }
});

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
  await service(page, 'welcome', 'web', 'Source & scaling');
  await shoot(page, 'applications/service-git-source', page.getByLabel('Dockerfile path', { exact: true }));
  await lasting(page, 51_300);
  // Its output names the checkout's random directory: show it where a host
  // keeps it.
  await output(page, 1, text => text.replace(/\/\S+\/checkouts\/checkout-\w+/, '/var/lib/piqueld/checkouts/checkout-k3Zq8P'));
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

// docs/user/applications: the services, secrets, volumes and files of notes,
// then the job of reader, the link from auth to notes, and a command in
// notes, each continuing from the one before.
test('configuring applications', async ({ page, daemon, cli, passkeys }) => {
  void passkeys;
  await accountForm(page, daemon.setup);
  await submitRegistration(page);
  const piquelctl = await Piquelctl.signIn(page, cli);

  // Configuring services.
  let notes = await manifest(`${guides}/services.md`);
  await piquelctl.apply(notes);
  await service(page, 'notes', 'web', 'Health check');
  await shoot(page, 'applications/service-health-check', page.getByLabel('Retries', { exact: true }));

  // Secrets and variables: smtp-password, stored and mounted in web.
  await piquelctl.run(['app', 'secret', 'notes', 'set', 'smtp-password', '--file', await piquelctl.write('smtp-password', 'correct horse battery staple'), '--yes']);
  notes = notes.replace('depends_on = ["db"]\n', `depends_on = ["db"]

[spec.services.environment]
SMTP_PASSWORD_FILE = "/run/secrets/smtp-password"

[[spec.services.secrets]]
name = "smtp-password"
target = "/run/secrets/smtp-password"
`);
  await piquelctl.apply(notes);
  await application(page, 'notes', 'Secrets');
  await shoot(page, 'applications/application-secrets');
  await environment(page, 'notes', 'production');
  await environmentTab(page, 'Secrets').click();
  await shoot(page, 'applications/environment-secrets');

  // Volumes and files: db keeps its data in a volume, and web mounts its
  // configuration.
  notes = notes.replace('start_period_seconds = 30\n', `start_period_seconds = 30

[[spec.services.mounts]]
volume = "data"
target = "/var/lib/postgresql/data"
`).replace('path = "/"\n', `path = "/"

[[spec.services.files]]
target = "/etc/nginx/conf.d/default.conf"
content = """
server {
    listen 80;
    location / {
        root /usr/share/nginx/html;
    }
}
"""
`) + '\n[[spec.volumes]]\nname = "data"\n';
  await piquelctl.apply(notes);
  await service(page, 'notes', 'db', 'Volume mounts');
  await shoot(page, 'applications/volume-mounts');

  // Running jobs: reader migrates its database before each rollout.
  const jobs = `${guides}/jobs.md`;
  let reader = await manifest(jobs, 'Set it up');
  await piquelctl.apply(reader, false);
  const password = 'a'.repeat(64);
  await piquelctl.run(['app', 'secret', 'reader', 'set', 'db-password', '--file', await piquelctl.write('db-password', password), '--yes']);
  await piquelctl.run(['app', 'secret', 'reader', 'set', 'database-url', '--file', await piquelctl.write('database-url', `postgres://reader:${password}@db:5432/reader?sslmode=disable`), '--yes']);
  reader += `\n${await manifest(jobs, 'Set it up', 1)}`;
  await piquelctl.apply(reader);
  await application(page, 'reader', 'Jobs');
  await shoot(page, 'applications/jobs-tab');
  await lasting(page, 1_100);
  await output(page, 1);
  await application(page, 'reader', 'Builds');
  await page.getByRole('button', { name: /Job migrate #1/ }).click();
  await shoot(page, 'applications/job-run', page.getByText('Retained output', { exact: true }));

  // Connecting applications: auth exports api to notes.
  await piquelctl.apply(await manifest(`${guides}/links.md`, 'Set it up'));
  notes += `\n${await manifest(`${guides}/links.md`, 'Set it up', 1)}`;
  await piquelctl.apply(notes);
  await application(page, 'auth', 'Links');
  await shoot(page, 'applications/links-tab');
  await environment(page, 'notes', 'production');
  await shoot(page, 'applications/environment-links', card(page, 'Links'));

  // Running commands: a database console command in db.
  await piquelctl.run(['app', 'exec', 'notes', 'db', '--', 'psql', '-U', 'notes', '-c', "SELECT pg_size_pretty(pg_database_size('notes'))"]);
  await environment(page, 'notes', 'production');
  await environmentTab(page, 'Events').click();
  await page.getByLabel('Event kind', { exact: true }).fill('command_started');
  await page.getByLabel('Event kind', { exact: true }).blur();
  const started = page.locator('article.event', { hasText: 'Command Started' });
  await expect(page.locator('article.event')).toHaveCount(1);
  await shoot(page, 'applications/command-events', started);
});
