# Authentication

This page describes how piqueld signs callers in. The user guide covers
[managing accounts](../../user/accounts/accounts.md),
[signing in from the CLI](../../user/accounts/cli-and-tokens.md), and
[recovering access](../../user/accounts/recovery.md); what callers may do
once signed in is described in [authorization](authorization.md).

The generated [OpenAPI contract](../../openapi-v1.json) documents `/api/v1/auth`:
status, current account and its grants, registration and login ceremonies, logout,
the account directory, management commands, device start/poll/inspect/approve
operations, the first-account `GET /auth/setup-link` (Unix socket only), and
the host operator's links: `POST /auth/recovery` and
`POST /auth/sign-in-link` (Unix socket only), and `POST /auth/login/operator`,
which redeems a sign-in link. `GET /auth/me` reports either a `user` or, for the
host operator, an `operator` with its Unix user ID. `POST /auth/manage` accepts a tagged `action`; actions on other
accounts follow the [account management](../../user/accounts/permissions.md#who-may-manage-accounts) rules.
Management responses never return existing credential secrets.
The device protocol uses the device-code interaction pattern; the JSON endpoints
are piqueld API contracts, not a general-purpose OAuth authorization server.

Authentication rejections use the same request tracing and paired `x-request-id`
header/error-body ID as other API responses. Authentication storage failures
return `503 storage_unavailable` with a log-correlated `details.diagnostic_id`;
they do not attempt to persist a diagnostic in the failing database. See
[observability](observability.md#history-and-diagnostics).

Authentication metadata shares the existing SQLite database. The reconciler's
periodic full scan deletes expired sessions, tokens, invitations and links
(`Store::prune_auth`), so it does not grow with every sign-in. WebAuthn challenges
and device requests are short-lived, bounded, server-side state; a daemon restart
cancels pending ceremonies without invalidating durable credentials. All
registration/login challenges are browser-bound and single-use. Registration
requires resident credentials and verified users; assertions are checked against
the exact configured origin, RP ID, credential, and immutable user handle.
The pinned `webauthn-rs-core` integration requires OpenSSL and pkg-config for native
builds; the Nix packages and development shell provide them.

## Origin and passkeys

`auth.public_url` is the single browser origin: WebAuthn, the links piqueld
prints, and the Origin checks below all use it. `Auth::validate_origin`
accepts a bare origin with a DNS hostname, over HTTPS or as
`http://localhost` or a subdomain of it, such as `http://dev.localhost`, which
browsers resolve to loopback and treat as a secure context. IP addresses,
paths, queries and fragments are refused.
No forwarded-header trust configuration exists: behind a reverse proxy, the
configured origin is what WebAuthn and CSRF checks compare against; restrict
access to the proxy's unencrypted upstream. Tailscale
transport encryption alone doesn't make an HTTP origin a secure browser
context, so remote browsers need an HTTPS hostname.

The relying-party ID is the origin's hostname, and origin matching is exact:
no subdomains or other ports. Existing passkeys don't work under an unrelated
hostname, and there is no automated hostname migration. With the
[tailnet node](../../user/reference/configuration.md#tailnet-node), its state
lives in the data directory, so moving or reinstalling the daemon with that
directory keeps the hostname, and with it the passkeys.

- Registration requires a resident (discoverable) key and user verification,
  and requests no attestation or authenticator attachment, so synced passkeys
  and security keys both work. Login lists no allowed credentials, so the
  authenticator offers any passkey for the relying party, and requires user
  verification.
- A ceremony, and the `piqueld_ceremony` cookie binding its finish request to
  the browser that started it, stays redeemable for 5 minutes
  (`CEREMONY_LIFETIME`).
- The user handle is the account's immutable ID (a UUID v7). Login looks the
  passkey up by credential ID and user handle, then verifies the assertion and
  updates its signature counter under the store's writer lock, in the
  transaction that opens the session. `webauthn-rs-core` refuses signature
  counter regressions, which can indicate a cloned authenticator.
- Redeeming a setup, invitation, enrollment or recovery secret consumes it in
  the same transaction that stores the passkey (and creates the account), and
  issues a browser session. A store refusal, such as a concurrent redemption,
  answers 400 `authentication_invalid`. Username uniqueness
  (`COLLATE NOCASE`) is checked when registration starts, before the browser
  creates the passkey, and enforced again there; a conflict answers 409
  `account_conflict` and leaves the invitation usable.
- Ceremony failures that say nothing about the caller's credentials answer
  400 `authentication_invalid`: a challenge that expired, was already used,
  or was started in another browser, a missing ceremony cookie, and a setup,
  invitation or enrollment link that can no longer be redeemed. 401 means the
  passkey or the signed-in session was refused. Host operator sign-in links,
  which sign in without a passkey, answer 401 when unusable.
- A scoped credential can't add a passkey, so `register/start` refuses it
  with 403, as the finish request would.
- An invitation or enrollment link is checked again at redemption against its
  issuer's current grants (see
  [authorization](authorization.md#links-act-with-their-issuers-authority)).
  Opening a link only reads its URL fragment: `Invite::take` in the dashboard
  removes it with `history.replaceState`, and nothing is consumed until the
  finish request.
- Usernames are 1–64 ASCII letters, digits, `.`, `-` or `_`; display names
  and passkey and token names are limited to 200 bytes.

## First-account setup

While no account exists, startup writes the setup link to
`<data_dir>/setup-link`, through a temporary file persisted with mode 0600,
and logs "first-account setup is open; run `piquelctl setup-link` to get its
link". `GET /auth/setup-link` serves it over the Unix socket to any client
that can connect, socket group members included; TCP listeners answer 404.
After setup it answers 409 `setup_completed`.

A restart before registration preserves the secret of a still-valid link and
rewrites the link for the current `auth.public_url`. If the file is missing at
startup, a new secret replaces the old one. The first registration creates
the account with `admin` on every application, its passkey, its browser
session, and the permanent closure of setup in one transaction; the file is
removed on the next startup. Setup never reopens, even if the user table is
emptied by hand.

Before setup, only the host operator can use the API beyond its public
endpoints.

## The host operator

A request over the API's Unix socket is from the
[host operator](../../user/accounts/recovery.md#the-host-operator) when
`SO_PEERCRED` (`peer_cred()`) reports root or the daemon's effective user ID,
and the request carries neither an `Authorization` nor a `Cookie` header, even
an unusable one. Nothing else the client sends can claim it. Socket group
members, TCP, the tailnet node and the dashboard gateway socket never qualify.
A stale saved token sent by root therefore answers 401 rather than falling
back to the host operator.

- `POST /auth/sign-in-link` requires that host operator: with any credential
  it answers 404 `endpoint_not_found`, like every caller that isn't the host
  operator. A link lasts 10 minutes (`OPERATOR_LINK_LIFETIME`); issuing one
  doesn't invalidate older ones, and unredeemed links are neither listed nor
  revocable. Redemption replaces the link's hash with the new session's, and
  the session lasts 12 hours (`OPERATOR_SESSION_LIFETIME`) with no idle
  timeout. Issuing logs a warning ("issued a host operator sign-in link") and
  raises `operator_sign_in_issued`; redemption logs "host operator signed in
  to the dashboard" at `info`.
- `POST /auth/recovery` requires the same host operator: with any
  credential, even an unusable one, it answers 404 `endpoint_not_found`.
  `piquelctl` sends none for `setup-link`, `recover-admin` and
  `sign-in-link`, so a stale saved login can't lock root out of recovery. It
  answers 409 `setup_pending` before the first
  account exists. The recovery link is a single row, replaced by every new
  one, valid for a day, and uses the `#invite=` fragment, so the dashboard
  shows the registration page. Issuing logs a warning and raises
  `admin_recovery_issued`; redemption raises `admin_granted`, with no actor,
  and logs the registration at `info`. The link is printed once and never
  logged.
- Only hashes of link and session secrets are stored.
- Actions that create something owned by or issued from an account
  (invitations, enrollment links, tokens, CLI login approvals) answer 400
  `authentication_invalid`: "the host operator is not an account; sign in with
  an account to do this".

## Credentials and cookies

Every credential secret is `pqd_` followed by 43 characters of unpadded
URL-safe base64 from 32 random bytes; 43-character secrets issued before the
prefix are still accepted. Anything else is refused without a database
lookup. Only SHA-256 hashes are stored. A credential's last-used time is
refreshed at most once a minute (`TOUCH_INTERVAL`), which keeps ordinary
requests read-only; a browser session also ends after a day unused. No
credential is renewed: expiry is fixed when it is issued. Logging out revokes
the credential of the current request; the host operator on the Unix socket
has none.

Browser sessions use HTTP-only, `SameSite=Strict` cookies. For HTTPS origins
they are also `Secure` and use the `__Host-` prefix, so applications on sibling
subdomains cannot set or shadow them. Browsers share cookies between the ports
of a host, so an origin with an explicit port adds it to the cookie names
(`__Host-piqueld_session_8443`, `piqueld_session_7845`), keeping daemons
served on different ports signed in separately. A bearer credential takes
precedence over the cookie.

`cross_site` in `api/http/auth.rs` refuses, with 403 `origin_mismatch`, on
every listener including the Unix socket:

- a mutation or WebSocket upgrade with any Origin other than the configured
  one;
- a cookie-authenticated mutation without an Origin;
- a registration or login request without the configured Origin, unless it
  sends none and carries a bearer credential that authenticates.

On TCP, including the tailnet node and the dashboard gateway, the browser
policy in `api/http/browser.rs` runs first. It refuses, with 403
`browser_access_denied`, requests with an untrusted Host, and unsafe-method
requests whose `Sec-Fetch-Site` is anything but `same-origin` or `none`,
`same-site` included. All API listeners require authentication regardless
of socket group membership or Tailscale connectivity; only the host operator
on the Unix socket needs none. Public endpoints are the status, setup-link,
recovery, sign-in-link, ceremony, operator login, and device start and poll
routes, plus TCP `/health`, the dashboard's static assets, and the separate
webhook socket. The daemon serves no metrics; it pushes them instead, see
[telemetry export](../../user/operations/telemetry.md).

## CLI device login

`device_start` creates a pending request for 10 minutes, keyed by the hash of
a secret 43-character device code that only the CLI knows; the 8-character
user code (`XXXX-XXXX`, without `I`, `O`, `0` or `1`) alone cannot retrieve
the credential. The request records the peer address, which the dashboard
shows as "network address …", or "the daemon's local Unix socket". Behind an
external reverse proxy, every request shows the proxy's address, so the
comparison cannot tell callers apart.

- The start body is limited to 16 KiB (`DEVICE_START_LIMIT_BYTES`) because
  pending logins live in memory; larger bodies answer 413
  `request_body_too_large`. The other sign-in endpoints accept 64 KiB.
- The CLI polls every 5 seconds; faster polls get `slow_down`, and each one
  adds 5 seconds. It stops after `min(expires_in, 15 minutes)`.
- Approval needs an account, through a credential with its full access, that
  holds any requested grants; both are checked again when the poll issues
  the 30-day CLI session, named `piquelctl`. If the approver's session ends
  before the next poll, no credential is issued. Approval doesn't create an
  account.
- One poll at a time issues the session, without holding the pending logins
  while it writes; a concurrent poll gets `authorization_pending`. The first
  poll after approval prepares the session's secret and ID and stores them.
  If that poll fails or is cancelled, the next one answers the same session
  when it is stored and live. Otherwise it can't tell a session never stored
  from one revoked since, so it refuses the login with 401 rather than
  resurrect a revoked credential.
- `piquelctl` checks that the daemon honoured requested limits: it compares
  the grants the start response echoes with the ones it asked for, and stops
  with "the daemon ignored the requested access; it predates limited logins"
  before showing the code. `token create` first decodes `GET /auth/me`, whose
  `scoped` field older daemons lack, and stops with "the daemon predates
  limited tokens, so none was created".

## The CLI's credentials file

`piquelctl` keeps logins in `credentials.json` (see
[saved logins](../../user/accounts/cli-and-tokens.md#saved-logins)), as
`{"endpoints":{KEY:{"selected":ID,"accounts":{ID:{"username":…,"token":…}}}}}`.
The key is the resolved URL without a trailing `/`, or `unix:<socket path>`,
compared as a string. Each update (login, logout) holds an exclusive lock on
the sibling `credentials.json.lock` across its read, change and write, which
serializes concurrent CLI processes, so don't delete it while one runs. Reads
take no lock, since writes go to a temporary file (mode 0600) that is synced
and renamed over the target. Reads refuse a file with any group or other
permission bit. Logging in again as the same account replaces the saved token
without revoking the old one. After `logout` removes the selected login, the
first remaining account, in ID order, becomes the default.

The client refuses remote HTTP authentication unless opted in with
`--allow-insecure-http` or, for library callers,
`Client::with_insecure_http()`: an `http` URL whose host isn't `localhost` or
a subdomain of it, `127.0.0.0/8` or `::1`, for a request that carries a bearer
token or targets `/api/v1/auth/…`. `Client::tcp` resolves `localhost` and its
subdomains to `::1` and `127.0.0.1` itself, whatever the system resolver says,
so those names always stay on the machine. The Unix socket uses an internal
`http://localhost` base URL and needs no opt-in. `setup-link`, `sign-in-link`
and `recover-admin` refuse any URL endpoint, including one from `PIQUELD_URL`
or a profile.

## Logging

Successful registrations ("registered passkey"), passkey logins ("signed in
with passkey"), device approvals ("approved CLI device login", with the
requesting peer address or `unix-socket`), and account-management actions
("account management action applied") are logged at `info` level with the
acting account. Rejected WebAuthn verifications, including signature-counter
regressions, are logged as warnings ("passkey verification failed") and
answer a plain 401; an unknown passkey or a mismatched user handle is refused
without a warning.

## Dashboard reauthentication

The browser client raises an event on any 401 outside the login and
registration ceremonies. `Gate` then shows "Your session has expired" over the
mounted dashboard, so unsaved editor changes stay; the host operator is told
to open a new sign-in link in another tab, then **Continue**. Signing in again
as the same identity resumes in place; another identity remounts the app,
dropping the previous one's data, drafts and one-time secrets. Failed
mutations are never replayed. Signing out treats a 401 as success, so it works
when the server has already invalidated the session.

## Rate limits

Public registration, login, and device-start requests, and redemptions of host
operator sign-in links, are limited to 30 starts per TCP peer per minute,
across all listeners. An unknown, used or expired sign-in link is turned away
by a read, without waiting for the store's writer. Requests refused for their Host,
Fetch Metadata, or Origin do not count. IPv6 peers are grouped by /64
prefix, IPv4-mapped addresses count as IPv4, and Unix socket callers share one
peer bucket. Excess requests receive
HTTP 429 `authentication_busy` and `Retry-After: 60`; ongoing ceremonies and authenticated API work are
unaffected. There is no daemon-wide limit, so one caller cannot block sign-in for
everyone else. Client-supplied forwarded IP headers are deliberately ignored: callers behind the
same external reverse proxy share its allowance. The daemon's own tailnet node
and [dashboard hostname](../../user/guides/ingress.md#dashboard-hostname) gateway pass each
client's tailnet address. Limits reset on daemon restart, along
with pending ceremonies and device requests.

Never expose piqueld to the internet, including through a reverse proxy. These
limits protect a trusted network from accidents, not a public endpoint from
abuse. Pending ceremonies and device requests have fixed in-memory capacities,
1024 each (`MAX_PENDING`); when one is full, starts answer the same 429,
without `Retry-After`.

## Tests

Browser authentication and CLI device-login coverage lives in the
[Playwright browser suite](https://github.com/piquel-fr/piqueld/blob/main/tests/playwright/README.md), including session revocation and
preservation of unsaved editor drafts during reauthentication.
