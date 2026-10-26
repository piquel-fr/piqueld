# Publishing the documentation

This book is published at <https://docs.piqueld.dev> by a piqueld instance,
which runs it as the `docs` application. Every push to `main` rebuilds the book
and deploys it. This page sets that up once, on the instance that serves it.

Two files in
[`deploy/docs`](https://github.com/piquel-fr/piqueld/tree/main/deploy/docs)
define the site:

- `Dockerfile` builds the book with the mdBook that `flake.lock` pins, through
  `nix develop .#docs`, the shell `just docs` and CI use. It serves the pages
  with [static-web-server](https://static-web-server.net/) on port 80.
- `piqueld.toml` is the application's manifest. piqueld reads it from `main`
  on each deployment, builds the `Dockerfile` from the same commit, and serves
  it on `docs.piqueld.dev`.

## Before you start

- The instance serves public routes through
  [HTTP ingress](../user/guides/ingress.md#enable-and-expose-an-application),
  and has a [webhook hostname](../user/guides/ingress.md#push-webhooks) whose
  DNS is public, so GitHub can report pushes.
- The host is `x86_64-linux`, the only Linux system the flake's `docs` shell
  supports, and can reach GitHub and `cache.nixos.org` while it builds.
- You can create applications on the instance, and change and deploy them:
  `apps:create`, `apps:write` and `apps:deploy`; see
  [permissions](../user/accounts/authorization.md#grants).
- You can change the DNS of `piqueld.dev` and the settings of the
  `piquel-fr/piqueld` repository on GitHub.

Run the `piquelctl` commands on this page from a checkout of `main`, on the
host or on any machine where `piquelctl` is signed in to the instance.

## Set it up

1. Point `docs.piqueld.dev` at the instance, like any public route: an `A`
   record to the host's public address, a CNAME to the tunnel with a
   [Cloudflare Tunnel](../user/guides/ingress.md#cloudflare-tunnel), or
   nothing when piqueld
   [manages the zone's records](../user/guides/ingress.md#managed-dns-records).

2. Check what saving the manifest would do:

   ```console
   piquelctl app plan --file deploy/docs/piqueld.toml
   ```

   The plan creates the `docs` application, with its `web` service and its
   `docs.piqueld.dev` route.

3. Save the manifest and deploy it:

   ```console
   piquelctl --timeout 15m app apply --file deploy/docs/piqueld.toml --deploy
   ```

   The first build downloads the Nix image and the documentation toolchain,
   and can take several minutes. Later builds reuse them, and only rebuild the
   book.

4. Generate the secret of the repository's webhook:

   ```console
   piquelctl app repository webhook rotate docs
   ```

   It prints the webhook's payload URL and secret. piqueld never shows the
   secret again.

5. In GitHub, open the repository's **Settings**, then **Webhooks**, and
   select **Add webhook**. Set **Payload URL** and **Secret** to the values
   from the previous step, **Content type** to `application/json`, and the
   events to **Just the push event**. Then select **Add webhook**.

From now on, change the site through pull requests to `main`, including
`deploy/docs/piqueld.toml`: each push to `main` deploys the manifest and the
book of that commit. Applying the file again is safe, since it is the same
manifest the repository holds.
[Repository-backed manifests](../user/reference/manifest.md#repository-backed-manifests)
and [deploying on push](../user/reference/manifest.md#deploying-on-push)
describe how this works.

## Check that it works

The site serves the book:

```console
curl --fail --head https://docs.piqueld.dev/dev/publishing.html
```

After the next push to `main`, check that it deployed:

```console
piquelctl env show docs production
```

Its `Sync` line says `following`, with the commit of that push. In GitHub,
the webhook's **Recent Deliveries** shows each push, with piqueld's response
`204`.

## Build the image locally

From the root of the repository:

```console
docker build --file deploy/docs/Dockerfile --tag piqueld-docs .
docker run --rm --publish 8080:80 piqueld-docs
```

The book is then on <http://localhost:8080>. To build it in a
[development instance](development.md#docker)'s engine rather than the host's,
set `DOCKER_HOST` to that engine's socket first.

## Troubleshooting

- **The deployment fails with `git_build_failed`.** The build's output ends
  with the error. List the builds with
  `piquelctl builds list --application <id>`, where `<id>` is the ID
  `piquelctl app show docs` prints, then read one with
  `piquelctl builds logs <number>`.
- **The first deployment fails with `preparation_timeout`.** The build took
  longer than
  [`reconciliation.prepare_timeout_seconds`](../user/reference/configuration-settings.md#reconciliationprepare_timeout_seconds),
  300 seconds by default. Deploy again with
  `piquelctl --timeout 15m app deploy docs`: the steps that finished stay
  cached. If it keeps failing, raise the setting.
- **A push deploys nothing.** `piquelctl env show docs production` says why on
  its `Sync` line, and GitHub's **Recent Deliveries** shows whether the
  delivery failed. A `401` means the secret doesn't match: generate a new one
  and copy it into GitHub again.
