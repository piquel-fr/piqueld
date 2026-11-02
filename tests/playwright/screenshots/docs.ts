// Helpers for the documentation screenshots: each one brings the dashboard to
// a page, or state, a guide shows; `shoot` then saves it into docs/images.
import type { Browser, Locator, Page } from '@playwright/test';
import { test as base, expect, adminToken, fillRegistration, submitRegistration, applicationTab, rewrite, title, Passkeys, Process, type Cli } from '../fixtures.js';
import { createHash } from 'node:crypto';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { crc32 } from 'node:zlib';

const docs = new URL('../../../docs/', import.meta.url);

/** Git repositories on this machine that the daemon and `piquelctl` reach
 * at the URLs the guides use, such as https://github.com/example/blog.git.
 * Commits have a fixed author and date, so they get the same IDs on every
 * run. */
export class GitHost {
  constructor(readonly root: string) {}

  /** The environment that sends every `https://` URL to the host, without
   * the machine's own Git configuration. */
  get env(): NodeJS.ProcessEnv {
    const author = { NAME: 'Alice', EMAIL: 'alice@example.com', DATE: '2026-10-24T09:41:07Z' };
    return {
      GIT_CONFIG_GLOBAL: '/dev/null',
      GIT_CONFIG_NOSYSTEM: '1',
      GIT_CONFIG_COUNT: '1',
      GIT_CONFIG_KEY_0: `url.file://${this.root}/.insteadOf`,
      GIT_CONFIG_VALUE_0: 'https://',
      ...Object.fromEntries(Object.entries(author).flatMap(([key, value]) => [[`GIT_AUTHOR_${key}`, value], [`GIT_COMMITTER_${key}`, value]])),
    };
  }

  /** A clone of a new, empty repository at `url`, on branch `branch`. */
  async create(url: string, branch = 'main'): Promise<Worktree> {
    const path = url.replace(/^https:\/\//, '');
    await this.git(this.root, 'init', '--bare', '--initial-branch', branch, path);
    const worktree = new Worktree(this, join(this.root, 'worktrees', path));
    await this.git(this.root, 'clone', url, worktree.directory);
    await worktree.git('symbolic-ref', 'HEAD', `refs/heads/${branch}`);
    return worktree;
  }

  /** Runs `git` with `args` in `directory`, and returns its output. */
  async git(directory: string, ...args: string[]): Promise<string> {
    const child = new Process('git', args, { ...process.env, ...this.env }, directory);
    const code = await child.exited;
    expect(code, `git ${args.join(' ')}: ${child.stderr}`).toBe(0);
    return child.stdout;
  }
}

/** A clone of a repository of a `GitHost`, where a guide's commands run. */
export class Worktree {
  constructor(readonly host: GitHost, readonly directory: string) {}

  /** Runs `git` with `args` in the worktree, and returns its output. */
  git(...args: string[]): Promise<string> {
    return this.host.git(this.directory, ...args);
  }

  /** Writes `files`, by path in the worktree. */
  async write(files: Record<string, string>) {
    for (const [path, content] of Object.entries(files)) {
      await mkdir(dirname(join(this.directory, path)), { recursive: true });
      await writeFile(join(this.directory, path), content);
    }
  }

