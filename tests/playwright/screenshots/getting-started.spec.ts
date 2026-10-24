import { submitRegistration } from '../fixtures.js';
import { test, accountForm, deployFrom, environment, overview, shoot } from './docs.js';

// docs/user/getting-started: the first account, then the first application.
test('getting started', async ({ page, daemon, cli, passkeys }) => {
  void passkeys;
  await accountForm(page, daemon.setup);
  await shoot(page, 'getting-started/create-account');
  await submitRegistration(page);
  await deployFrom(page, cli, 'user/getting-started/first-app.md');
  await overview(page);
  await shoot(page, 'getting-started/overview', page.getByRole('region', { name: 'Recent deployments' }));
  await environment(page, 'hello', 'production');
  await shoot(page, 'getting-started/environment', page.getByRole('region', { name: 'Observed services' }));
});
