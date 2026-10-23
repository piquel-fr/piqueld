# Authentication

The generated [OpenAPI contract](../../openapi-v1.json) documents `/api/v1/auth`:
status, current account and its grants, registration and login ceremonies, logout,
the account directory, management commands, device start/poll/inspect/approve
operations, the first-account `GET /auth/setup-link` (Unix socket only), and
the host operator's links: `POST /auth/recovery` and
`POST /auth/sign-in-link` (Unix socket only), and `POST /auth/login/operator`,
which redeems a sign-in link. `GET /auth/me` reports either a `user` or, for the
host operator, an `operator` with its Unix user ID. `POST /auth/manage` accepts a tagged `action`; actions on other
accounts follow the [authorization](../../user/accounts/authorization.md#managing-accounts) rules.
Management responses never return existing credential secrets.
The device protocol uses the device-code interaction pattern; the JSON endpoints
are piqueld API contracts, not a general-purpose OAuth authorization server.

Authentication rejections use the same request tracing and paired `x-request-id`
header/error-body ID as other API responses. Authentication storage failures
return `503 storage_unavailable` with a log-correlated `details.diagnostic_id`;
they do not attempt to persist a diagnostic in the failing database. See
[observability](../../user/guides/observability.md#history-and-diagnostics).

Authentication metadata shares the existing SQLite database. WebAuthn challenges
and device requests are short-lived, bounded, server-side state; a daemon restart
cancels pending ceremonies without invalidating durable credentials. All
registration/login challenges are browser-bound and single-use. Registration
requires resident credentials and verified users; assertions are checked against
the exact configured origin, RP ID, credential, and immutable user handle.
The pinned `webauthn-rs-core` integration requires OpenSSL and pkg-config for native
builds; the Nix packages and development shell provide them.

Public registration, login, and device-start requests are limited to 30 starts
per TCP peer per minute, across all listeners. Requests refused for their Host,
Fetch Metadata, or Origin do not count. IPv6 peers are grouped by /64
prefix, and Unix socket callers share one peer bucket. Excess requests receive
HTTP 429 and `Retry-After: 60`; ongoing ceremonies and authenticated API work are
unaffected. There is no daemon-wide limit, so one caller cannot block sign-in for
everyone else. Client-supplied forwarded IP headers are deliberately ignored: callers behind the
same external reverse proxy share its allowance. The daemon's own tailnet node
and [dashboard hostname](../../user/guides/ingress.md#dashboard-hostname) gateway pass each
client's tailnet address. Limits reset on daemon restart, along
with pending ceremonies and device requests.

Never expose piqueld to the internet, including through a reverse proxy. These
limits protect a trusted network from accidents, not a public endpoint from
abuse. Pending ceremonies and device requests have fixed in-memory capacities.

Browser authentication and CLI device-login coverage lives in the
[Playwright browser suite](https://github.com/piquel-fr/piqueld/blob/main/tests/playwright/README.md), including session revocation and
preservation of unsaved editor drafts during reauthentication.
