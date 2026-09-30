import { test, expect, auth } from '../fixtures.js';
import type { Page } from '@playwright/test';

async function createApplication(page: Page) {
  await page.getByRole('button', { name: '+ Create application', exact: true }).click();
  await page.getByLabel('Application name', { exact: true }).fill('browser-test');
  await page.getByRole('button', { name: 'Create application', exact: true }).click();
  await expect(page).toHaveURL(/\/dashboard\/applications\/[^/]+$/);
  await expect(page.locator('.application-name h1')).toHaveText('browser-test');
}

test('creates an application, saves a rename, and retains it after reload', async ({ page, account }) => {
  void account;
  await createApplication(page);
  await page.getByRole('button', { name: 'Rename application', exact: true }).click();
  await page.getByLabel('Application name', { exact: true }).fill('renamed-application');
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.locator('.application-name h1')).toHaveText('renamed-application');
  await page.reload();
  await expect(page.locator('.application-name h1')).toHaveText('renamed-application');
});

test('reauthenticates after revocation without losing an unsaved editor draft', async ({ page, account }) => {
  await createApplication(page);
  await page.getByRole('button', { name: 'Rename application', exact: true }).click();
  await page.getByLabel('Application name', { exact: true }).fill('preserved-draft');
  await auth(page, 'manage', { action: 'revoke_all', user_id: account.id });
  // The dashboard's next refresh detects revocation without an explicit reload.
  const dialog = page.getByRole('dialog');
  await expect(dialog).toBeVisible({ timeout: 20_000 });
  await dialog.getByRole('button', { name: 'Sign in with a passkey', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(page.getByLabel('Application name', { exact: true })).toHaveValue('preserved-draft');
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.locator('.application-name h1')).toHaveText('preserved-draft');
});
