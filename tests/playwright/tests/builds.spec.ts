import { test, expect, rewrite } from '../fixtures.js';

test('the builds page shows the build machine, and warns while it is unreachable', async ({ page, account }) => {
  void account;
  // The fixture daemon has no build machine: its status and a check report one.
  const machine = { builder: 'tcp://build.example.ts.net:1234', repository: 'build.example.ts.net:5000/piqueld' };
  await rewrite(page, '/api/v1/system/status', data => {
    data.builder = {
      ...machine,
      last_check: { reachable: false, checked_at_ms: Date.now() - 60_000, message: 'Connection refused (os error 111)' },
    };
  });
  await page.route(url => url.pathname === '/api/v1/system/builder/check', route =>
    route.fulfill({ json: { data: { ...machine, last_check: { reachable: true, checked_at_ms: Date.now() } } } }));
  await page.goto('/dashboard/builds');
  const builder = page.getByRole('group', { name: 'Build machine' });
  await expect(builder).toContainText('Images build on the build machine at tcp://build.example.ts.net:1234');
  await expect(builder).toContainText('It was unreachable');
  await expect(builder).toContainText('Connection refused');
  await builder.getByRole('button', { name: 'Check now', exact: true }).click();
  await expect(builder).toContainText('It answered');
});