  /** Commits `files` to `branch` and pushes it. A new branch starts from
   * branch `from`. */
  async push(branch: string, files: Record<string, string>, from?: string) {
    if (from) await this.git('switch', '--create', branch, from);
    else if ((await this.git('symbolic-ref', '--short', 'HEAD')).trim() !== branch) await this.git('switch', branch);
    await this.write(files);
    await this.git('add', '--all');
    await this.git('commit', '--message', `Update ${Object.keys(files).join(', ')}`);
    await this.git('push', 'origin', branch);
  }
}

/** The fixtures of the browser tests, with the page's clock under the
 * test's control, which `shoot` needs, and the page drawn as on a host. The
 * daemon reaches the repositories of `gitHost` at their URLs. */
export const test = base.extend<{ gitHost: GitHost }>({
  gitHost: async ({}, use) => {
    const root = await mkdtemp(join(tmpdir(), 'piqueld-git-'));
    try { await use(new GitHost(root)); } finally { await rm(root, { recursive: true, force: true }); }
  },
  processEnv: async ({ gitHost }, use) => { await use(gitHost.env); },
  page: async ({ page }, use) => {
    await page.clock.install();
    // The browser image has no system UI font: its fallback has no bold
    // face, which the dashboard doesn't synthesize. The stylesheet itself
    // sets one, so the dashboard's CSP still applies.
    await page.route(url => url.pathname.startsWith('/dashboard/') && url.pathname.endsWith('.css'), async route => {
      const response = await route.fetch();
      const body = `${await response.text()}\n:root { font-family: "Liberation Sans", sans-serif !important; }\n`;
      await route.fulfill({ response, body });
    });
    // The fixture runs no ingress gateway, which reads as unhealthy. Report
    // it as a daemon with the default configuration does: disabled.
    await rewrite(page, '/api/v1/system/readiness', data => {
      if (!data.ingress.enabled) Object.assign(data.ingress, { healthy: true, message: 'Ingress is disabled in daemon TOML; its listeners are stopped' });
    });
    await use(page);
  },
});

/** Saves the page as docs/images/`name`.png, cut 12 pixels below the
 * bottom of `through`, or 800 pixels tall without it, once `freeze` has
 * made its content the same on every run. With `from`, the image starts 12
 * pixels above it, the page scrolled under the sidebar, instead of at the
 * top of the page.
 *
 * The image records a fingerprint of the page it shows: its content and
 * styles, not its pixels, which antialiasing varies between runs. A page
 * whose fingerprint matches its image's isn't shot again. With
 * `--update-snapshots=none`, as in CI, a page that no longer matches fails
 * instead. */
export async function shoot(page: Page, name: string, through?: Locator, from?: Locator) {
  const top = await from?.evaluate(element => element.getBoundingClientRect().top + scrollY - 12) ?? 0;
  const bottom = await through?.evaluate(element => element.getBoundingClientRect().bottom + scrollY + 12);
  await page.setViewportSize({ width: 1280, height: bottom ? Math.ceil(bottom - top) : 800 });
  await page.evaluate(top => scrollTo(0, top), top);
  // Stop the dashboard's timers, without running any, so no refresh redraws
  // what `freeze` replaced. Fixing the time first makes pausing at it skip
  // nothing, however long the calls take.
  const now = Date.now();
  await page.clock.setFixedTime(now);
  await page.clock.pauseAt(now);
  const frozen = await freeze(page);
  const file = new URL(`images/${name}.png`, docs);
  const current = await fingerprint(page);
  if (await readFile(file).then(Png.fingerprint, () => undefined) !== current) {
    if (test.info().config.updateSnapshots === 'none') {
      throw new Error(`docs/images/${name}.png shows an older version of its page: run just docs-screenshots`);
    }
    await writeFile(file, Png.withFingerprint(await page.screenshot(), current));
  }
  // A refresh already under way when the clock stopped redraws it anyway.
  expect(frozen, 'a refresh redrew the page while it was saved').toEqual(expect.arrayContaining(await varying(page)));
  // Catch up with the time spent paused, so later pages read the daemon's
  // new timestamps as just now.
  await page.clock.setSystemTime(Date.now());
  await page.clock.resume();
}

// The text that differs on every run: random IDs, UUIDs and digests, and
// dates, with their time or, as for dates more than a week away, without.
const identifier = String.raw`\b(?:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|[0-9a-f]{32,64})\b`;
const date = String.raw`\b\d{1,2}\/\d{1,2}\/\d{4}\b(?:, \d{1,2}:\d{2}:\d{2} [AP]M\b)?`;
const fixedDate = '10/24/2026, 9:41:07 AM';
const isoDate = String.raw`\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z\b`;

/** Each distinct ID, digest and date the page shows. */
async function varying(page: Page): Promise<string[]> {
  return page.evaluate(pattern => [...new Set(document.body.innerText.match(new RegExp(pattern, 'g')))], `${identifier}|${date}`);
}

/** A hash of what the frozen page draws: the window's size and, when
 * scrolled, where, its styles, and its content, with form values and without
 * scripts. IDs and dates left in attributes, such as links, are replaced like
 * `freeze` replaces text: each ID by its order on the page, keeping its
 * format and length. */
async function fingerprint(page: Page): Promise<string> {
  const drawn = await page.evaluate(() => {
    const body = document.body.cloneNode(true) as HTMLElement;
    body.querySelectorAll('script').forEach(script => script.remove());
    const styles = Array.from(document.styleSheets).flatMap(sheet => Array.from(sheet.cssRules, rule => rule.cssText));
    const values = Array.from(document.querySelectorAll('input, select, textarea'), field => (field as HTMLInputElement).value);
    const size = scrollY ? `${innerWidth}x${innerHeight}+${scrollY}` : `${innerWidth}x${innerHeight}`;
    return [size, ...styles, ...values, body.outerHTML].join('\n');
  });
  const order = new Map<string, number>();
  const stable = drawn
    .replace(new RegExp(identifier, 'g'), id => {
      if (!order.has(id)) order.set(id, order.size);
      return `id-${order.get(id)}:${id.replace(/[0-9a-f]/g, 'x')}`;
    })
    .replace(new RegExp(`${date}|${isoDate}`, 'g'), 'date');
  return createHash('sha256').update(stable).digest('hex');
}

/** The fingerprint `shoot` records in a PNG, as a `tEXt` chunk. */
const Png = {
  keyword: 'piqueld:page',

  /** The fingerprint recorded in `png`, if any. */
  fingerprint(png: Buffer): string | undefined {
    for (let at = 8; at < png.length; at += 12 + png.readUInt32BE(at)) {
      if (png.toString('latin1', at + 4, at + 8) !== 'tEXt') continue;
      const [keyword, text] = png.toString('latin1', at + 8, at + 8 + png.readUInt32BE(at)).split('\0');
      if (keyword === Png.keyword) return text;
    }
  },

  /** `png` with `fingerprint` recorded before its last chunk, `IEND`. */
  withFingerprint(png: Buffer, fingerprint: string): Buffer {
    const data = Buffer.from(`tEXt${Png.keyword}\0${fingerprint}`, 'latin1');
    const length = Buffer.alloc(4);
    length.writeUInt32BE(data.length - 4);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(data));
    const end = png.length - 12;
    return Buffer.concat([png.subarray(0, end), length, data, crc, png.subarray(end)]);
  },
};

