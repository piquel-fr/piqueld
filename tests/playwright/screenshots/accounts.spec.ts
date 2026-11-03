import { adminToken, accountRows, openAccount, chooseGrants, done, expect, signUp, submitRegistration } from '../fixtures.js';
import { test, accountForm, accounts, invite, shoot } from './docs.js';

// docs/user/accounts: the directory, alice's passkeys, a new API token and
// her sessions and tokens, carol's grants, and pending links.
test('accounts', async ({ page, browser, daemon, cli, passkeys }) => {
  await accountForm(page, daemon.setup);
  await submitRegistration(page);
  const token = await adminToken(page, 'Screenshots');
  for (const name of ['blog', 'shop', 'wiki']) {
    const created = await cli.run(['app', 'create', name, '--yes'], token);
    expect(created.code, created.stderr).toBe(0);
  }
  // Revoked, so that alice's sessions and tokens show only the guide's.
  const revoked = await cli.run(['logout'], token);
  expect(revoked.code, revoked.stderr).toBe(0);
  await accounts(page);
  await signUp(browser, await invite(page, 'read-only'), 'carol', 'Carol');

  // A second passkey, from another device: the first one is already registered.
  let alice = await openAccount(page, 'alice', 'Access');
  await passkeys.reset();
  await alice.getByRole('button', { name: 'Add passkey', exact: true }).click();
  const adding = page.getByRole('dialog', { name: 'Add passkey' });
  await adding.getByLabel('Passkey name', { exact: true }).fill('Phone');
  await adding.getByRole('button', { name: 'Add passkey', exact: true }).click();
  await expect(page.getByText('Passkey added.', { exact: true })).toBeVisible();
  await accounts(page);
  await expect(accountRows(page)).toHaveCount(2);
  await shoot(page, 'accounts/accounts-page', accountRows(page).last());

  alice = await openAccount(page, 'alice', 'Access');
  const keys = alice.getByRole('list', { name: 'Passkeys' }).getByRole('listitem');
  await expect(keys).toHaveCount(2);
  await shoot(page, 'accounts/passkeys', keys.last(), alice.getByRole('heading', { name: 'Passkeys', exact: true }));

  await alice.getByRole('button', { name: 'New token', exact: true }).click();
  const creating = page.getByRole('dialog', { name: 'New API token' });
  await chooseGrants(creating, 'deploy', ['blog']);
  await creating.getByLabel('New token name', { exact: true }).fill('ci');
  const days = creating.getByLabel('Expires in days (blank: never)', { exact: true });
  await days.fill('30');
  await days.blur();
  await expect(creating.getByText('apps:deploy on blog', { exact: true })).toBeVisible();
  // At the default height the dialog shows from its title, scrolling inside.
  await shoot(page, 'accounts/new-api-token');
  await creating.getByRole('button', { name: 'Create token', exact: true }).click();
  await done(page);

  // Shot early, while every credential was last used "just now".
  alice = await openAccount(page, 'alice', 'Access');
  await expect(alice.getByRole('cell', { name: 'ci', exact: true })).toBeVisible();
  await shoot(page, 'accounts/sessions-and-tokens', alice.getByRole('table'),
    alice.getByRole('heading', { name: 'Sessions and API tokens', exact: true }));

  const carol = await openAccount(page, 'carol');
  await carol.getByRole('button', { name: 'Edit grants', exact: true }).click();
  await chooseGrants(carol, 'read-only', ['blog']);
  const save = carol.getByRole('button', { name: 'Save grants', exact: true });
  await shoot(page, 'accounts/grant-editor', save, carol.getByRole('heading', { name: 'Access', exact: true }));
  await save.click();
  await expect(carol.getByText('apps:read on blog', { exact: true })).toBeVisible();

  await accounts(page);
  await invite(page, 'deploy', ['blog']);
  const carolAccess = await openAccount(page, 'carol', 'Access');
  await carolAccess.getByRole('button', { name: 'Create enrollment link', exact: true }).click();
  await done(page);
  await accounts(page);
  const pending = page.locator('section').filter({ has: page.getByRole('heading', { name: 'Pending links', exact: true }) });
  await expect(pending.getByRole('row')).toHaveCount(3);
  await shoot(page, 'accounts/pending-links', pending, pending);
});
