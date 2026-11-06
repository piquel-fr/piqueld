import { test, expect, auth, confirmNext, register, type Managed } from '../fixtures.js';
import type { Route } from '@playwright/test';
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';

test('destinations are added, refused with a reason in their dialog, tested, edited and deleted', async ({ page, account }) => {
  void account;
  // A local receiver acknowledging every notification.
  const received: unknown[] = [];
  const receiver = createServer((request, response) => {
    let body = '';
    request.on('data', chunk => { body += chunk; });
    request.on('end', () => {
      received.push(JSON.parse(body));
      response.writeHead(204).end();
    });
  });
  await new Promise<void>(done => receiver.listen(0, '127.0.0.1', done));
  const { port } = receiver.address() as AddressInfo;
  try {
    await page.goto('/dashboard/notifications');
    await expect(page.getByText('No destinations. Nothing is notified.')).toBeVisible();
    await page.getByRole('button', { name: 'Add destination', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'Notification destination' });
    await dialog.getByRole('textbox', { name: 'Name', exact: true }).fill('ops');
    await dialog.getByLabel('Webhook URL').fill(`http://127.0.0.1:${port}/hook`);
    await dialog.getByRole('checkbox', { name: 'Daemon starts', exact: true }).uncheck();
    // A destination that cannot be saved says why in its dialog and keeps the draft.
    const message = dialog.getByRole('textbox', { name: 'Build failures message', exact: true });
    await message.fill('x'.repeat(1001));
    await dialog.getByRole('button', { name: 'Save destination', exact: true }).click();
    await expect(dialog.getByRole('alert')).toContainText('Build failures:');
    await expect(dialog.getByRole('textbox', { name: 'Name', exact: true })).toHaveValue('ops');
    // An unchecked category's message is kept but not sent, so it saves.
    await dialog.getByRole('checkbox', { name: 'Build failures', exact: true }).uncheck();
    await expect(message).toHaveValue('x'.repeat(1001));
    await dialog.getByRole('button', { name: 'Save destination', exact: true }).click();
    await expect(dialog).toBeHidden();
    const row = page.getByRole('row', { name: /ops/ });
    await expect(row).toContainText('5 categories · every application');
    await expect(row).toContainText('enabled');

    await row.getByRole('button', { name: 'Test', exact: true }).click();
    await expect(page.getByText('The test notification was acknowledged.')).toBeVisible();
    expect(received).toHaveLength(1);

    // Editing shows the saved destination, without the earlier error, and
    // keeps the URL unless a new one is typed.
    await row.getByRole('button', { name: 'Edit', exact: true }).click();
    await expect(dialog.getByRole('textbox', { name: 'Name', exact: true })).toHaveValue('ops');
    await expect(dialog.getByRole('checkbox', { name: 'Build failures', exact: true })).not.toBeChecked();
    await expect(dialog.getByRole('alert')).toHaveCount(0);
    await expect(dialog.getByLabel('Webhook URL')).toHaveValue('');
    await dialog.getByRole('checkbox', { name: 'Previews', exact: true }).check();
    await dialog.getByRole('button', { name: 'Save destination', exact: true }).click();
    await expect(page.getByText('Saved ops.')).toBeVisible();
    await expect(row).toContainText('previews');

    confirmNext(page);
    await row.getByRole('button', { name: 'Delete', exact: true }).click();
    await expect(page.getByText('No destinations. Nothing is notified.')).toBeVisible();
  } finally {
    receiver.close();
  }
});

test('failed deliveries offer a retry to accounts that operate the daemon', async ({ page, account }) => {
  void account;
  // The fixture delivers nothing on its own: list one failed and one delivered.
  const delivery = (id: string, state: string, last_error: string | null) => ({
    id, event_id: 1, destination: 'ops', subject: { application: 'shop', environment: 'production' },
    category: 'deployment_failures', state, attempts: 3, created_at_ms: Date.now(),
    next_attempt_ms: Date.now(), updated_at_ms: Date.now(), last_error,
  });
  let retriedState = 'failed';
  await page.route(url => url.pathname === '/api/v1/notifications/deliveries', (route: Route) => route.fulfill({
    json: { data: { items: [delivery('delivery-failed', retriedState, 'HTTP 500'), delivery('delivery-sent', 'delivered', null)], next_cursor: null } },
  }));
  await page.route(url => url.pathname === '/api/v1/notifications/deliveries/delivery-failed/retry', (route: Route) => {
    if (route.request().method() !== 'POST') return route.fallback();
    retriedState = 'pending';
    return route.fulfill({ json: { data: true } });
  });

  await page.goto('/dashboard/notifications');
  const deliveries = page.locator('section', { has: page.getByRole('heading', { name: 'Deliveries', exact: true }) });
  await expect(deliveries.getByRole('row', { name: /HTTP 500/ })).toContainText('failed');
  await expect(deliveries.getByRole('button', { name: 'Retry', exact: true })).toHaveCount(1);
  await deliveries.getByRole('row', { name: /HTTP 500/ }).getByRole('button', { name: 'Retry', exact: true }).click();
  // The list refreshes at once: the delivery is pending again.
  await expect(deliveries.getByRole('row', { name: /HTTP 500/ })).toContainText('pending');
  await expect(deliveries.getByRole('button', { name: 'Retry', exact: true })).toHaveCount(0);

  // Reading the daemon shows a failure, without the retry.
  retriedState = 'failed';
  const link = (await auth<Managed>(page, 'manage', { action: 'create_invitation', grants: [{ permission: 'system:read' }] })).invitation_url;
  await register(page, link, 'bob');
  await page.goto('/dashboard/notifications');
  await expect(deliveries.getByRole('row', { name: /HTTP 500/ })).toBeVisible();
  await expect(deliveries.getByRole('button', { name: 'Retry', exact: true })).toHaveCount(0);
});
