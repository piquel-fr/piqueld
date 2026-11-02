import { test, expect } from '../fixtures.js';
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';

test('destinations are added, tested, edited and deleted from the dashboard', async ({ page, account }) => {
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
    await dialog.getByRole('button', { name: 'Save destination', exact: true }).click();
    const row = page.getByRole('row', { name: /ops/ });
    await expect(row).toContainText('6 categories · every application');
    await expect(row).toContainText('enabled');

    await row.getByRole('button', { name: 'Test', exact: true }).click();
    await expect(page.getByText('The test notification was acknowledged.')).toBeVisible();
    expect(received).toHaveLength(1);

    // Editing keeps the URL unless a new one is typed.
    await row.getByRole('button', { name: 'Edit', exact: true }).click();
    await expect(dialog.getByRole('textbox', { name: 'Name', exact: true })).toHaveValue('ops');
    await expect(dialog.getByLabel('Webhook URL')).toHaveValue('');
    await dialog.getByRole('checkbox', { name: 'Previews', exact: true }).check();
    await dialog.getByRole('button', { name: 'Save destination', exact: true }).click();
    await expect(page.getByText('Saved ops.')).toBeVisible();
    await expect(row).toContainText('previews');

    page.once('dialog', confirm => void confirm.accept());
    await row.getByRole('button', { name: 'Delete', exact: true }).click();
    await expect(page.getByText('No destinations. Nothing is notified.')).toBeVisible();
  } finally {
    receiver.close();
  }
});

test('a destination that cannot be saved says why in its dialog and keeps the draft', async ({ page, account }) => {
  void account;
  await page.goto('/dashboard/notifications');
  await page.getByRole('button', { name: 'Add destination', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: 'Notification destination' });
  await dialog.getByRole('textbox', { name: 'Name', exact: true }).fill('ops');
  await dialog.getByLabel('Webhook URL').fill('http://127.0.0.1:9/hook');
  await dialog.getByRole('textbox', { name: 'Build failures message', exact: true }).fill('x'.repeat(1001));
  await dialog.getByRole('button', { name: 'Save destination', exact: true }).click();
  await expect(dialog.getByRole('alert')).toContainText('Build failures:');
  await expect(dialog.getByRole('textbox', { name: 'Name', exact: true })).toHaveValue('ops');

  // An unchecked category's message is kept but not sent, so it saves.
  await dialog.getByRole('checkbox', { name: 'Build failures', exact: true }).uncheck();
  await expect(dialog.getByRole('textbox', { name: 'Build failures message', exact: true })).toHaveValue('x'.repeat(1001));
  await dialog.getByRole('button', { name: 'Save destination', exact: true }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText('Saved ops.')).toBeVisible();
  await expect(page.getByRole('row', { name: /ops/ })).toContainText('6 categories · every application');
  // Reopening shows the saved destination, without the earlier error.
  await page.getByRole('row', { name: /ops/ }).getByRole('button', { name: 'Edit', exact: true }).click();
  await expect(dialog.getByRole('checkbox', { name: 'Build failures', exact: true })).not.toBeChecked();
  await expect(dialog.getByRole('alert')).toHaveCount(0);
});
