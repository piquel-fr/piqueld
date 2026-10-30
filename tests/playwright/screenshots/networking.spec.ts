import { register, call, rewrite, webManifest } from '../fixtures.js';
import { test, card, deployFrom, environment, routes, runFrom, shoot, source } from './docs.js';

// docs/user/networking: hello from the first application, with the route its
// guide adds, then notes, an application that deploys on push with a webhook.
test('networking', async ({ page, daemon, cli, passkeys }) => {
  void passkeys;
  await register(page, daemon.setup, 'alice');
  await deployFrom(page, cli, 'user/getting-started/first-app.md');
  await runFrom(page, cli, 'user/networking/routes.md', 'app route add');
  await routes(page, 'hello');
  await shoot(page, 'networking/routes-tab', card(page, 'Routes'));

  await environment(page, 'hello', 'production');
  const visibility = card(page, 'Visibility');
  const ceiling = visibility.getByRole('combobox', { name: 'Route visibility' });
  await ceiling.selectOption('private');
  await shoot(page, 'networking/environment-visibility', visibility);
  // Unsaved, the edit would hold the reader on this page.
  await ceiling.selectOption('public');

  const notes = await call(page, 'POST', 'applications/apply', {
    manifest: webManifest('notes', {
      manifest: { path: 'app.toml', sync: { mode: 'webhook' }, repository: { url: 'https://github.com/example/notes.git', branch: 'main' } },
    }),
    expected_generation: 0,
  });
  // The fixture daemon sets no `ingress.webhook_hostname`: show the payload
  // URL of the guide's `hooks.example.com`, as its daemon reports it.
  await rewrite(page, `/api/v1/applications/${notes.application_id}/webhook`, data => {
    data.url ??= `https://hooks.example.com/hooks/github/${notes.application_id}`;
  });
  await source(page, 'notes');
  await shoot(page, 'networking/webhook-card', card(page, 'GitHub webhook'));
});