/** Replaces the text that differs on every run: each random ID or digest by
 * one derived from its order on the page, and moves every date by the same
 * amount, the earliest to a fixed one, so the time between them still shows.
 * Returns the replacements. */
async function freeze(page: Page): Promise<string[]> {
  const identifiers = (await varying(page)).filter(value => new RegExp(`^${identifier}$`).test(value));
  const stable = Object.fromEntries(identifiers.map((value, index) => {
    const digest = createHash('sha256').update(`piqueld ${index}`).digest('hex');
    let at = 0;
    return [value, value.replace(/[0-9a-f]/g, () => digest[at++])];
  }));
  // Dates are parsed and formatted in the page, in its time zone and locale,
  // as the dashboard formats them: with their time or, as for dates more than
  // a week away, without.
  const dates = await page.evaluate(({ pattern, stable, date, fixedDate }) => {
    const dates = document.body.innerText.match(new RegExp(date, 'g')) ?? [];
    const shift = Date.parse(fixedDate) - Math.min(...dates.map(value => Date.parse(value)));
    const moved = (value: string) => {
      const at = new Date(Date.parse(value) + shift);
      return value.includes(',') ? at.toLocaleString() : at.toLocaleDateString();
    };
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      node.nodeValue = node.nodeValue!.replace(new RegExp(pattern, 'g'), (value, id?: string) => id ? stable[id] ?? id : moved(value));
    }
    return dates.map(moved);
  }, { pattern: `(${identifier})|${date}`, stable, date, fixedDate });
  return [...Object.values(stable), ...dates];
}

/** The "Create your account" page of setup `link`, filled in for alice. */
export async function accountForm(page: Page, link: string) {
  await fillRegistration(page, link, 'alice', { display: 'Alice', passkey: 'Laptop' });
}

/** The `index`th TOML block of `guide`, a page under docs/, after its
 * heading `section` when given. */
export async function manifest(guide: string, section?: string, index = 0): Promise<string> {
  let markdown = await readFile(new URL(guide, docs), 'utf8');
  if (section) {
    const heading = markdown.search(new RegExp(`^#+ ${section}$`, 'm'));
    if (heading < 0) throw new Error(`${guide} has no section ${section}`);
    markdown = markdown.slice(heading);
  }
  const block = [...markdown.matchAll(/^( *)```toml\n([\s\S]*?)^\1```/gm)][index];
  if (!block) throw new Error(`${guide} shows no TOML block ${index} after ${section ?? 'its start'}`);
  const [, indent, toml] = block;
  return toml.replace(new RegExp(`^${indent}`, 'gm'), '');
}

/** `piquelctl`, signed in with a token of the page's account, running the
 * commands of a guide. */
export class Piquelctl {
  private constructor(readonly cli: Cli, readonly token: string) {}

  static async signIn(page: Page, cli: Cli): Promise<Piquelctl> {
    return new Piquelctl(cli, await adminToken(page, 'Screenshots'));
  }

  /** Runs `piquelctl` with `args`, in directory `cwd` when given, and
   * returns its output. */
  async run(args: string[], cwd?: string): Promise<string> {
    const result = await this.cli.run(args, this.token, cwd);
    expect(result.code, `piquelctl ${args.join(' ')}: ${result.stderr}`).toBe(0);
    return result.stdout;
  }

  /** Saves TOML `manifest` with `piquelctl app apply`, deploying it unless
   * `deploy` is false. */
  async apply(manifest: string, deploy = true) {
    const file = join(this.cli.daemon.directory, 'manifest.toml');
    await writeFile(file, manifest);
    await this.run(['app', 'apply', '--file', file, '--yes', ...deploy ? ['--deploy'] : []]);
  }
}

