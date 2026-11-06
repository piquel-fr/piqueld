import { test, expect, apply, environmentTab, webManifest } from '../fixtures.js';

test('the terminal runs a command, relays input and resizes, and reports failures and exit codes', async ({ page, account }) => {
  void account;
  // The fixture runs `web`, but not `worker`.
  const image = { type: 'image', image: 'nginx:alpine' };
  const { app, env } = await apply(page, webManifest('terminal', { services: [{ name: 'web', source: image }, { name: 'worker', source: image }] }));
  await page.goto(`/dashboard/applications/${app}/environments/${env}?tab=terminal`);
  const service = page.getByRole('combobox', { name: 'Service', exact: true });
  const run = page.getByRole('button', { name: 'Run', exact: true });
  const screen = page.locator('.exec-screen');
  await expect(page.getByLabel('Command', { exact: true })).toHaveValue('/bin/sh');

  // Start failures arrive over the WebSocket, which browsers can read.
  await service.selectOption('worker');
  await run.click();
  await expect(page.getByText('Service has no running task; deploy it or check its health')).toBeVisible();

  await service.selectOption('web');
  await run.click();
  await expect(page.getByRole('status').getByText('Running', { exact: true })).toBeVisible();
  await expect(screen).toContainText('fixture ready');
  await page.keyboard.type('hi');
  await expect(screen).toContainText(/fixture ready\s+hi/);
  // On macOS, Option types characters such as `@` on many layouts.
  await page.evaluate(() => Object.defineProperty(Navigator.prototype, 'platform', { get: () => 'MacIntel' }));
  await page.keyboard.press('Alt+@');
  await expect(screen).toContainText(/fixture ready\s+hi@/);
  await page.setViewportSize({ width: 900, height: 700 });
  await expect(screen).toContainText(/resized to \d+x\d+/);
  await page.keyboard.press('Control+d');
  await expect(page.getByText('Exited with code 3', { exact: true })).toBeVisible();

  // Disconnecting closes the WebSocket without waiting for the command.
  await run.click();
  await expect(screen).toContainText('fixture ready');
  await page.getByRole('button', { name: 'Disconnect', exact: true }).click();
  await expect(page.getByText('Disconnected. The command may still be running in the task.')).toBeVisible();

  await environmentTab(page, 'Events').click();
  await expect(page.locator('.event', { hasText: 'started a command in task fixture-task' }).first()).toBeVisible();
});
