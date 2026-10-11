import { test, expect } from '../fixtures.js';
import type { Page } from '@playwright/test';

/** Creates an application with services `web` (running in the fixture) and `worker` (not running). */
async function createApplication(page: Page) {
  return page.evaluate(async () => {
    const response = await fetch('/api/v1/applications/apply', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        manifest: {
          api_version: 'piqueld.dev/v1alpha1', kind: 'Application',
          metadata: { name: 'terminal' },
          spec: {
            services: [
              { name: 'web', source: { type: 'image', image: 'nginx:alpine' } },
              { name: 'worker', source: { type: 'image', image: 'nginx:alpine' } },
            ],
          },
        },
        expected_generation: 0,
      }),
    });
    const saved = (await response.json()).data;
    const application = (await (await fetch(`/api/v1/applications/${saved.application_id}`)).json()).data;
    return { app: saved.application_id as string, environment: application.environments[0].id as string };
  });
}

test('the terminal runs a command, relays input and resizes, and reports failures and exit codes', async ({ page, account }) => {
  void account;
  const { app, environment } = await createApplication(page);
  await page.goto(`/dashboard/applications/${app}/environments/${environment}?tab=terminal`);
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
  await page.setViewportSize({ width: 900, height: 700 });
  await expect(screen).toContainText(/resized to \d+x\d+/);
  await page.keyboard.press('Control+d');
  await expect(page.getByText('Exited with code 3', { exact: true })).toBeVisible();

  // Disconnecting closes the WebSocket without waiting for the command.
  await run.click();
  await expect(screen).toContainText('fixture ready');
  await page.getByRole('button', { name: 'Disconnect', exact: true }).click();
  await expect(page.getByText('Disconnected. The command may still be running in the task.')).toBeVisible();

  await page.getByRole('navigation', { name: 'Environment sections' }).getByRole('button', { name: 'Events', exact: true }).click();
  await expect(page.locator('.event', { hasText: 'started a command in task fixture-task' }).first()).toBeVisible();
});
