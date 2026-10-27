// Helpers for the documentation screenshots: each one brings the dashboard to
// a page, or state, a guide shows; `shoot` then saves it into docs/images.
import type { Browser, Locator, Page } from '@playwright/test';
import { test as base, expect, adminToken, fillRegistration, submitRegistration, applicationTab, rewrite, Passkeys, type Cli } from '../fixtures.js';
import { createHash } from 'node:crypto';
import { readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { crc32 } from 'node:zlib';

const docs = new URL('../../../docs/', import.meta.url);

/** The fixtures of the browser tests, with the page's clock under the
 * test's control, which `shoot` needs, and the page drawn as on a host. */
export const test = base.extend({
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

// The text that differs on every run: random IDs, UUIDs and digests, and dates.
const identifier = String.raw`\b(?:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|[0-9a-f]{32,64})\b`;
const date = String.raw`\b\d{1,2}\/\d{1,2}\/\d{4}, \d{1,2}:\d{2}:\d{2} [AP]M\b`;
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
 * one derived from its order on the page, and each date by a fixed one.
 * Returns the replacements. */
async function freeze(page: Page): Promise<string[]> {
  const identifiers = (await varying(page)).filter(value => new RegExp(`^${identifier}$`).test(value));
  const stable = Object.fromEntries(identifiers.map((value, index) => {
    const digest = createHash('sha256').update(`piqueld ${index}`).digest('hex');
    let at = 0;
    return [value, value.replace(/[0-9a-f]/g, () => digest[at++])];
  }));
  await page.evaluate(({ pattern, stable, fixedDate }) => {
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      node.nodeValue = node.nodeValue!.replace(new RegExp(pattern, 'g'), (value, id?: string) => id ? stable[id] ?? id : fixedDate);
    }
  }, { pattern: `(${identifier})|${date}`, stable, fixedDate });
  return [...Object.values(stable), fixedDate];
}

/** The "Create your account" page of setup `link`, filled in for alice. */
export async function accountForm(page: Page, link: string) {
  await fillRegistration(page, link, 'alice', { display: 'Alice', passkey: 'Laptop' });
}

/** Deploys the first manifest of `guide`, a page under docs/, with
 * `piquelctl app apply`, as the guide does, authenticated by a token of the
 * signed-in account. */
export async function deployFrom(page: Page, cli: Cli, guide: string) {
  const markdown = await readFile(new URL(guide, docs), 'utf8');
  const manifest = markdown.match(/```toml\n([\s\S]*?)```/)?.[1];
  if (!manifest) throw new Error(`${guide} shows no TOML manifest`);
  const file = join(cli.daemon.directory, 'manifest.toml');
  await writeFile(file, manifest);
  const applied = await cli.run(['app', 'apply', '--file', file, '--deploy', '--yes'], await adminToken(page, 'Screenshots'));
  expect(applied.code, applied.stderr).toBe(0);
}

/** The Overview, once every application is healthy and the latest deployment succeeded. */
export async function overview(page: Page) {
  await page.goto('/dashboard/');
  const metric = (name: string) => page.locator('.metric', { hasText: name }).locator('strong').textContent();
  await expect.poll(async () => await metric('Healthy') === await metric('Applications')).toBe(true);
  await expect(page.getByRole('region', { name: 'Recent deployments' }).getByRole('row').nth(1)).toContainText(/succeeded/i);
}

/** Environment `name` of application `app`, reached as a reader would, once
 * its services converged. */
export async function environment(page: Page, app: string, name: string) {
  await page.getByRole('link', { name: 'Applications', exact: true }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Applications');
  await page.getByRole('region', { name: 'Applications', exact: true }).getByRole('link').filter({ hasText: app }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText(app);
  await applicationTab(page, 'Environments').click();
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

/** The invitation or enrollment link the Accounts page shows once, read
 * before selecting **Done**. */
export async function done(page: Page): Promise<string> {
  const secret = page.locator('.secret-box');
  await expect(secret).toContainText(/#(invite|enroll)=/);
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
