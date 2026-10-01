import { test, expect } from '../fixtures.js';

const pages: [string, string][] = [
  ['Applications', 'Applications'],
  ['Builds', 'Builds'],
  ['Events', 'Events'],
  ['Errors', 'Errors'],
  ['Analytics', 'Analytics'],
  ['Notifications', 'Notifications'],
  ['Daemon status', 'Daemon status'],
  ['Host settings', 'Host settings'],
  ['Accounts', 'Accounts'],
  ['Overview', 'Overview'],
];

test('the sidebar reaches every page and shows the account and daemon connection', async ({ page, account }) => {
  await page.goto('/dashboard/');
  const sidebar = page.getByRole('complementary');
  await expect(sidebar.locator('.user-name')).toHaveText(account.username);
  await expect(sidebar.locator('.connection')).toHaveText('Reachable');
  for (const [link, heading] of pages) {
    await sidebar.getByRole('link', { name: link, exact: true }).click();
    await expect(page.getByRole('heading', { level: 1, name: heading, exact: true })).toBeVisible();
    await expect(sidebar.getByRole('link', { name: link, exact: true })).toHaveAttribute('aria-current', 'page');
  }
  await expect(page.getByRole('heading', { name: 'System status', exact: true })).toBeVisible();
  await expect(page.locator('.status-card', { hasText: 'piqueld daemon' })).toContainText('Reachable');
  await page.goto('/dashboard/missing');
  await expect(page.getByRole('heading', { name: 'Page not found', exact: true })).toBeVisible();
  await page.getByRole('link', { name: 'Back to overview', exact: true }).click();
  await expect(page.getByRole('heading', { level: 1, name: 'Overview', exact: true })).toBeVisible();
});
