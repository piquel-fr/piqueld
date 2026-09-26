# Browser end-to-end tests

Playwright Test drives Chromium against piqueld's real embedded dashboard and
HTTP/Unix API. A small Rust example starts a fresh database and daemon fixture for
each test. Its Docker runtime boundary is stubbed: these tests cover UI/API
behavior, not actual deployments. Docker reconciliation retains its existing
Rust integration suite.

## Setup and execution

Use the repository's normal Rust/dashboard build environment, Node.js 24,
pnpm 12.3.4, and Docker. On Linux (including NixOS):

```console
just setup-e2e
just test-e2e
just test-e2e --grep 'unsaved editor draft'
pnpm --dir e2e report
```

`setup-e2e` explicitly installs locked JavaScript development packages under
`e2e/node_modules` and pulls `mcr.microsoft.com/playwright:v1.63.0-noble` into
Docker. It does not install a system browser or change a Nix profile. There is no
Python or ChromeDriver dependency. Updating Playwright requires updating its exact
version in `package.json` and the lockfile; the runner derives the matching image
tag from that version.

`test-e2e` builds the CLI and Rust fixture, type-checks the tests, then starts a
short-lived browser container. Browser installation never happens implicitly
while running tests: the image must already exist. The container uses host
networking so `http://localhost:<port>` remains a WebAuthn secure context. This
wrapper targets Linux; Docker Desktop requires host networking support enabled.
It mounts only `e2e/` read-only and removes the container when the command exits.

The tests, CLI, and fixture run on the host; only the browser runs in the container.
Temporary state and Unix sockets are removed after each test. `CARGO_TARGET_DIR`
is supported; `PIQUELD_E2E_BIN_DIR` can override the directory containing the built
`piquelctl` and `examples/browser_fixture` executables.

## Coverage and diagnostics

- Initial setup, anonymous API rejection, discoverable passkey login, and logout.
- Assertion replay, substituted user handles, and user-verification downgrade.
- Transferable invitations, concurrent redemption, and cross-account management.
- CLI device approval, private credential files, automation tokens, and revocation.
- Account deletion confirmation, cancellation, and last-account protection.
- Application creation, persisted editing, and preservation of unsaved edits while
  reauthenticating after session revocation.

Passkey tests use Chromium's virtual CTAP2 authenticator through CDP. This exercises
real browser WebAuthn ceremonies, but does not validate physical authenticators or
Firefox/WebKit compatibility. Tests are independent and may run in parallel;
each receives its own daemon, database, browser context, and authenticator.

Tests use Playwright locators and retrying assertions, not fixed sleeps for UI
readiness. Retries of whole tests are disabled so failures remain visible. Failure
traces and screenshots are saved in `test-results/`; an HTML report is generated
in `playwright-report/`. CI uploads these for seven days. Traces may contain test
credential values: the suite only uses disposable fixture accounts, never live
service credentials.

For an already-provisioned Playwright browser server, the wrapper can be bypassed:
build the fixture/CLI as above, then run `PW_TEST_CONNECT_WS_ENDPOINT=ws://... pnpm
--dir e2e test`. The browser must be able to reach the fixture's localhost ports,
and the server's Playwright version must match the locked client version.
