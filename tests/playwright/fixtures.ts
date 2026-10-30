import { test as base, expect, type Page, type CDPSession, type Route } from '@playwright/test';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../../', import.meta.url));
const bin = process.env.PIQUELD_PLAYWRIGHT_BIN_DIR ?? resolve(root, process.env.CARGO_TARGET_DIR ?? 'target', 'debug');
export type User = { id: string; username: string };
export type Grant = { permission: string; applications?: string[] };
export type Session = { user: User; grants: Grant[]; scoped: boolean };
export type Account = { user: User; grants: Grant[] };
export type Ceremony = { id: string; options: { publicKey: unknown } };
export type Directory = { users: Account[] };
export type Managed = { invitation_url: string; token: string };
export type Daemon = { origin: string; setup: string; socket: string; directory: string };

/** One owned child process, with bounded shutdown and output kept for diagnostics. */
export class Process {
  readonly child: ChildProcessWithoutNullStreams;
  stdout = '';
  stderr = '';
  readonly exited: Promise<number | null>;
  constructor(command: string, args: string[], env: NodeJS.ProcessEnv = process.env) {
    this.child = spawn(command, args, { env, stdio: 'pipe' });
    this.child.stdout.on('data', chunk => { this.stdout += chunk; });
    this.child.stderr.on('data', chunk => { this.stderr += chunk; });
    this.exited = new Promise((done, reject) => {
      this.child.once('error', reject);
      this.child.once('close', done);
    });
    // Startup failures are reported by waitForOutput/run without unhandled rejections.
    void this.exited.catch(() => {});
  }
  async waitForOutput(pattern: RegExp, stream: 'stdout' | 'stderr'): Promise<string> {
    await expect.poll(() => {
      if (this.child.exitCode !== null) {
        throw new Error(`Process exited before readiness: ${this.stdout}\n${this.stderr}`);
      }
      return this[stream].match(pattern)?.[1];
    }, { timeout: 15_000, message: `Waiting for ${pattern}` }).toBeTruthy();
    return this[stream].match(pattern)![1];
  }
  async stop() {
    this.child.stdin.end();
    const timer = setTimeout(() => this.child.kill('SIGKILL'), 3_000);
    try { await this.exited; } finally { clearTimeout(timer); }
  }
}

export class Cli {
  readonly env: NodeJS.ProcessEnv;
  constructor(readonly daemon: Daemon) {
    this.env = { ...process.env, PIQUELD_CREDENTIALS_FILE: join(daemon.directory, 'credentials.json') };
    delete this.env.PIQUELD_TOKEN;
  }
  start(args: string[], token?: string): Process {
    const env = { ...this.env };
    if (token) env.PIQUELD_TOKEN = token;
    return new Process(join(bin, 'piquelctl'), ['--socket', this.daemon.socket, ...args], env);
  }
  /** Runs `piquelctl args` to completion, with `input`, when given, as its
   * whole standard input. */
  async run(args: string[], token?: string, input?: string) {
    const child = this.start(args, token);
    if (input !== undefined) child.child.stdin.end(input);
    const timer = setTimeout(() => child.child.kill('SIGKILL'), 15_000);
    try {
      const code = await child.exited;
      return { code, stdout: child.stdout, stderr: child.stderr };
    } finally { clearTimeout(timer); }
  }
}

/** Chromium's real WebAuthn implementation, backed by a virtual CTAP2 device. */
export class Passkeys {
  private constructor(private readonly cdp: CDPSession, private id: string) {}
  static async create(page: Page): Promise<Passkeys> {
    const cdp = await page.context().newCDPSession(page);
    await cdp.send('WebAuthn.enable');
    const { authenticatorId } = await cdp.send('WebAuthn.addVirtualAuthenticator', { options: {
      protocol: 'ctap2', transport: 'internal', hasResidentKey: true,
      hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true,
    } });
    return new Passkeys(cdp, authenticatorId);
  }
  async verified(value: boolean) {
    await this.cdp.send('WebAuthn.setUserVerified', { authenticatorId: this.id, isUserVerified: value });
  }
  async reset() {
    await this.cdp.send('WebAuthn.clearCredentials', { authenticatorId: this.id });
  }
  async close() { await this.cdp.detach(); }
}