/** The Overview, once every application is healthy and the latest deployment succeeded. */
export async function overview(page: Page) {
  await page.goto('/dashboard/');
  const metric = (name: string) => page.locator('.metric', { hasText: name }).locator('strong').textContent();
  await expect.poll(async () => await metric('Healthy') === await metric('Applications')).toBe(true);
  await expect(page.getByRole('region', { name: 'Recent deployments' }).getByRole('row').nth(1)).toContainText(/succeeded/i);
}

/** Application `name`'s page, reached as a reader would from a dashboard
 * loaded afresh, so its list of applications is current, on its tab `tab`
 * when given. */
export async function application(page: Page, name: string, tab?: string) {
  await page.goto('/dashboard/');
  await page.getByRole('navigation', { name: 'Dashboard navigation' }).getByRole('link', { name: 'Applications', exact: true }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Applications');
  await page.getByRole('region', { name: 'Applications', exact: true }).getByRole('link').filter({ hasText: name }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText(name);
  if (tab) await applicationTab(page, tab).click();
}

/** The "Create application" dialog, opened from Applications as a reader
 * would, with the application name `name` filled in. */
export async function newApplication(page: Page, name: string): Promise<Locator> {
  await page.goto('/dashboard/');
  await page.getByRole('navigation', { name: 'Dashboard navigation' }).getByRole('link', { name: 'Applications', exact: true }).click();
  await page.getByRole('button', { name: 'New application', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: 'Create application' });
  await dialog.getByLabel('Application name', { exact: true }).fill(name);
  return dialog;
}

/** Creates application `name` in the dashboard, and opens it. */
export async function createApplication(page: Page, name: string) {
  await (await newApplication(page, name)).getByRole('button', { name: 'Create application', exact: true }).click();
  await expect(title(page)).toHaveText(name);
}

/** Selects the only visible "Save changes" button, of the settings group
 * being edited, and waits until it saved. */
export async function saveChanges(page: Page) {
  const save = page.getByRole('button', { name: 'Save changes', exact: true }).locator('visible=true');
  await save.click();
  await expect(save).toHaveCount(0);
}

/** Environment `name` of application `app`, reached as a reader would, once
 * its services converged. */
export async function environment(page: Page, app: string, name: string) {
  await application(page, app, 'Environments');
  await page.getByRole('main').getByRole('link').filter({ hasText: name }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText(name);
  await expect(page.getByRole('region', { name: 'Observed services' })).toContainText(/converged/i);
}

/** The Accounts page, once its accounts loaded. */
export async function accounts(page: Page) {
  await page.goto('/dashboard/accounts');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Accounts');
  await expect(page.locator('.auth-account').first()).toBeVisible();
}

/** The card of account `username` on the Accounts page. */
export const accountCard = (page: Page, username: string) =>
  page.locator('.auth-account').filter({ has: page.getByRole('heading', { name: username, exact: true }) });

/** Chooses grants in the grant editor within `scope`, as the guides do: the
 * applications first, if any, then the preset. */
export async function chooseGrants(scope: Locator, preset: string, applications: string[] = []) {
  if (applications.length) {
    await scope.getByLabel('Application permissions apply to every application, including future ones').setChecked(false);
    for (const application of applications) await scope.getByLabel(application, { exact: true }).setChecked(true);
  }
  await scope.getByLabel('Preset').selectOption(preset);
}

/** Creates an invitation link on the Accounts page, as
 * docs/user/accounts/accounts.md does, and returns it. */
export async function invite(page: Page, preset: string, applications: string[] = []): Promise<string> {
  await page.getByRole('button', { name: 'Create invitation', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: 'Create invitation' });
  await chooseGrants(dialog, preset, applications);
  await dialog.getByRole('button', { name: 'Create link', exact: true }).click();
  return done(page);
}

/** The invitation link, enrollment link or API token the Accounts page shows
 * once, read before selecting **Done**. */
export async function done(page: Page): Promise<string> {
  const secret = page.locator('.secret-box');
  await expect(secret).toContainText(/#(invite|enroll)=|pqd_/);
  const link = (await secret.innerText()).trim();
  await page.getByRole('button', { name: 'Done', exact: true }).click();
  await expect(secret).toHaveCount(0);
  return link;
}

/** Creates account `username` through invitation `link`, in a browser of its own. */
export async function signUp(browser: Browser, link: string, username: string, display: string) {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const passkeys = await Passkeys.create(page);
    try {
      await fillRegistration(page, link, username, { display, passkey: 'Laptop' });
      await submitRegistration(page);
    } finally { await passkeys.close(); }
  } finally { await context.close(); }
}
