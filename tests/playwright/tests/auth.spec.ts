import { stat } from 'node:fs/promises';
import {
  test, expect, api, auth, adminToken, apply, card, register, proof, resolveEnvironment, webManifest, applicationTab, environmentTab, title, head,
  accountRows, openAccount, done, signUp, Passkeys, type Account, type Session, type Directory, type Managed, type Ceremony,
} from '../fixtures.js';

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
  })).status).toBe(400);
  const cookie = (await page.context().cookies()).find(cookie => cookie.name.startsWith('piqueld_session_'));
  expect(cookie?.httpOnly).toBe(true);
  await page.getByRole('button', { name: 'Sign out', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Sign in with a passkey', exact: true })).toBeVisible();
  expect((await api(page, 'me')).status).toBe(401);
  await page.getByRole('button', { name: 'Sign in with a passkey', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Sign out', exact: true })).toBeVisible();
  // The first account administers the installation.
  expect(await auth<Session>(page, 'me')).toEqual({ user: expect.objectContaining({ id: user.id }), grants: [{ permission: 'admin' }], scoped: false });
});

test('rejects assertion replay, substituted user handles, and downgraded user verification', async ({ page, account, passkeys }) => {
  void account;
  const signed = await proof(page, await auth<Ceremony>(page, 'login/start', {}));
  expect((await api(page, 'login/finish', signed)).status).toBe(200);
  // A replayed assertion finds its challenge already used.
  expect((await api(page, 'login/finish', signed)).status).toBe(400);
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
  await page.getByRole('button', { name: 'Invite', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: 'Create invitation' });
  await dialog.getByLabel('Preset').selectOption('developer');
  await dialog.getByRole('button', { name: 'Create link', exact: true }).click();
  const secret = page.locator('.secret-box');
  await expect(secret).toContainText('#invite=');
  const link = (await secret.innerText()).trim();
  // Only the notice about the link is announced, and Done removes the link.
  await expect(page.locator('[role=status]', { hasText: link })).toHaveCount(0);
  await page.getByRole('button', { name: 'Done', exact: true }).click();
  await expect(secret).toHaveCount(0);
  const bob = await register(page, link, 'bob');
  expect((await api(page, 'register/start', {
    invitation: link.split('#invite=')[1], user_id: null,
    username: 'late', display_name: '', passkey_name: 'Test',
  })).status).toBe(400);
  const permissions = (await auth<Session>(page, 'me')).grants.map(grant => grant.permission);
  expect(permissions).toContain('apps:deploy');
  expect(permissions).not.toContain('accounts:manage');
  // Without accounts:manage, Bob sees and changes only his own account.
  await page.goto('/dashboard/accounts');
  await expect(accountRows(page)).toHaveCount(1);
  await expect(accountRows(page).getByRole('link', { name: 'bob', exact: true })).toBeVisible();
  const denied = await api<{ code: string }>(page, 'manage', {
    action: 'update_user', user_id: account.id, username: 'taken-over', display_name: '',
  });
  expect(denied.status).toBe(403);
  expect(denied.body.code).toBe('permission_denied');
  expect((await auth<Directory>(page, 'directory')).users.map(entry => entry.user.id)).toEqual([bob.id]);
});

test('an application shows only the controls the account may use on it', async ({ page, account }) => {
  void account;
  const { app, env, generation } = await apply(page, webManifest('limited'));
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [
    { permission: 'apps:read', applications: [app] },
    { permission: 'secrets:write', applications: [app] },
  ] })).invitation_url;
  await register(page, link, 'bob');

  // Bob may read the application, but not rename, deploy, or delete it.
  await page.goto(`/dashboard/applications/${app}`);
  await expect(title(page)).toHaveText('limited');
  await expect(head(page).getByRole('button')).toHaveCount(0);
  await expect(page.getByRole('heading', { name: 'Delete application', exact: true })).toHaveCount(0);
  await expect(applicationTab(page, 'Events')).toHaveCount(0);
  await applicationTab(page, 'Environments').click();
  await expect(page.getByRole('list', { name: 'Environments' }).getByRole('link')).toHaveText(['production']);
  await expect(page.getByRole('list', { name: 'Environments' }).getByRole('button')).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'New environment', exact: true })).toHaveCount(0);

  // Its configuration is read-only, while secrets:write still stores secrets.
  await applicationTab(page, 'Secrets').click();
  await expect(page.getByText('Read-only: editing this configuration requires the apps:write permission.')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Add secret file', exact: true })).toBeDisabled();
  const store = card(page, 'Secret store');
  await store.getByLabel('Secret name', { exact: true }).fill('token');
  await store.getByLabel('Value', { exact: true }).fill('hunter2');
  await store.getByRole('button', { name: 'Save secret', exact: true }).click();
  await expect(store.getByText('Secret saved. Deploy to use its new version.')).toBeVisible();

  // A link to a hidden tab opens the environment's overview instead, and
  // the deployed environment offers no Stop.
  await resolveEnvironment(page, env, generation);
  await page.goto(`/dashboard/applications/${app}/environments/${env}?tab=logs`);
  await expect(environmentTab(page, 'Overview')).toHaveAttribute('aria-selected', 'true');
  await expect(environmentTab(page, 'Logs')).toHaveCount(0);
  await expect(environmentTab(page, 'Events')).toHaveCount(0);
  await expect(head(page).getByRole('button')).toHaveCount(0);
  await expect(page.getByRole('heading', { name: 'Delete environment', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Rename environment', exact: true })).toBeDisabled();
});

test('copy buttons are hidden without a clipboard API, as on plain HTTP origins', async ({ page, account }) => {
  void account;
  await page.addInitScript(() => Object.defineProperty(Navigator.prototype, 'clipboard', { get: () => undefined }));
  await page.goto('/dashboard/accounts');
  await page.getByRole('button', { name: 'Invite', exact: true }).click();
  await page.getByRole('dialog', { name: 'Create invitation' }).getByRole('button', { name: 'Create link', exact: true }).click();
  await expect(page.locator('.secret-box')).toContainText('#invite=');
  await expect(page.getByRole('button', { name: 'Copy to clipboard', exact: true })).toHaveCount(0);
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
      const card = await openAccount(page, 'bob', 'Access');
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
      })).status).toBe(400);
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
      expect(results.map(result => result.status).sort()).toEqual([200, 400]);
    } finally { await Promise.all(devices.map(device => device.close())); }
  } finally { await Promise.all(contexts.map(context => context.close())); }
});

