import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { test, audit, deliveries, deploy, deployFrom, destination, errors, manifestOf, notifications, piquelctl, save, shoot, telemetryGrant } from './docs.js';

// docs/user/operations: a deployment that fails, what it notifies, the
// audit trail, and a telemetry receiver.
const firstApp = 'user/getting-started/first-app.md';

/** The first application's manifest, named `broken` and running `image`.
 * The fixture's registry lacks `nginx:missing`, so deploying it fails. */
async function broken(image = 'nginx:missing') {
  return (await manifestOf(firstApp)).replace('name = "hello"', 'name = "broken"').replace('nginx:alpine', image);
}

test('history', async ({ page, cli, account }) => {
  void account;
  await deploy(page, cli, await broken(), true);
  await errors(page, 2);
  await shoot(page, 'operations/errors', page.getByText(/^Application deletion removes its history/));
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
    await piquelctl(page, cli, ['notification', 'set', 'ops', '--kind', 'discord', '--url-stdin'], `http://127.0.0.1:${port}/webhook\n`);
    await piquelctl(page, cli, ['notification', 'test', 'ops']);
    await deploy(page, cli, await broken(), true);
    await deploy(page, cli, await broken('nginx:alpine'));
    await notifications(page, 2);
    await shoot(page, 'operations/notifications', deliveries(page));
  } finally {
    receiver.close();
  }
});

test('destination', async ({ page, cli, account }) => {
  void account;
  await save(page, cli, (await manifestOf(firstApp)).replace('name = "hello"', 'name = "shop"'));
  // The guide's example, on a new Discord destination.
  await piquelctl(page, cli, [
    'notification', 'set', 'ops', '--kind', 'discord', '--url-stdin',
    '--categories', 'deployment_failures,recovery',
    '--apps', 'shop',
    '--message', 'deployment_failures=<@&123456789012345678>',
  ], 'https://discord.example/webhook\n');
  await shoot(page, 'operations/destination', await destination(page, 'ops'));
});

test('telemetry', async ({ page, cli, account }) => {
  void account;
  const service = await manifestOf('user/operations/telemetry.md', 'receives_telemetry');
  const header = (await manifestOf(firstApp)).split('[[spec.services]]')[0]!.replace('name = "hello"', 'name = "collector"');
  await save(page, cli, header + service);
  await shoot(page, 'operations/telemetry-grant', await telemetryGrant(page, 'collector', 'production'));
});

test('audit', async ({ page, cli, account }) => {
  void account;
  await deployFrom(page, cli, firstApp);
  await piquelctl(page, cli, ['audit', '--limit', '3']);
  await audit(page);
  await shoot(page, 'operations/audit', page.getByRole('table'));
});
