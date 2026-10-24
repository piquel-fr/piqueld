# Authorization

This page describes how piqueld decides what a signed-in caller may do. The
user guide's [permissions](../../user/accounts/permissions.md) page lists what
each permission allows; [authentication](authentication.md) covers how callers
sign in.

## Grants

`piqueld_core::access` holds the model, shared by the daemon, the dashboard
and the CLI:

- `Permission` is `admin`, an `AppPermission`, or a `GlobalPermission`. The
  latter two each come from a `permissions!` table of wire names and
  descriptions, and `Permission::description` adds `admin`'s. The dashboard's
  grant editor shows each description next to its checkbox.
- A `Scope` is every application (including later ones) or a set of
  application IDs. Installation-wide permissions can only have the whole
  installation as their scope, which the types enforce.
- `Grants` is a validated set in canonical form, so equal access compares
  equal: grants for one permission merge, nothing `admin` (or `apps:deploy`
  and `apps:delete`, for their previews permissions) already covers is listed
  again, and `apps:read` lists every application readable through other
  application permissions. On the wire it is a list of `Grant`s, each with an
  `applications` list that is absent for every application.
- `Preset`s only fill in grants; what is stored is the explicit list.
  `read-only` and `developer` add `system:read` when their scope is every
  application.

An account's access is the union of its grants. A credential's access is its
own grants, if any, intersected on every request with its account's current
grants (`Grants::intersection`), so demoting an account demotes its tokens.

## Checking a request

- `Grants::require_app` checks one permission on one application. An
  application the caller cannot read at all is `Denied::Hidden`, answered
  like a missing application (404), so its existence doesn't leak; a readable
  one without the permission is `Denied::Missing`, 403 `permission_denied`
  with the permission in `details.permission`.
- `Grants::require_change` checks a mutation's permissions against a
  `Target`:
  - `Target::New` requires `apps:create` in place of `apps:write`, and every
    other required permission on some application, such as `apps:deploy` to
    create and deploy in one step, or to enable deploying on push.
  - `Target::Named` is an existing application named by the caller, such as
    in a manifest. One the caller cannot read is refused as if it were being
    created: 403 naming the first permission creating it would need and the
    caller lacks, `apps:create` or, for a change that deploys, `apps:deploy`,
    and `apps:write` for callers holding both. So callers who could not
    create the application cannot tell taken names apart.
  - `Target::Preview` lets previews permissions stand in for the permissions
    they cover (`AppPermission::on_previews`): `previews:deploy` reads a
    preview's logs. Deploying and deleting previews have their own routes,
    which require the previews permissions directly; reconciling a preview, or
    deploying it through `/environments/{id}/deploy`, still needs
    `apps:deploy`, and `DELETE /environments/{id}` `apps:delete`.
  - `Target::Unknown`, an application or environment that does not exist or a
    malformed ID, passes only for callers holding the permission on every
    application, and is otherwise hidden.
- Requests about an environment are checked on its application, so a grant on
  an application covers its environments and previews, including later ones.
- `Grants::may_change_account` approves a change to another account only with
  `accounts:manage` and every grant the target holds (`Grants::covers`);
  `Grants::may_grant` approves handing out grants the caller holds. `admin` is
  only covered by `admin`, because it includes permissions added later.
  Lacking `accounts:manage` answers 403 `permission_denied`; a failed
  `covers` check answers 403 `permission_exceeded`.

Routes with an application `{id}` check the permission on it; routes without
one (analytics, operations) need the permission on some application and
narrow their results to `Grants::app_scope`. The OpenAPI document names each
operation's requirement in its `x-piqueld-access` extension: `public`,
`authenticated`, or one permission. `authenticated` operations check
permissions in their handlers: apply and plan, events and the event stream,
diagnostics, notification destinations, the audit trail, builds, and the
account directory, management and device approval. The extension names a
single permission, so it omits combined checks: granting telemetry also needs
`apps:read`, and saving with `deploy=true` also needs `apps:deploy`.

## What callers see

Applications a caller cannot read are left out of lists, history, builds,
analytics, the event stream, the ingress routes in system readiness
(`service/system.rs`), and the preview counts in system status;
`preview_limit_reached` details list only readable previews. Daemon-scoped
history and diagnostics require `system:read`; application history requires
`events:read` on that application. Event IDs and stream checkpoints are
installation-wide, and the stream sends ID-only messages to advance past
filtered-out events, so a reader can tell that hidden events happened, but not
what or where.

Some installation-wide views are not filtered by application: notification
delivery subjects and destinations' application selections, for holders of
`system:read` or `notifications:manage`, and the audit trail's targets, for
holders of `audit:read`. `notifications:manage` alone also lists
destinations, and a destination may select any application ID.

## Writes re-read access in their transaction

Grants are read at every authentication. Every write (account changes,
passkeys, tokens, application mutations, secrets, key recovery, notification
destinations and retries) also re-reads the caller's credential and grants
inside its transaction, so a request that authenticated just before a
demotion, revocation, or expiry cannot use the access it lost. Revoking a
credential, setting grants and deleting an account commit through
`commit_access_change`, which notifies long-lived connections.

## Long-lived connections