export async function api<T = Record<string, unknown>>(page: Page, path: string, body?: unknown): Promise<{ status: number; body: T }> {
  return page.evaluate(async ({ path, body }) => {
    const response = await fetch(`/api/v1/auth/${path}`, body === undefined ? {} : {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
    });
    return { status: response.status, body: await response.json() };
  }, { path, body });
}
export async function auth<T = Record<string, unknown>>(page: Page, path: string, body?: unknown): Promise<T> {
  const result = await api<T>(page, path, body);
  expect(result.status, JSON.stringify(result.body)).toBe(200);
  return result.body;
}
/** A new, never-expiring API token named `name`, with the admin grant, of the signed-in account. */
export async function adminToken(page: Page, name: string): Promise<string> {
  return (await auth<Managed>(page, 'manage', { action: 'create_token', grants: [{ permission: 'admin' }], name, days: null })).token;
}
/** Opens invitation `link` and fills in its account form, the display and
 * passkey names only when given. */
export async function fillRegistration(page: Page, link: string, username: string, names: { display?: string; passkey?: string } = {}) {
  await page.goto(link);
  await page.getByLabel('Username', { exact: true }).fill(username);
  if (names.display) await page.getByLabel('Display name (optional)', { exact: true }).fill(names.display);
  if (names.passkey) await page.getByLabel('Passkey name', { exact: true }).fill(names.passkey);
}
/** Creates the account of the filled-in form, with a passkey, and returns it once signed in. */
export async function submitRegistration(page: Page): Promise<User> {
  await page.getByRole('button', { name: 'Create account with a passkey', exact: true }).click();
  await expect(page).toHaveURL(/\/dashboard\/$/);
  await expect(page.getByRole('button', { name: 'Sign out', exact: true })).toBeVisible();
  return (await auth<Session>(page, 'me')).user;
}
export async function register(page: Page, link: string, username: string): Promise<User> {
  await fillRegistration(page, link, username);
  return submitRegistration(page);
}
export async function proof(page: Page, ceremony: Ceremony, registration = false) {
  const credential = await page.evaluate(async ({ options, registration }) => {
    const parser = PublicKeyCredential as unknown as {
      parseCreationOptionsFromJSON(input: unknown): PublicKeyCredentialCreationOptions;
      parseRequestOptionsFromJSON(input: unknown): PublicKeyCredentialRequestOptions;
    };
    const credential = registration
      ? await navigator.credentials.create({ publicKey: parser.parseCreationOptionsFromJSON(options.publicKey) })
      : await navigator.credentials.get({ publicKey: parser.parseRequestOptionsFromJSON(options.publicKey) });
    return (credential as PublicKeyCredential & { toJSON(): { response: Record<string, unknown> } }).toJSON();
  }, { options: ceremony.options, registration });
  return { id: ceremony.id, credential };
}

/** Calls the API from the signed-in page, returning the response's `data`.
 * A string body is sent as raw bytes; a failed response throws. */