test('CLI device approval, private credentials, API tokens, revocation and expired logout', async ({ page, account, cli }) => {
  const login = cli.start(['login']);
  try {
    // The printed link carries the code, so opening it goes straight to the review.
    const link = await login.waitForOutput(/Open (\S+)/, 'stderr');
    const code = await login.waitForOutput(/Check the page shows code: ([A-Z2-9-]+)/, 'stderr');
    expect(link).toMatch(new RegExp(`/dashboard/auth#device=${code}$`));
    await page.goto(link);
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
    const token = await adminToken(page, 'Test');
    expect(token).toMatch(/^pqd_/);
    expect((await cli.run(['whoami'], token)).code).toBe(0);
    // The dashboard's background refresh runs only when the test advances
    // the clock, once the session is revoked.
    await page.clock.install();
    await page.goto('/dashboard/accounts');
    // Revoke only once the reloaded dashboard has signed in again: revoked
    // before that, it never had a session to expire and shows the sign-in page.
    await expect(accountRows(page).getByRole('link', { name: 'alice', exact: true })).toBeVisible();
    await auth(page, 'manage', { action: 'revoke_all', user_id: account.id });
    expect((await cli.run(['whoami'], token)).code).not.toBe(0);
    await page.clock.runFor(16_000);
    const dialog = page.getByRole('dialog');
    await expect(dialog).toBeVisible();
    await dialog.getByRole('button', { name: 'Sign out', exact: true }).click();
    await expect(page.getByRole('button', { name: 'Sign in with a passkey', exact: true })).toBeVisible();
    await expect(dialog).toHaveCount(0);
  } finally { await login.stop(); }
});

