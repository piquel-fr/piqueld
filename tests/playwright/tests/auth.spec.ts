import { stat } from 'node:fs/promises';
import { test, expect, api, auth, register, proof, Passkeys, type Session, type Directory, type Managed, type Ceremony } from '../fixtures.js';

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
  const cookie = (await page.context().cookies()).find(cookie => cookie.name.startsWith('piqueld_session_'));
  expect(cookie?.httpOnly).toBe(true);
  await page.getByRole('button', { name: 'Sign out', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Sign in with a passkey', exact: true })).toBeVisible();
  expect((await api(page, 'me')).status).toBe(401);
  await page.getByRole('button', { name: 'Sign in with a passkey', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Sign out', exact: true })).toBeVisible();
  // The first account administers the installation.
  expect(await auth<Session>(page, 'me')).toEqual({ user: expect.objectContaining({ id: user.id }), grants: [{ permission: 'admin' }] });
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

test('invitations carry grants and limit what the new account may see and change', async ({ page, account }) => {
  await page.goto('/dashboard/accounts');
  await page.getByRole('button', { name: 'Create invitation', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: 'Create invitation' });
  await dialog.getByLabel('Preset').selectOption('developer');
  await dialog.getByRole('button', { name: 'Create link', exact: true }).click();
  const secret = page.locator('.secret-box');
  await expect(secret).toContainText('#invite=');
  const link = (await secret.innerText()).trim();
  const bob = await register(page, link, 'bob');
  expect((await api(page, 'register/start', {
    invitation: link.split('#invite=')[1], user_id: null,
    username: 'late', display_name: '', passkey_name: 'Test',
  })).status).toBe(401);
  const permissions = (await auth<Session>(page, 'me')).grants.map(grant => grant.permission);
  expect(permissions).toContain('apps:deploy');
  expect(permissions).not.toContain('accounts:manage');
  // Without accounts:manage, Bob sees and changes only his own account.
  await page.goto('/dashboard/accounts');
  await expect(page.locator('.auth-account')).toHaveCount(1);
  await expect(page.getByRole('heading', { name: 'bob', exact: true })).toBeVisible();
  const denied = await api<{ code: string }>(page, 'manage', {
    action: 'update_user', user_id: account.id, username: 'taken-over', display_name: '',
  });
  expect(denied.status).toBe(403);
  expect(denied.body.code).toBe('permission_denied');
  expect((await auth<Directory>(page, 'directory')).users.map(entry => entry.user.id)).toEqual([bob.id]);
});

test('enrollment links add a passkey only to their account', async ({ page, account, browser }) => {
  void account;
  const invitation = (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [] })).invitation_url;
  const context = await browser.newContext();
  try {
    const recipient = await context.newPage();
    const device = await Passkeys.create(recipient);
    try {
      const bob = await register(recipient, invitation, 'bob');
      // An administrator cannot register a passkey for Bob directly.
      expect((await api(page, 'register/start', {
        invitation: null, user_id: bob.id, username: '', display_name: '', passkey_name: 'Takeover',
      })).status).toBe(400);
      await page.goto('/dashboard/accounts');
      const card = page.locator('.auth-account').filter({ has: page.getByRole('heading', { name: 'bob', exact: true }) });
      await card.getByRole('button', { name: 'Create enrollment link', exact: true }).click();
      const secret = page.locator('.secret-box');
      await expect(secret).toContainText('#enroll=');
      const link = (await secret.innerText()).trim();
      // Bob signs out and enrolls a fresh authenticator through the link.
      await recipient.getByRole('button', { name: 'Sign out', exact: true }).click();
      await expect(recipient.getByRole('button', { name: 'Sign in with a passkey', exact: true })).toBeVisible();
      await device.reset();
      await recipient.goto(link);
      await expect(recipient.getByRole('heading', { name: 'Add a passkey', exact: true })).toBeVisible();
      await recipient.getByRole('button', { name: 'Register passkey', exact: true }).click();
      await expect(recipient).toHaveURL(/\/dashboard\/$/);
      expect((await auth<Session>(recipient, 'me')).user.id).toBe(bob.id);
      expect((await api(recipient, 'register/start', {
        invitation: link.split('#enroll=')[1], user_id: null, username: '', display_name: '', passkey_name: 'Again',
      })).status).toBe(401);
    } finally { await device.close(); }
  } finally { await context.close(); }
});

test('a transferable invitation can be redeemed successfully only once under a race', async ({ page, account, browser, daemon }) => {
  void account;
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [] })).invitation_url;
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
    await page.getByRole('button', { name: 'Review request', exact: true }).click();
    await expect(page.locator('.device-request')).toContainText(code);
    await expect(page.locator('.device-request')).toContainText("the daemon's local Unix socket");
    await page.getByRole('button', { name: 'Approve CLI login', exact: true }).click();
    await expect(page.getByText('CLI approved. You can return to your terminal.', { exact: true })).toBeVisible();
    await expect.poll(() => login.child.exitCode).toBe(0);
    const who = await cli.run(['--json', 'whoami']);
    expect(who.code, who.stderr).toBe(0);
    expect(JSON.parse(who.stdout).user.id).toBe(account.id);
    expect((await stat(cli.env.PIQUELD_CREDENTIALS_FILE!)).mode & 0o777).toBe(0o600);
    expect((await cli.run(['logout'])).code).toBe(0);
    expect((await cli.run(['whoami'])).code).not.toBe(0);
    const { token } = await auth<Managed>(page, 'manage', { action: 'create_token', name: 'Test', days: null });
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

test('account deletion can be cancelled and the last administrator is protected', async ({ page, account }) => {
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [{ permission: 'admin' }] })).invitation_url;
  const bob = await register(page, link, 'bob');
  await page.goto('/dashboard/accounts');
  const alice = page.locator('.auth-account').filter({ has: page.getByRole('heading', { name: 'alice', exact: true }) });
  page.once('dialog', dialog => dialog.dismiss());
  await alice.getByRole('button', { name: 'Delete account', exact: true }).click();
  expect((await auth<Directory>(page, 'directory')).users.map(entry => entry.user.id)).toContain(account.id);
  page.once('dialog', dialog => dialog.accept());
  await alice.getByRole('button', { name: 'Delete account', exact: true }).click();
  await expect(alice).toHaveCount(0);
  expect((await auth<Directory>(page, 'directory')).users.map(entry => entry.user.id)).not.toContain(account.id);
  expect((await api(page, 'manage', { action: 'delete_user', user_id: bob.id })).status).toBe(409);
});