export async function call(page: Page, method: string, path: string, body?: unknown, headers: Record<string, string> = {}) {
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

/** Rewrites the JSON `data` of GET responses whose path is exactly `path`. */
export async function rewrite(page: Page, path: string, edit: (data: Record<string, any>) => void) {
  await page.route(url => url.pathname === path, async (route: Route) => {
    if (route.request().method() !== 'GET') return route.continue();
    const response = await route.fetch();
    const body = await response.json();
    edit(body.data);
    await route.fulfill({ response, json: body });
  });
}

/** Shows environment `id` resolved at `generation` in application
 * responses: the stubbed runtime never resolves a target. */
export async function resolveEnvironment(page: Page, id: string, generation: number) {
  const resolve = (value: unknown): void => {
    if (Array.isArray(value)) return value.forEach(resolve);
    if (typeof value !== 'object' || value === null) return;
    const record = value as Record<string, unknown>;
    if (record.id === id && 'resolved_generation' in record) record.resolved_generation = generation;
    Object.values(record).forEach(resolve);
  };
  await page.route(url => url.pathname.startsWith('/api/v1/applications'), async (route: Route) => {
    const response = await route.fetch();
    const json = await response.json();
    resolve(json);
    await route.fulfill({ response, json });
  });
}

/** The manifest of application `name`, running one `web` service from
 * `nginx:alpine`, with the rest of `spec`. */
export const webManifest = (name: string, spec: Record<string, unknown> = {}) => ({
  api_version: 'piqueld.dev/v1alpha1', kind: 'Application',
  metadata: { name },
  spec: { services: [{ name: 'web', source: { type: 'image', image: 'nginx:alpine' } }], ...spec },
});

/** One of the application page's section tabs. */
export const applicationTab = (page: Page, name: string) =>
  page.getByRole('navigation', { name: 'Application sections' }).getByRole('button', { name, exact: true });
/** One of an environment or preview page's section tabs. */
export const environmentTab = (page: Page, name: string) =>
  page.getByRole('navigation', { name: 'Environment sections' }).getByRole('button', { name, exact: true });
/** The title of an application, environment, or service page. */
export const title = (page: Page) => page.locator('.detail-title h1');
/** The header of an application, environment, or service page, with its actions. */
export const head = (page: Page) => page.locator('.detail-head');

/** Fixture options a configuration can set under `use`. */
export type Options = {
  /** Deploy to the fixture's in-memory Docker engine, so deployments succeed,
   * instead of never deploying. */
  fakeDocker: boolean;
};

export const test = base.extend<Options & { daemon: Daemon; passkeys: Passkeys; account: User; cli: Cli; browserErrors: void }>({
  fakeDocker: [false, { option: true }],
  daemon: async ({ fakeDocker }, use, testInfo) => {
    const directory = await mkdtemp(join(tmpdir(), 'piqueld-playwright-'));
    const env: NodeJS.ProcessEnv = { ...process.env, PIQUELD_PLAYWRIGHT_DATA_DIR: directory };
    if (fakeDocker) env.PIQUELD_PLAYWRIGHT_FAKE_DOCKER = '1';
    const child = new Process(join(bin, 'examples/browser_fixture'), [], env);
    try {
      const line = await child.waitForOutput(/^(\{[^\n]+\})/m, 'stdout');
      const daemon: Daemon = { ...JSON.parse(line), directory };
      await use(daemon);
    } finally {
      try {
        await child.stop();
        await testInfo.attach('daemon.log', { body: child.stderr, contentType: 'text/plain' });
      } finally { await rm(directory, { recursive: true, force: true }); }
    }
  },
  baseURL: async ({ daemon }, use) => { await use(daemon.origin); },
  // The dashboard keeps fetching until the context closes, which disposes the
  // response of any request a test's route handler is still rewriting.
  page: async ({ page }, use) => {
    await use(page);
    await page.unrouteAll({ behavior: 'ignoreErrors' });
  },
  passkeys: async ({ page }, use) => {
    const passkeys = await Passkeys.create(page);
    try { await use(passkeys); } finally { await passkeys.close(); }
  },
  account: async ({ page, daemon, passkeys }, use) => {
    void passkeys;
    await use(await register(page, daemon.setup, 'alice'));
  },
  cli: async ({ daemon }, use) => { await use(new Cli(daemon)); },
  browserErrors: [async ({ page }, use) => {
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await use();
    expect(errors, 'Uncaught browser errors').toEqual([]);
  }, { auto: true }],
});
export { expect };
