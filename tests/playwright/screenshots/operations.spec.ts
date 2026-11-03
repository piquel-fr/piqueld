import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { rewrite } from '../fixtures.js';
import { test, audit, deliveries, destination, errors, manifest, notifications, Piquelctl, shoot, telemetryGrant } from './docs.js';

// docs/user/operations: a deployment that fails, what it notifies, the
// audit trail, and a telemetry receiver.
const firstApp = 'user/getting-started/first-app.md';

/** The first application's manifest, named `broken` and running `image`.
 * The fixture's registry lacks `nginx:missing`, so deploying it fails. */
async function broken(image = 'nginx:missing') {
  return (await manifest(firstApp)).replace('name = "hello"', 'name = "broken"').replace('nginx:alpine', image);
}

test('history', async ({ page, cli, account }) => {
  void account;
  // The fake engine fails in a few milliseconds, never the same number:
  // show each step lasting as long every time.
  await rewrite(page, '/api/v1/events', data => {
    for (const event of data.items) event.duration_ms &&= 412;
  });
  await (await Piquelctl.signIn(page, cli)).applyFailing(await broken());
  await errors(page, 2);
  await shoot(page, 'operations/errors', page.locator('article.event').last());
});

test('notifications', async ({ page, cli, account }) => {
  void account;
  // A Discord channel's webhook, which acknowledges every notification.
  const receiver = createServer((request, response) => {
    request.resume().on('end', () => response.writeHead(204).end());
  });
  await new Promise<void>(done => receiver.listen(0, '127.0.0.1', done));
  try {
    const { port } = receiver.address() as AddressInfo;
    const piquelctl = await Piquelctl.signIn(page, cli);
    await piquelctl.run(['notification', 'set', 'ops', '--kind', 'discord', '--url-stdin'], undefined, `http://127.0.0.1:${port}/webhook\n`);
    await piquelctl.run(['notification', 'test', 'ops']);
    await piquelctl.applyFailing(await broken());
    await piquelctl.apply(await broken('nginx:alpine'));
    await notifications(page, 2);
    await shoot(page, 'operations/notifications', deliveries(page));
  } finally {
    receiver.close();
  }
});

test('destination', async ({ page, cli, account }) => {
  void account;
  const piquelctl = await Piquelctl.signIn(page, cli);
  await piquelctl.apply((await manifest(firstApp)).replace('name = "hello"', 'name = "shop"'), false);
  // The guide's example, on a new Discord destination.
  await piquelctl.run([
    'notification', 'set', 'ops', '--kind', 'discord', '--url-stdin',
    '--categories', 'deployment_failures,recovery',
    '--apps', 'shop',
    '--message', 'deployment_failures=<@&123456789012345678>',
  ], undefined, 'https://discord.example/webhook\n');
  await shoot(page, 'operations/destination', await destination(page, 'ops'));
});

test('telemetry', async ({ page, cli, account }) => {
  void account;
  const service = await manifest('user/operations/telemetry.md', 'Receive telemetry in an application');
  const header = (await manifest(firstApp)).split('[[spec.services]]')[0]!.replace('name = "hello"', 'name = "collector"');
  await (await Piquelctl.signIn(page, cli)).apply(header + service, false);
  await shoot(page, 'operations/telemetry-grant', await telemetryGrant(page, 'collector', 'production'));
});

test('audit', async ({ page, cli, account }) => {
  void account;
  const piquelctl = await Piquelctl.signIn(page, cli);
  await piquelctl.apply(await manifest(firstApp));
  await piquelctl.run(['audit', '--limit', '3']);
  await audit(page);
  await shoot(page, 'operations/audit', page.getByRole('table'));
});