Event streams and `apps:exec` command sessions are authorized when they open;
a `Lease` (`api/http/lease.rs`) then rechecks the caller's access while they
last. Its first recheck is due at once, so a change made between
authentication and subscribing is not missed. The lease polls every second
(`TICK`) and rechecks when an access change was notified, or 30 seconds
(`RECHECK`) after the previous recheck started. The periodic recheck catches
what isn't notified: an expired credential, grants removed with a deleted
application, creator grants, and a tailnet peer that no longer matches a
token's binding, which each recheck looks up afresh.

- A stream switches to the caller's new visibility, so it stops sending
  history the caller may no longer read; once the caller loses access
  entirely, it ends with an `access_ended` message whose data is the
  `ErrorBody` the request would now get.
- A command session that lost access ends with a `Failed` frame carrying the
  refusal, such as 401, 403, or 404 once the application is hidden, which is
  audited. A 503 `tailnet_unavailable` from a bound token's lookup ends it
  too, unaudited. Like a disconnect, this only stops the stream: the command
  itself may keep running in the task.
- Both end, without a message, when the daemon shuts down.

## Limited credentials

API tokens are always limited (`scoped`), even with `admin` on every
application; CLI logins are limited whenever they asked for any grants, even
ones equal to the account's full access. A limited credential is refused with
403 `credential_scoped` (`Denied::Scoped`) when it would create credentials
(tokens, passkeys, invitations, enrollment links, CLI login approvals), set
grants on any account, or change its own account. It may revoke itself. With
`accounts:manage` and a covering scope, it can still edit other accounts'
profiles, delete them, remove or rename their passkeys, and revoke their
credentials and invitations. A limited credential without `audit:read` reads
only its own requests in the audit trail, rather than its account's.

Deleting an application removes the grants on it once the application row is
finally deleted, after its last environment. Limited credentials, tokens and
CLI logins alike, left without any grant are then revoked.

## Creator grants

Creating an application requires `apps:create`. `Grants::for_created_application`
gives the creator, on the new application, every application permission (and
`admin`) it holds on some but not all applications, so it can work on what it
created without gaining any new kind of ability. Permissions it already holds
everywhere need nothing new. An account whose only permission is `apps:create`
therefore receives nothing and cannot read the application it created.
Applications created through a limited credential give no creator grants,
since those credentials hand out no lasting access.

## Links act with their issuer's authority

Invitation and enrollment links store their issuer. At redemption, the store
checks that the issuer could still create the link: still holds
`accounts:manage` and every grant the invitation hands out, or may still
change the enrollment's target account. A refused link answers a plain 401
`authentication_required`, like an expired one, and stays listed until it
expires. Creating a link needs a credential with its account's full access.
Revoking one needs the authority to have created it, and a full-access
credential only for an enrollment link to the caller's own account, like every
change to it. Deleting the issuer deletes its links (`ON DELETE CASCADE`), and
deleting an enrollment's target account deletes its enrollment links.

## Lockout protection

Setting grants, deleting an account and removing a passkey are refused with
409 `account_lockout` when no account would keep `admin` on every application
together with a passkey (`store/auth/access.rs`). The host operator is not an
account and never counts, and the rule applies to its changes too. Revoking a
host operator browser session requires `accounts:manage` and `admin` on every
application.

## Links between applications

[Links](../../user/reference/manifest.md#links) have no permission checks of
their own: exports and imports are configuration edits (`apps:write`), and
deploying them needs `apps:deploy` (`previews:deploy` for a preview). A
preview's imports come from its branch or local snapshot, so whoever may
deploy previews chooses them. An export names applications, not IDs: an
unbound name matches whichever application holds it until the exporter's next
deployment binds it, and a deleted application's binding passes to its name's
next holder only at the exporter's next deployment. Link reports read
`unavailable` for both a missing environment and one not exported to the
importer, and importer lists are filtered to readable applications.

## Telemetry receivers

`PUT /api/v1/environments/{id}/telemetry` requires `telemetry:grant` on the
route and `apps:read` in the mutation. Without `telemetry:grant` it answers
403 even for hidden applications; with it, hidden applications and previews
answer 404. Revoking stops export before the response. Telemetry status lists
only the receivers of readable applications.

## Tailnet-bound tokens

A token may carry a `TailnetBinding`: a login name (containing `@`, up to 256
bytes) or `tag:` followed by letters, digits and dashes. Creating one needs
the daemon's tailnet node (`tailscale.enabled`) or a dashboard hostname
(`TokenPolicy::tailnet`), since it could never be used otherwise. For a bound
token, `FreshTailnet::check` looks the connection's peer up afresh through the
node's `tailscaled` (the apps node's, for the dashboard hostname) on every
request, again when an `app exec` command arrives, and on every lease recheck.
A lookup that times out answers 503 `tailnet_unavailable`: `LOOKUP_TIMEOUT`,
5 seconds, applies both to waiting behind another lookup of the same address
and to its own query, so the worst case is about 10 seconds. Any other
mismatch, including requests from the Unix socket or other listeners,
which have no peer, answers 401 `tailnet_binding_mismatch`, and the refusal is
audited. Tagged nodes have no login, so a user binding never matches them.
CLI logins cannot be bound.
