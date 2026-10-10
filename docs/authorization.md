# Authorization

Every API request is made by a signed-in account through one credential: a
browser session, a CLI login, or an API token. What the request may do is
decided by the account's **grants**. See [authentication](authentication.md)
for how accounts sign in.

## Grants

A grant gives one permission, either on every application (including ones
created later) or only on listed applications:

```text
admin                          every ability, including permissions added later
admin on blog                  every application permission on blog
apps:deploy on blog, shop      deploy these two applications
system:read                    installation-wide; never limited to applications
```

An account's access is the union of its grants. A grant on an application
covers each of its [environments](application-manifest.md), including ones
added later: requests about an environment are checked on its application.

| Permission | Scope | Allows |
| --- | --- | --- |
| `admin` | all or listed applications | On every application: everything, including installation-wide permissions. On listed applications: every application permission there |
| `apps:read` | all or listed applications | Configuration, environments, status, deployments, operations, builds and job runs, and secret names and which environments may mount them |
| `apps:write` | all or listed applications | Configuration edits, manifest apply, rename, and adding or renaming environments |
| `apps:deploy` | all or listed applications | Deploy and reconcile environments, create and deploy previews |
| `apps:delete` | all or listed applications | Delete the application, its environments or its previews |
| `apps:exec` | all or listed applications | Run commands in running containers (`piquelctl app exec`), which reaches everything those containers can, including secret values |
| `secrets:write` | all or listed applications | Store, regenerate, and delete secret values, and choose which environments may mount a stored secret |
| `logs:read` | all or listed applications | Runtime, build and job logs, which may contain sensitive output |
| `events:read` | all or listed applications | Application history, diagnostics, and deployment analytics |
| `apps:create` | installation | Create applications |
| `system:read` | installation | Daemon configuration, resources, daemon history, notification deliveries, and DNS providers and certificates |
| `system:operate` | installation | Secret key recovery, notification retries, and DNS provider checks |
| `accounts:manage` | installation | Other accounts and invitations, within the limits below |
| `audit:read` | installation | Every account's [audit trail](observability.md#audit-trail); everyone reads their own |

Every application permission also allows reading that application, so a
deploy-only token can follow its own deployment. The daemon status, readiness,
and `OpenAPI` document are available to every signed-in account. The generated
[OpenAPI document](openapi-v1.json) names each operation's requirement in its
`x-piqueld-access` extension: `public`, `authenticated`, or a permission.

## What callers see

- Applications a caller cannot read do not exist for it: they and their
  environments answer 404 and are left out of lists, history, builds,
  analytics, the event stream, and the ingress routes in system readiness.
- On an application it can read, a missing permission answers 403
  `permission_denied` with the permission in `details.permission`.
- Saving a manifest by name for an existing application the caller cannot read
  is refused with 403 rather than reported missing.
- Daemon-scoped history requires `system:read`; application history requires
  `events:read` on that application.
- Event IDs and stream checkpoints are installation-wide, so a reader can tell
  that hidden events happened, but not what or where.

## Creating applications

Creating an application requires `apps:create`. The creator then receives, on
the new application, every application permission (and `admin`) it holds on
some but not all applications, so it can work on what it created without
gaining any new kind of ability. Permissions it already holds everywhere need
nothing new. Creating and deploying in one step also requires `apps:deploy`
somewhere. Applications created through an API token or a limited CLI login
give no creator grants, since those credentials hand out no lasting access.

## Managing accounts

Every account may manage itself: edit its profile, add and remove its own
passkeys, see and revoke its own sessions and tokens, and create tokens for
itself.

Changing another account (profile, grants, passkeys, credentials, deletion, or
an enrollment link) requires `accounts:manage` **and** every grant that account
holds. Grants can only be handed out by someone who holds them, whether on an
account or an invitation. So a "team lead" with
developer access and `accounts:manage` can manage developers but not
administrators, and administrators can manage each other. Refusals for
exceeding your own access answer 403 `permission_exceeded`.

Passkeys are only added by their owner. To help someone else, create an
enrollment link: a single-use, 24-hour link that adds a passkey to that
account and signs it in.

Invitation and enrollment links act with their issuer's authority. A link is
refused if, by the time it is redeemed, its issuer could no longer create it:
for example after the target account was promoted beyond the issuer, or the
issuer lost `accounts:manage`. Revoking a link requires the same authority
as creating it.

