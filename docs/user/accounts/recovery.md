# Recovering access

Get back into piqueld when someone loses their passkey, or when no
administrator can sign in at all. The last resort is the host operator, who
controls the daemon's host.

## The host operator

Whoever controls the daemon's host already controls piqueld: they can read
and replace its database and its secret key, and drive its Docker engine.
piqueld calls them the **host operator**, and lets them act without an
account, which is simpler for them and leaves a clearer trail than editing the
database. That grants nothing they don't already have, so there's no setting
to turn it off.

On the host, `piquelctl` acts as the host operator when root or the daemon's
own user runs it, over the daemon's Unix socket:

```console
$ sudo piquelctl whoami
host operator (uid 0)
  admin on every application
```

- Members of the `piqueld` group aren't the host operator: they sign in with
  their own account. Over the network, nobody is.
- With a saved login or `PIQUELD_TOKEN`, `piquelctl` acts as that account,
  even when root runs it.

The host operator holds `admin` on every application, so it may do what an
administrator does, except what needs an account. It has no passkeys or
tokens, can't create invitation or enrollment links, and can't approve CLI
logins. It isn't an account, so it doesn't count as one either: some account
must still keep `admin` and a passkey.

piqueld's history and [audit trail](../guides/observability.md#audit-trail)
attribute what it does to the host operator and its Unix user, such as
`host operator (uid 0)`, and to its browser session, if any.

## Sign a browser in as the host operator

On the host, print a sign-in link:

```console
$ sudo piquelctl sign-in-link
Warning: this link signs a browser in as the host operator, with full access, for 12 hours; it works once within 10 minutes. It was recorded as a security event, which notifies administrators when security notifications are enabled.
https://piqueld.<tailnet>.ts.net/dashboard/auth#operator=…
```

Open the link and select **Sign in as host operator**. The dashboard shows
**Host operator** as the signed-in user, and hides what needs an account. The
link also works before the first account exists, so you can try a new
installation from a browser.

Every link raises a
[`security` notification](../guides/observability.md#security-notifications),
"A host operator sign-in link was issued over the Unix socket to uid 0", and
the daemon logs a warning.

Accounts with `accounts:manage` see these browsers under **Host operator
sessions**, on the **Accounts** page, and in `piquelctl account list`:

```text
host operator (uid 0) browser session 01a16b51-bcb2-7800-87b6-bb2605a22b2d (admin, expires 1792845081)
```

To end one, select the sign-out button next to **Host operator** at the
bottom of that browser's sidebar, select **Revoke** on the **Accounts** page,
or run `piquelctl token revoke` with its ID. Revoking one needs
`accounts:manage` and `admin` on every application, since it acts with that.

## Someone lost their passkey

First, they get a new passkey on their account, the first way that works:

- **They still have another passkey, or a browser still signed in as their
  account.** On their own card, they
  [add a passkey](accounts.md#to-your-own-account).
- **`piquelctl` is still signed in as their account, with its full access.**
  On that machine, they create an enrollment link for their own account,
  which needs no `accounts:manage`:

  ```console
  piquelctl account enroll bob
  ```

  They open the link themselves, in a browser, and register a passkey, which
  also signs that browser in. The link signs in whoever uses it, so they
  don't share it.
- **Otherwise,** an administrator
  [sends them an enrollment link](accounts.md#to-someone-elses-account). That
  needs `accounts:manage` and every grant of their account.

API tokens and [limited logins](cli-and-tokens.md#ask-for-less-access) can't
add passkeys or create enrollment links. If nobody who can sign in may send
the link, see the next section.

Then, signed in as their account or as that administrator:

1. On their card on the **Accounts** page, select **Remove** next to the lost
   passkey.
2. If the passkey may have been stolen, select **Revoke all** under
   **Sessions and API tokens**. Removing a passkey doesn't end the sessions it
   opened.

## No administrator can sign in

When every administrator has lost their passkeys, and none is still signed
in to a browser or to `piquelctl` with their account's full access, the host
operator creates a new administrator. Their API tokens and limited logins
can't help: they can't add passkeys or create links.

1. On the host, print a recovery link:

   ```console
   $ sudo piquelctl recover-admin
   Warning: this link creates a new administrator account and works once within 24 hours; open it in a browser and register a passkey. It was recorded as a security event, which notifies administrators when security notifications are enabled.
   https://piqueld.<tailnet>.ts.net/dashboard/auth#invite=…
   ```

   Whoever completes the link first becomes an administrator. If you send it
   to someone, use a private channel you trust. If anyone else may have seen
   it, run `recover-admin` again, which replaces it.

2. Open the link. On **Create your account**, choose a username, and select
   **Create account with a passkey**. The new account has `admin` on every
   application, and the dashboard opens, signed in.
3. Repair access on the **Accounts** page: send enrollment links to the
   administrators who lost their passkeys, remove the lost passkeys, and
   revoke the sessions and tokens that may be compromised.
4. Delete the recovery account once you no longer need it.

The link works once, within 24 hours. Running `recover-admin` again replaces
it. It only creates a new account and never changes existing ones, so a
compromised administrator isn't quietly given back their access.

Issuing the link and using it each raise a `security` notification:
"An admin recovery link was issued over the Unix socket to uid 0", then, for
example, "rescue became an administrator by an admin recovery link". Both
are recorded in the daemon's history, and the request in the audit trail. An
unexpected one means someone with root on the host is acting: treat it as a
compromise of the host.

Before the first account exists, there's nothing to recover:
[create it](../getting-started/first-account.md) with `piquelctl setup-link`.

## A lost secret key

The daemon's secret key, `secrets.key` in its data directory, encrypts the
values of stored secrets and the addresses of notification destinations.
Accounts, passkeys, sessions and tokens don't depend on it, so losing it
doesn't stop anyone signing in.
[Secret storage](../reference/configuration.md#secret-storage) explains what
losing it means, and how to recover with `system:operate`.

## Troubleshooting

- **`sign-in-link` or `recover-admin` answers `endpoint_not_found` (404).**
  The command didn't reach the daemon's Unix socket as root or the daemon's
  user. Run it with `sudo` on the host, without `--url`. If the daemon runs in
  a container or a user namespace, the user has to match as the daemon sees
  it.
- **`sign-in-link` answers 404 even as root.** It refuses requests that carry
  a credential. Unset `PIQUELD_TOKEN`, or run `sudo piquelctl logout` if root
  has a saved login for the socket.
- **"recover-admin is only served over the daemon's Unix socket; use
  --socket".** `--url`, `PIQUELD_URL` or a connection profile points
  `piquelctl` at a network address. Recovery and sign-in links are never
  served over the network.
- **`recover-admin` answers `setup_pending`.** No account exists yet. Run
  `piquelctl setup-link` instead.
- **"the host operator is not an account; sign in with an account to do
  this".** Invitations, enrollment links and tokens need an account. Create
  one with `recover-admin` if nobody can sign in.
