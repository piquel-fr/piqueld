import { stat } from 'node:fs/promises';
import { test, expect, api, auth, register, proof, Passkeys, type User, type Directory, type Managed, type Ceremony } from '../fixtures.js';

test('setup gates anonymous access, closes permanently, and supports username-less login', async ({ page, daemon, passkeys }) => {
  void passkeys;
  await page.goto('/dashboard/');
  await expect(page.getByRole('heading', { name: 'Set up piqueld' })).toBeVisible();
  expect((await api(page, 'me')).status).toBe(401);
  expect((await page.request.get(`${daemon.origin}/api/v1/applications`)).status()).toBe(401);
  const user = await register(page, daemon.setup, 'alice');
  expect((await auth(page, 'status')).initialized).toBe(true);
  expect((await api(page, 'register/start', {
    invitation: daemon.setup.split('#invite=')[1], user_id: null,
    username: 'intruder', display_name: '', passkey_name: 'Test',
  })).status).toBe(401);
  const cookie = (await page.context().cookies()).find(cookie => cookie.name === 'piqueld_session');
  expect(cookie?.httpOnly).toBe(true);
  await page.getByRole('button', { name: 'Sign out', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Sign in with a passkey', exact: true })).toBeVisible();
  expect((await api(page, 'me')).status).toBe(401);
  await page.getByRole('button', { name: 'Sign in with a passkey', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Sign out', exact: true })).toBeVisible();
  expect((await auth<User>(page, 'me')).id).toBe(user.id);
});

test('rejects assertion replay, substituted user handles, and downgraded user verification', async ({ page, account, passkeys }) => {
  void account;
  const signed = await proof(page, await auth<Ceremony>(page, 'login/start', {}));
  expect((await api(page, 'login/finish', signed)).status).toBe(200);
  expect((await api(page, 'login/finish', signed)).status).toBe(401);
  const wrong = await proof(page, await auth<Ceremony>(page, 'login/start', {}));
  wrong.credential.response.userHandle = Buffer.from('not-an-account').toString('base64url');
  expect((await api(page, 'login/finish', wrong)).status).toBe(401);
  const challenge = await auth<Ceremony>(page, 'login/start', {});
  (challenge.options.publicKey as Record<string, unknown>).userVerification = 'discouraged';
  await passkeys.verified(false);
  const downgraded = await proof(page, challenge);
  await passkeys.verified(true);
  expect((await api(page, 'login/finish', downgraded)).status).toBe(401);
});

test('invitation signup permits cross-account profile editing and passkey enrollment', async ({ page, account, passkeys }) => {
  await page.goto('/dashboard/accounts');
  await page.getByRole('button', { name: 'Create invitation', exact: true }).click();
  const secret = page.locator('.auth-secret');
  await expect(secret).toContainText('#invite=');
  const link = (await secret.innerText()).split('\n').at(-1)!;
  const bob = await register(page, link, 'bob');
  expect((await api(page, 'register/start', {
    invitation: link.split('#invite=')[1], user_id: null,
    username: 'late', display_name: '', passkey_name: 'Test',
  })).status).toBe(401);
  await page.goto('/dashboard/accounts');
  const alice = page.locator('.auth-account').filter({ has: page.getByRole('heading', { name: 'alice', exact: true }) });
  await alice.getByLabel('Display name', { exact: true }).fill('Edited by Bob');
  await alice.getByLabel('Username', { exact: true }).fill('alice-edited');
  const updated = page.locator('.auth-account').filter({ has: page.getByRole('heading', { name: 'alice-edited', exact: true }) });
  await updated.getByRole('button', { name: 'Save profile', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'alice-edited', exact: true })).toBeVisible();
  await expect.poll(async () => (await auth<Directory>(page, 'directory')).users.find(user => user.id === account.id)?.username).toBe('alice-edited');
  // A fresh authenticator can enroll a key for another account without its approval.
  await passkeys.reset();
  await updated.getByLabel('New passkey name', { exact: true }).fill('Enrolled by Bob');
  await updated.getByRole('button', { name: 'Add passkey', exact: true }).click();
  await expect(page.locator('.auth-secret')).toHaveText('Passkey added');
  expect((await auth<User>(page, 'me')).id).toBe(bob.id);
});

test('a transferable invitation can be redeemed successfully only once under a race', async ({ page, account, browser, daemon }) => {
  void account;
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation' })).invitation_url;
  const contexts = await Promise.all([browser.newContext(), browser.newContext()]);
  try {
    const recipients = await Promise.all(contexts.map(context => context.newPage()));
    const devices = await Promise.all(recipients.map(page => Passkeys.create(page)));
    try {
      const proofs = await Promise.all(recipients.map(async (recipient, index) => {
        await recipient.goto(`${daemon.origin}/dashboard/auth`);
        const challenge = await auth<Ceremony>(recipient, 'register/start', {
          invitation: link.split('#invite=')[1], user_id: null,
          username: `racer${index}`, display_name: '', passkey_name: 'Race',
        });
        return proof(recipient, challenge, true);
      }));
      const results = await Promise.all(recipients.map((recipient, index) => api(recipient, 'register/finish', proofs[index])));
      expect(results.map(result => result.status).sort()).toEqual([200, 401]);
    } finally { await Promise.all(devices.map(device => device.close())); }
  } finally { await Promise.all(contexts.map(context => context.close())); }
});

test('CLI device approval, private credentials, API tokens, revocation and expired logout', async ({ page, account, cli }) => {
  const login = cli.start(['login']);
  try {
    const code = await login.waitForOutput(/Enter code: ([A-Z2-9-]+)/, 'stderr');
    await page.goto('/dashboard/auth#device');
    await page.getByLabel('CLI code', { exact: true }).fill(code);
    await page.getByRole('button', { name: 'Approve CLI login', exact: true }).click();
    await expect(page.locator('.auth-secret')).toContainText('CLI approved');
    await expect.poll(() => login.child.exitCode).toBe(0);
    const who = await cli.run(['--json', 'whoami']);
    expect(who.code, who.stderr).toBe(0);
    expect(JSON.parse(who.stdout).id).toBe(account.id);
    expect((await stat(cli.env.PIQUELD_CREDENTIALS_FILE!)).mode & 0o777).toBe(0o600);
    expect((await cli.run(['logout'])).code).toBe(0);
    expect((await cli.run(['whoami'])).code).not.toBe(0);
    const { token } = await auth<Managed>(page, 'manage', { action: 'create_token', user_id: account.id, name: 'Test', days: null });
    expect((await cli.run(['whoami'], token)).code).toBe(0);
    await page.goto('/dashboard/accounts');
    await auth(page, 'manage', { action: 'revoke_all', user_id: account.id });
    expect((await cli.run(['whoami'], token)).code).not.toBe(0);
    const dialog = page.getByRole('dialog');
    await expect(dialog).toBeVisible({ timeout: 20_000 });
    await dialog.getByRole('button', { name: 'Sign out', exact: true }).click();
    await expect(page.getByRole('button', { name: 'Sign in with a passkey', exact: true })).toBeVisible();
    await expect(dialog).toHaveCount(0);
  } finally { await login.stop(); }
});

test('account deletion can be cancelled and the last account is protected', async ({ page, account }) => {
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation' })).invitation_url;
  const bob = await register(page, link, 'bob');
  await page.goto('/dashboard/accounts');
  const alice = page.locator('.auth-account').filter({ has: page.getByRole('heading', { name: 'alice', exact: true }) });
  page.once('dialog', dialog => dialog.dismiss());
  await alice.getByRole('button', { name: 'Delete account', exact: true }).click();
  expect((await auth<Directory>(page, 'directory')).users.map(user => user.id)).toContain(account.id);
  page.once('dialog', dialog => dialog.accept());
  await alice.getByRole('button', { name: 'Delete account', exact: true }).click();
  await expect(alice).toHaveCount(0);
  expect((await auth<Directory>(page, 'directory')).users.map(user => user.id)).not.toContain(account.id);
  expect((await api(page, 'manage', { action: 'delete_user', user_id: bob.id })).status).toBe(400);
});
