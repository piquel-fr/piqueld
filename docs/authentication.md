# Authentication

piqueld uses passkeys for browser login. Every account has the same capabilities:
any authenticated user can edit or delete any other account, enroll or remove
its passkeys, create its API tokens, and revoke its sessions. There are no roles,
ownership restrictions, or extra authentication prompts for these changes.
The first account has no special privileges. There is no account recovery flow.

## Upgrading an existing installation

1. Choose the stable HTTPS hostname and configure `auth.public_url` before creating
   any passkeys. Arrange TLS termination and verify that the browser can reach it.
   Tailscale users still need HTTPS for the browser; see the transport notes below.
2. Stop the daemon and back up its entire data directory, including `piqueld.db`
   and any SQLite `-wal`/`-shm` files. Keep the previous binary and configuration.
   The new daemon migrates the database on startup; an older binary rejects the
   newer schema. Replacing the binary alone is not a supported rollback.
3. Start the upgraded daemon, open its private `setup-link`, and create the first
   account. Existing applications continue reconciling, but all API clients now
   need credentials, including clients connecting over a Unix socket.
4. Run `piquelctl login` for interactive clients. Create automation tokens and
   update scripts, deployment jobs, and API health checks that previously used
   anonymous access. TCP `/health` remains public. Verify a browser edit and an
   authenticated CLI command before declaring the upgrade complete.

To roll back, stop the new daemon and restore the complete pre-upgrade data
directory, previous binary, and configuration together. This discards database
changes made since the backup, including accounts and application edits. Running
Docker resources may have changed in the meantime; check them against the restored
desired state before restarting reconciliation. Never restore a live database or
combine a restored database with WAL files from the newer installation.

## Website and initial setup

Build with the embedded dashboard (`just daemon-embedded` or the combined Nix
package). Configure one stable browser origin:

```toml
[auth]
public_url = "https://piqueld.example.com"
```

Terminate HTTPS at an external reverse proxy and forward requests to a configured
piqueld listener. No forwarded-header trust configuration is needed: WebAuthn and
CSRF checks use the explicitly configured origin. Restrict access to the proxy's
unencrypted upstream as appropriate for your deployment. `http://localhost:7845`
is the development default. Use `localhost`, not an IP address, for browser login.
Tailscale transport encryption alone does not make an HTTP website a secure
browser context; remote browser access still needs an HTTPS hostname.

Before the first account exists, the API exposes only authentication/setup
endpoints (plus static website assets and TCP `/health`). Startup writes a private
`<data_dir>/setup-link` file, mode `0600`. Open that link, choose a username and
optional display name, and register a passkey. The account, passkey, and permanent
closure of initial setup commit together. The link becomes invalid immediately;
the file is removed on the next startup. A restart before registration preserves
the valid link. Initial setup never reopens automatically.

Passkeys use discoverable credentials, so subsequent login starts directly with
“Sign in with a passkey”; typing a username is unnecessary. Authenticators must
support resident credentials and user verification (PIN/biometrics). Synced
passkeys and compatible security keys are supported. The configured hostname is
part of the passkey identity: existing passkeys do not work under an unrelated hostname. Automated hostname
migration is outside this version.

## Accounts and invitations

The dashboard's **Accounts** page manages all accounts and credentials. Account
names are unique without regard to case, editable, and contain 1–64 ASCII letters,
digits, dots, dashes, or underscores. Internal account IDs never change.

Any user can create an invitation. Copy and share its link; no recipient or email
address is attached. The first person to complete registration chooses their own
account details. Links expire after 24 hours and can be revoked by any user.
Opening a link does not consume it. Deleting its issuer revokes pending links.

Removing a passkey prevents future logins with that credential and leaves existing
sessions/tokens intact. **Revoke all sessions and tokens** is a separate action.
Deleting an account revokes everything belonging to it. Self-deletion is supported,
but the final account cannot be deleted. If everybody loses their passkeys and
all sessions/tokens become unusable, there is no supported recovery mechanism.

## CLI and automation

Login works over TCP or the Unix socket, including when the CLI runs over SSH:

```console
piquelctl --url https://piqueld.example.com login
piquelctl --url https://piqueld.example.com whoami
piquelctl --url https://piqueld.example.com status
piquelctl --url https://piqueld.example.com logout
```