At least one account must keep `admin` on every application together with a
passkey. Changes that would remove the last such account are refused with 409
`account_lockout`. If every administrator still loses access, the host's
operator can [recover it](authentication.md#recovering-administrator-access).
The [host operator](authentication.md#the-host-operator) itself is not an
account: it holds `admin` on every application without counting as one, and
these account rules apply to the accounts it changes.

Without `accounts:manage`, the account directory lists only the caller's own
account.

## Invitations and presets

Invitations state the grants of the account they create; there is no default
access. The dashboard and `piquelctl account` offer presets that only fill in
the grant selection and are never stored:

| Preset | Grants |
| --- | --- |
| `read-only` | `apps:read`, `logs:read`, `events:read` (and `system:read` on every application) |
| `deploy` | `apps:read`, `apps:deploy` |
| `developer` | every application permission except `apps:delete` (and `system:read` on every application). This includes `apps:exec`: deploying their own configuration already reaches everything a command could |
| `admin` | `admin` |

```console
piquelctl account invite --preset developer --app blog --app shop
piquelctl account access bob --preset read-only
piquelctl account access ci --permission apps:deploy --app blog --yes
piquelctl account enroll bob
piquelctl account list
```

## API tokens

An API token acts with its own grants, limited on every request by its
account's current access: demoting the account demotes its tokens. Tokens are
created for your own account only, with grants you hold. Token secrets start
with `pqd_`, so secret scanners can find leaked ones.

Credentials with limited access (API tokens and CLI logins that asked for less
access) cannot create credentials or hand out access: no tokens, passkeys,
invitations, enrollment links, CLI login approvals, or grants for other
accounts. They also cannot change their own account, which needs no
permission, though they can revoke themselves. A leaked token can therefore
never outlive or exceed its own limits.

```console
piquelctl token create ci --preset deploy --app blog --days 30
piquelctl token create reader --permission apps:read --no-expiry
piquelctl token list
piquelctl token revoke <id>
```

`auth.max_token_days` limits the lifetime of new tokens; see
[configuration](configuration.md#authentication-origin). Deleting an
application removes grants on it; tokens left without any grant are revoked.

A CLI login can ask for less than your access with the same options;
`--app` takes application IDs there, since names cannot be looked up before
signing in. The approval page shows the requested access, and the approver
must hold it. The request is limited to 16 KiB, which fits a preset on a few
dozen applications; beyond that, ask for every application instead of listing them:

```console
piquelctl login --preset read-only
```

Daemons older than these limits would ignore them and issue full access, so
`piquelctl` checks that the daemon supports them before asking for a limited
token or login.

### Tailnet-bound tokens

With the daemon's [tailnet node](configuration.md#tailnet-node) enabled, a
token can be bound to a tailnet user or tag. It is then accepted only through
that node, from a device signed in as that user or carrying that tag, as
reported by the node's `tailscaled` on every request (and again when an
`app exec` command starts), so removing a tag or signing a device out takes
effect immediately. Anywhere else, including the Unix socket,
plain TCP listeners, and other tailnet devices, it is refused with 401
`tailnet_binding_mismatch`, and the refusal is audited. A leaked CI token is
then useless outside the tailnet's CI runners.

```console
piquelctl token create ci --preset deploy --app blog --tailnet tag:ci
piquelctl token create laptop --preset read-only --tailnet alice@example.com
```

Tagged devices belong to no user, so a user binding never matches them; bind
those tokens to a tag. Creating a bound token without the tailnet node is
refused, since it could never be used.

## Upgrading

The first account created during setup receives `admin` on every application.
Upgrading an existing installation gives every existing account `admin` on
every application, so access is unchanged until you reduce it, including
`apps:exec`. Existing tokens keep acting with their account's full access, but
like every token they can no longer create credentials. Invitations created before the
upgrade create accounts without access; grant it after they register.

Grants are evaluated on every request, so changes take effect immediately for
the account's sessions and tokens. Every write (account changes, passkeys,
tokens, application mutations, secrets, key recovery, and notification retries)
re-reads the caller's credential and grants inside its transaction, so a request
that authenticated just before a demotion, revocation, or expiry cannot use the
access it lost. Event streams opened before a change keep
their original visibility until they reconnect. A command started with
`apps:exec` runs until it ends, but a connection authorized before the change
cannot start one.
