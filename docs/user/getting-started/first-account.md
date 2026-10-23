# Creating your account

You sign in to piqueld with a passkey instead of a password. Your phone,
laptop, password manager or security key keeps it, so there's no password to
choose, remember or leak. This page creates the first account, which
administers piqueld, and signs `piquelctl` in with it.

## Before you start

- piqueld is [installed](install.md), and you've chosen the
  [address of its dashboard](install.md#choose-how-to-reach-the-dashboard).
- Your browser can create passkeys: a device with a PIN or biometrics, a
  password manager with passkey support, or a security key.

## Create the first account

1. On the piqueld host, print the setup link:

   ```console
   piquelctl setup-link
   ```

   It prints a link such as
   `https://piqueld.<tailnet>.ts.net/dashboard/auth#invite=…`. Add `--open` to
   also open it in the host's browser. Only root and members of the `piqueld`
   group can get the link, because it's only served on the host's socket.

2. Open the link in your browser. Choose a username, and optionally a display
   name and a name for the passkey.

   ![The "Create your account" page, with the username alice, the display name Alice, and the passkey name Laptop](../../images/getting-started/create-account.png)

3. Select **Create account with a passkey**, and save the passkey when your
   browser asks.

The dashboard opens, signed in. Your account has `admin` on every application.
The link only works once: when the first account exists, setup is closed for
good. Invite other people from the dashboard's **Accounts** page; see
[accounts and invitations](../accounts/authentication.md#accounts-and-invitations).

To sign in later, open the dashboard and select **Sign in with a passkey**.
You don't need to type your username.

## Sign piquelctl in

On the piqueld host, members of the `piqueld` group sign in with:

```console
piquelctl login
```

From another machine, give `piquelctl` the dashboard's address:

```console
piquelctl --url https://piqueld.<tailnet>.ts.net login
```

`login` prints a link, a code, and the network address the daemon sees your
request coming from. Open the link, sign in, and enter the code. Before you
approve, check that the address the dashboard shows is the one your terminal
printed.

Then check who you're signed in as:

```console
piquelctl whoami
```

On the host, root doesn't need to sign in: `sudo piquelctl` acts as the
[host operator](../accounts/authentication.md#the-host-operator).

Next, [deploy your first application](first-app.md).

## Troubleshooting

- **`piquelctl setup-link` says setup is already completed.** An account
  exists already. If nobody can sign in with it any more, see
  [recovering administrator access](../accounts/authentication.md#recovering-administrator-access).
- **The browser won't create or use the passkey.** Passkeys need HTTPS, or
  `localhost`, and only work at the address they were created on. Open the
  dashboard at exactly the address piqueld uses: `piquelctl setup-link` prints
  it.
- **You can't run `piquelctl` on the host.** The daemon also writes the setup
  link to the file `setup-link` in its data directory, `/var/lib/piqueld` by
  default. Only root and the daemon's user can read it.