Remote HTTP authentication is rejected by default, including device login.
For an API connection protected separately by Tailscale, explicitly opt in with
`piquelctl --allow-insecure-http --url http://<tailnet-host>:7845 login` (and use
that flag for subsequent commands). This does not enable TLS; the caller must
ensure transport encryption. HTTPS, loopback HTTP, and Unix sockets need no opt-in.
The browser still uses the configured HTTPS `auth.public_url` for passkeys.
Library callers can opt in with `Client::with_insecure_http()`.

`login` prints a browser URL and code. Sign in with a passkey at that URL, enter
the code from your terminal, and explicitly approve the CLI. The pending request
expires after ten minutes. The CLI polls using a separate secret; the displayed
code alone cannot retrieve its credential. Approval does not create an account;
an invitation must be redeemed first.

Credentials are stored separately from connection profiles, in
`$XDG_CONFIG_HOME/piqueld/credentials.json` (default
`$HOME/.config/piqueld/credentials.json`), mode `0600`. Override its location with
`PIQUELD_CREDENTIALS_FILE`. Saved credentials are keyed by endpoint and immutable
account ID; the most recent login selects the default account for that endpoint.
Use `--account USERNAME_OR_ID` to select another saved login. TCP and Unix endpoints
have separate entries. Login again after renaming an account to update its saved
username, or select it by immutable ID. `logout` revokes the credential remotely
and removes its local copy.
Credential updates use a persistent sibling `.lock` file to serialize concurrent
CLI processes. Do not delete that lock file while CLI commands are running.

Create named automation tokens on the Accounts page. The raw token appears only
once; the daemon stores only its hash. Supply it using `PIQUELD_TOKEN`, which takes
precedence over saved credentials. Tokens have the same capabilities as their
account. Logging out with `PIQUELD_TOKEN` revokes that token and leaves saved logins
alone. Do not put token values into connection profiles or Nix configuration.

| Credential | Expiry |
| --- | --- |
| Browser session | 24 hours without API use, or 7 days total |
| CLI session | 30 days; repeat browser login afterward |
| Automation token | 90 days by default; custom days or no expiry |

Browser sessions use HTTP-only, SameSite=Strict cookies, with Secure enabled for
HTTPS origins. Cookie-authenticated mutations require the configured Origin.
API/CLI credentials use `Authorization: Bearer …`. No tokens are automatically
renewed. All API listeners require account authentication, regardless of socket
group membership or Tailscale connectivity.

If a browser request reports an expired or revoked session, the dashboard offers
passkey login in place. Unsaved editor changes stay mounted. After signing in,
retry the failed action; mutations are never replayed automatically. Signing out
still works when the server has already invalidated the session.

## API and implementation

The generated [OpenAPI contract](openapi-v1.json) documents `/api/v1/auth`:
status, current account, registration and login ceremonies, logout, the account
directory, management commands, and device start/poll/approve operations.
`POST /auth/manage` accepts a tagged `action`; all authenticated accounts can use
all actions. Management responses never return existing credential secrets.
The device protocol uses the device-code interaction pattern; the JSON endpoints
are piqueld API contracts, not a general-purpose OAuth authorization server.

Authentication metadata shares the existing SQLite database. WebAuthn challenges
and device requests are short-lived, bounded, server-side state; a daemon restart
cancels pending ceremonies without invalidating durable credentials. All
registration/login challenges are browser-bound and single-use. Registration
requires resident credentials and verified users; assertions are checked against
the exact configured origin, RP ID, credential, and immutable user handle.
The pinned `webauthn-rs-core` integration requires OpenSSL and pkg-config for native
builds; the Nix packages and development shell provide them.

Public registration, login, and device-start requests share limits of 30 starts
per TCP peer per minute and 60 per daemon per minute, across all listeners. Unix
socket callers share one peer bucket. Excess requests receive HTTP 429 and
`Retry-After: 60`; ongoing ceremonies and authenticated API work are unaffected.
The global budget keeps admitted pending requests below the in-memory capacity.
Forwarded IP headers are deliberately ignored: callers behind the same reverse
proxy share its allowance. On publicly reachable deployments, also apply per-client
limits at the trusted proxy; sustained or distributed traffic can still consume
the daemon's admission budget. Limits reset on daemon restart, along with pending
ceremonies and device requests.
Browser authentication and CLI device-login coverage lives in the
[Playwright end-to-end suite](../e2e/README.md), including session revocation and
preservation of unsaved editor drafts during reauthentication.