test('the CLI edits and deletes accounts and manages their passkeys, links and credentials', async ({ page, account, browser, cli }) => {
  // Tokens can't create invitations, so the browser does.
  const invite = async () => (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [] })).invitation_url;
  const token = await adminToken(page, 'cli');
  const ctl = async (...args: string[]) => {
    const result = await cli.run(['--json', ...args], token);
    expect(result.code, result.stderr).toBe(0);
    return result.stdout ? JSON.parse(result.stdout) : null;
  };
  const context = await browser.newContext();
  try {
    const recipient = await context.newPage();
    const device = await Passkeys.create(recipient);
    try {
      const bob = await register(recipient, await invite(), 'bob');
      const updated = await ctl('account', 'update', 'bob', '--username', 'robert', '--display-name', 'Robert');
      expect(updated.accounts[0].user).toMatchObject({ id: bob.id, username: 'robert', display_name: 'Robert' });

      const [passkey] = await ctl('account', 'passkey', 'list', 'robert');
      expect(passkey.user_id).toBe(bob.id);
      await ctl('account', 'passkey', 'rename', passkey.id, 'Laptop');
      expect((await ctl('account', 'passkey', 'list', 'robert'))[0].name).toBe('Laptop');

      // Bob's browser session ends with the rest of his credentials.
      expect(await ctl('token', 'list', '--user', 'robert')).not.toHaveLength(0);
      await ctl('token', 'revoke', '--all', '--user', 'robert', '--yes');
      expect(await ctl('token', 'list', '--user', 'robert')).toHaveLength(0);
      expect((await api(recipient, 'me')).status).toBe(401);

      await invite();
      const [pending] = await ctl('account', 'invitation', 'list');
      await ctl('account', 'invitation', 'revoke', pending.id);
      expect(await ctl('account', 'invitation', 'list')).toHaveLength(0);

      await ctl('account', 'passkey', 'remove', passkey.id, '--yes');
      expect(await ctl('account', 'passkey', 'list', 'robert')).toHaveLength(0);
      await ctl('account', 'delete', 'robert', '--yes');
      expect((await ctl('account', 'list')).accounts.map((entry: Account) => entry.user.id)).toEqual([account.id]);
    } finally { await device.close(); }
  } finally { await context.close(); }
});

test('account deletion can be cancelled and the last administrator is protected', async ({ page, account }) => {
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [{ permission: 'admin' }] })).invitation_url;
  const bob = await register(page, link, 'bob');
  const alice = await openAccount(page, 'alice');
  page.once('dialog', dialog => dialog.dismiss());
  await alice.getByRole('button', { name: 'Delete account', exact: true }).click();
  expect((await auth<Directory>(page, 'directory')).users.map(entry => entry.user.id)).toContain(account.id);
  page.once('dialog', dialog => dialog.accept());
  await alice.getByRole('button', { name: 'Delete account', exact: true }).click();
  // Deleting returns to the directory, which no longer lists the account.
  await expect(page).toHaveURL(/\/dashboard\/accounts$/);
  await expect(accountRows(page).getByRole('link', { name: 'alice', exact: true })).toHaveCount(0);
  expect((await auth<Directory>(page, 'directory')).users.map(entry => entry.user.id)).not.toContain(account.id);
  expect((await api(page, 'manage', { action: 'delete_user', user_id: bob.id })).status).toBe(409);
});

