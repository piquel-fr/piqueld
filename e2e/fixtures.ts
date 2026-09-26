import { test as base, expect, type Page, type CDPSession } from '@playwright/test';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const bin = process.env.PIQUELD_E2E_BIN_DIR ?? resolve(root, process.env.CARGO_TARGET_DIR ?? 'target', 'debug');
export type User = { id: string; username: string };
export type Ceremony = { id: string; options: { publicKey: unknown } };
export type Directory = { users: User[] };
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
  async run(args: string[], token?: string) {
    const child = this.start(args, token);
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
export async function register(page: Page, link: string, username: string): Promise<User> {
  await page.goto(link);
  await page.getByLabel('Username', { exact: true }).fill(username);
  await page.getByRole('button', { name: 'Create account with a passkey', exact: true }).click();
  await expect(page).toHaveURL(/\/dashboard\/$/);
  await expect(page.getByRole('button', { name: 'Sign out', exact: true })).toBeVisible();
  return auth<User>(page, 'me');
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

export const test = base.extend<{ daemon: Daemon; passkeys: Passkeys; account: User; cli: Cli; browserErrors: void }>({
  daemon: async ({}, use, testInfo) => {
    const directory = await mkdtemp(join(tmpdir(), 'piqueld-e2e-'));
    const child = new Process(join(bin, 'examples/browser_fixture'), [], { ...process.env, PIQUELD_E2E_DATA_DIR: directory });
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
