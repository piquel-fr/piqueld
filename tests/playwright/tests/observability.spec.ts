import { test, expect, call, webManifest } from '../fixtures.js';

test('an event leads to its operation\'s events and to the requests behind them in the audit trail', async ({ page, account }) => {
  void account;
  const saved = await call(page, 'POST', 'applications/apply', { manifest: webManifest('shop'), expected_generation: 0 });
  const id = saved.application_id as string;
  const deployed = await call(page, 'POST', `environments/${id}/deploy?expected_generation=${saved.generation}`);
  // A stale generation is refused, and audited as failed.
  await expect(call(page, 'POST', `environments/${id}/deploy?expected_generation=${saved.generation + 1}`)).rejects.toThrow('generation_conflict');

  await page.goto('/dashboard/events');
  const events = page.locator('.event');
  await expect(events.filter({ hasText: 'created environment production' })).toHaveCount(1);
  await expect(events.filter({ hasText: 'alice became an administrator' })).toHaveCount(1);
  const kind = page.getByRole('combobox', { name: 'Event kind', exact: true });
  await expect(kind.getByRole('option', { name: 'environment created', exact: true })).toHaveCount(1);
  await kind.selectOption('environment_created');
  await expect(events).toHaveCount(1);
  await expect(events.locator('.badge')).toHaveText('environment created');
  // Every recorded field is behind Details.
  await events.getByRole('button', { name: 'Details', exact: true }).click();
  await expect(events.locator('.event-details')).toContainText('environment_created');
  await kind.selectOption('');

  const requested = events.filter({ hasText: 'refresh requested' });
  await requested.getByRole('link', { name: 'Operation', exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/dashboard/events\\?operation=${deployed.operation_id}$`));
  await expect(page.getByText(/^Showing the events of operation/)).toBeVisible();
  await expect(events).toHaveCount(1);
  await expect(events.locator('.badge')).toHaveText('refresh requested');

  // The requester's trail: their deployments, not the anonymous sign-in check.
  await requested.getByRole('link', { name: 'Requester', exact: true }).click();
  await expect(page).toHaveURL(/\/dashboard\/audit\?credential=/);
  const requests = page.locator('tbody tr');
  await expect(requests.filter({ hasText: 'POST /api/v1/environments/{id}/deploy' })).toHaveCount(2);
  await expect(requests.filter({ hasText: 'POST /api/v1/applications/apply' })).toHaveCount(1);
  await expect(requests.filter({ hasText: 'anonymous' })).toHaveCount(0);
  await page.getByRole('combobox', { name: 'Outcome', exact: true }).selectOption('failed');
  await expect(requests).toHaveCount(1);
  await expect(requests).toContainText('POST /api/v1/environments/{id}/deploy 409');
});