test('CLI approval survives signing in first, and a typed code is reviewed before approval', async ({ page, account, cli }) => {
  void account;
  const login = cli.start(['login']);
  try {
    const link = await login.waitForOutput(/Open (\S+)/, 'stderr');
    const code = await login.waitForOutput(/Check the page shows code: ([A-Z2-9-]+)/, 'stderr');
    // Signed out, the link asks for a passkey and keeps its code for after.
    await page.context().clearCookies();
    await page.goto(link);
    await page.getByRole('button', { name: 'Sign in with a passkey', exact: true }).click();
    const request = page.locator('.device-request');
    await expect(request).toContainText(code);
    // Cancelling returns to the code entry, which refuses an unknown code.
    await page.getByRole('button', { name: 'Cancel', exact: true }).click();
    const input = page.getByLabel('CLI code', { exact: true });
    await expect(input).toHaveValue(code);
    const review = page.getByRole('button', { name: 'Review request', exact: true });
    await input.fill('ZZZZ-ZZZZ');
    await review.click();
    await expect(page.getByRole('alert')).toContainText('device code is invalid or expired');
    await expect(request).toHaveCount(0);
    await input.fill(code);
    await review.click();
    await page.getByRole('button', { name: 'Approve CLI login', exact: true }).click();
    await expect(page.getByText('CLI approved. You can return to your terminal.', { exact: true })).toBeVisible();
    await expect.poll(() => login.child.exitCode).toBe(0);
  } finally { await login.stop(); }
});

test.describe('as the host operator', () => {
  test.use({ processEnv: { PIQUELD_PLAYWRIGHT_HOST_OPERATOR: '1' } });

  test('signs in with a one-time link, cannot approve CLI logins, and is revoked by an administrator', async ({ page, account, browser, cli, daemon }) => {
    void account;
    const issued = await cli.run(['--json', 'sign-in-link']);
    expect(issued.code, issued.stderr).toBe(0);
    const context = await browser.newContext();
    try {
      const operator = await context.newPage();
      await operator.goto(JSON.parse(issued.stdout).url);
      await expect(operator.getByRole('heading', { name: 'Sign in as the host operator', exact: true })).toBeVisible();
      // The secret leaves the address bar as soon as the page reads it.
      await expect(operator).toHaveURL(`${daemon.origin}/dashboard/auth`);
      await operator.getByRole('button', { name: 'Sign in as host operator', exact: true }).click();
      await expect(operator).toHaveURL(`${daemon.origin}/dashboard/`);
      await operator.goto(`${daemon.origin}/dashboard/auth#device`);
      await expect(operator.getByText(/^The host operator cannot approve CLI logins/)).toBeVisible();
      await expect(operator.getByLabel('CLI code', { exact: true })).toHaveCount(0);

      // Not being an account, it manages accounts but issues no links or tokens.
      await operator.goto(`${daemon.origin}/dashboard/accounts`);
      await expect(operator.getByRole('button', { name: 'Invite', exact: true })).toHaveCount(0);
      const alice = await openAccount(operator, 'alice', 'Access');
      await expect(alice.getByText(/An account with accounts:manage can send them a single-use enrollment link/)).toBeVisible();
      await expect(alice.getByRole('button', { name: 'Create enrollment link', exact: true })).toHaveCount(0);
      await expect(alice.getByRole('button', { name: 'New token', exact: true })).toHaveCount(0);

      await page.goto('/dashboard/accounts');
      const sessions = page.locator('section').filter({ has: page.getByRole('heading', { name: 'Host operator sessions', exact: true }) });
      await sessions.getByRole('button', { name: 'Revoke', exact: true }).click();
      await expect(sessions).toHaveCount(0);
      expect((await api(operator, 'me')).status).toBe(401);
    } finally { await context.close(); }
  });
});

