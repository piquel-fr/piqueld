import { adminToken, expect, submitRegistration } from '../fixtures.js';
import { test, accountCard, accountForm, accounts, chooseGrants, done, invite, shoot, signUp } from './docs.js';

// docs/user/accounts: alice's account and passkeys, carol's invitation and
// grants, a new API token, and pending links.
test('accounts', async ({ page, browser, daemon, cli, passkeys }) => {
  await accountForm(page, daemon.setup);
  await submitRegistration(page);
  const token = await adminToken(page, 'Screenshots');
  for (const name of ['blog', 'shop', 'wiki']) {
    const created = await cli.run(['app', 'create', name, '--yes'], token);
    expect(created.code, created.stderr).toBe(0);
  }

  // A second passkey, from another device: the first one is already registered.
  await accounts(page);
  const alice = accountCard(page, 'alice');
  await passkeys.reset();
  await alice.getByLabel('New passkey name', { exact: true }).fill('Phone');
  await alice.getByRole('button', { name: 'Add passkey', exact: true }).click();
  await expect(page.getByText('Passkey added.', { exact: true })).toBeVisible();
  await accounts(page);
  await expect(alice.getByLabel('Passkey name', { exact: true })).toHaveCount(2);
  await shoot(page, 'accounts/accounts-page', alice.getByRole('button', { name: 'Add passkey', exact: true }));

  await signUp(browser, await invite(page, 'read-only'), 'carol', 'Carol');
  await accounts(page);
  const carol = accountCard(page, 'carol');
  await carol.getByRole('button', { name: 'Edit grants', exact: true }).click();
  await chooseGrants(carol, 'read-only', ['blog']);
  const save = carol.getByRole('button', { name: 'Save grants', exact: true });
  await shoot(page, 'accounts/grant-editor', save, carol);
  await save.click();
  await expect(carol.getByText('apps:read on blog', { exact: true })).toBeVisible();

  await accounts(page);
  await chooseGrants(alice, 'deploy', ['blog']);
  await alice.getByLabel('New token name', { exact: true }).fill('ci');
  const days = alice.getByLabel('Expires in days (blank: never)', { exact: true });
  await days.fill('30');
  await days.blur();
  await expect(alice.getByText('apps:deploy on blog', { exact: true })).toBeVisible();
  await shoot(page, 'accounts/new-api-token', alice.getByRole('button', { name: 'Create token', exact: true }),
    alice.getByRole('heading', { name: 'New API token', exact: true }));

  await accounts(page);
  await invite(page, 'deploy', ['blog']);
  await carol.getByRole('button', { name: 'Create enrollment link', exact: true }).click();
  await done(page);
  const pending = page.locator('section').filter({ has: page.getByRole('heading', { name: 'Pending links', exact: true }) });
  await expect(pending.getByRole('row')).toHaveCount(3);
  await shoot(page, 'accounts/pending-links', pending, pending);
});
