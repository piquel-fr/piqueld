import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import type { Route } from '@playwright/test';
import { deploymentCards, environmentTab, expect, rewrite, toggleDeployment } from '../fixtures.js';
import { test, audit, deliveries, destination, environment, errors, manifest, notifications, Piquelctl, shoot, telemetryGrant } from './docs.js';

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

test('deployment timeline', async ({ page, cli, account }) => {
  void account;
  // The fake engine runs each step in a few milliseconds, never the same
  // number: show each lasting as long every time, the wait for the service
  // longer than usual.
  const durations: Record<string, [number, number]> = {
    resolving_image: [1_400, 1_600], planning: [200, 200], ensure_network: [300, 300],
    ensure_service: [900, 1_000], wait_for_service: [9_400, 6_200], routing: [400, 400],
  };
  await page.route(url => url.pathname.endsWith('/timeline/stream'), async (route: Route) => {
    const url = route.request().url().replace(/\/stream$/, '');
    const timeline = (await (await route.fetch({ url })).json()).data;
    let at = timeline.operation.started_at_ms;
    for (const step of timeline.steps) {
      const [took, usual] = durations[step.phase] ?? [500, 500];
      Object.assign(step, { started_at_ms: at, finished_at_ms: at + took, expected_ms: usual });
      at += took;
    }
    timeline.operation.finished_at_ms = at;
    await route.fulfill({ contentType: 'text/event-stream', body: `event: timeline\ndata: ${JSON.stringify(timeline)}\n\n` });
  });
  await (await Piquelctl.signIn(page, cli)).apply(await manifest(firstApp));
  await environment(page, 'hello', 'production');
  await environmentTab(page, 'Deployments').click();
  const deployment = deploymentCards(page).first();
  await toggleDeployment(deployment);
  await deployment.getByRole('tab', { name: 'Timeline', exact: true }).click();
  // The last step is shown once the timeline arrived.
  await expect(deployment.getByRole('region', { name: 'Step detail' }).getByRole('heading')).toBeVisible();
  await deployment.evaluate(card => Promise.all(card.getAnimations({ subtree: true }).map(animation => animation.finished)));
  await shoot(page, 'operations/deployment-timeline', deployment, deployment);
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