test('the dashboard creates an API token within its form and its account\'s limits, and revokes it', async ({ page, account, cli }) => {
  void account;
  const { app: blog } = await apply(page, webManifest('blog'));
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [
    { permission: 'apps:read', applications: [blog] }, { permission: 'apps:deploy', applications: [blog] },
  ] })).invitation_url;
  await register(page, link, 'bob');
  const bob = await openAccount(page, 'bob', 'Access');
  await bob.getByRole('button', { name: 'New token', exact: true }).click();
  const form = page.getByRole('dialog', { name: 'New API token' });
  const effective = form.locator('.field', { hasText: 'Effective access' }).locator('.grant-list li');
  // New tokens start read-only, and every choice stays within Bob's access.
  await expect(effective).toHaveText(['apps:read on blog']);
  await form.getByLabel('Preset').selectOption('developer');
  await expect(effective).toHaveText(['apps:read on blog', 'apps:deploy on blog']);

  // An invalid field is reported in the form, which stays open.
  const days = form.getByLabel('Expires in days (blank: never)', { exact: true });
  const create = form.getByRole('button', { name: 'Create token', exact: true });
  await form.getByLabel('New token name', { exact: true }).fill('ci');
  await days.fill('0');
  await create.click();
  await expect(form.getByRole('alert')).toHaveText('Enter a positive number of days, or leave blank');
  await days.fill('30');
  await create.click();
  await expect(form).toBeHidden();
  const token = await done(page);

  const row = bob.getByRole('row').filter({ has: page.getByRole('cell', { name: 'ci', exact: true }) });
  await expect(row.locator('.grant-list li')).toHaveText(['apps:read on blog', 'apps:deploy on blog']);
  await expect(row).not.toContainText('Never');
  const who = await cli.run(['--json', 'whoami'], token);
  expect(who.code, who.stderr).toBe(0);
  expect(JSON.parse(who.stdout)).toMatchObject({ scoped: true, grants: [
    { permission: 'apps:read', applications: [blog] }, { permission: 'apps:deploy', applications: [blog] },
  ] });
  await row.getByRole('button', { name: 'Revoke', exact: true }).click();
  await expect(row).toHaveCount(0);
  expect((await cli.run(['whoami'], token)).code).not.toBe(0);
});

test('editing grants that mix scopes warns, then applies the chosen applications to every permission', async ({ page, account, browser }) => {
  void account;
  const { app: blog } = await apply(page, webManifest('blog'));
  const { app: shop } = await apply(page, webManifest('shop'));
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [
    { permission: 'apps:read' }, { permission: 'apps:deploy', applications: [blog] },
  ] })).invitation_url;
  await signUp(browser, link, 'bob');
  const bob = await openAccount(page, 'bob');
  const original = ['apps:read on every application', 'apps:deploy on blog'];
  await expect(bob.locator('.grant-list li')).toHaveText(original);
  const edit = bob.getByRole('button', { name: 'Edit grants', exact: true });
  const save = bob.getByRole('button', { name: 'Save grants', exact: true });
  // Saving without changing the selection keeps grants it cannot show.
  await edit.click();
  await save.click();
  await expect(save).toHaveCount(0);
  await expect(bob.locator('.grant-list li')).toHaveText(original);
  await edit.click();
  await expect(bob.getByText(/^These grants cover different applications per permission/)).toBeVisible();
  await expect(bob.getByLabel('Application permissions apply to every application, including future ones')).not.toBeChecked();
  await expect(bob.getByLabel('blog', { exact: true })).toBeChecked();
  await bob.getByLabel('blog', { exact: true }).setChecked(false);
  await expect(bob.getByText('Select at least one application, or application permissions grant nothing.', { exact: true })).toBeVisible();
  await bob.getByLabel('shop', { exact: true }).setChecked(true);
  await save.click();
  await expect(bob.locator('.grant-list li')).toHaveText(['apps:read on shop', 'apps:deploy on shop']);
  const saved = (await auth<Directory>(page, 'directory')).users.find(entry => entry.user.username === 'bob')!.grants;
  expect(saved).toEqual([
    { permission: 'apps:read', applications: [shop] },
    { permission: 'apps:deploy', applications: [shop] },
  ]);
});
