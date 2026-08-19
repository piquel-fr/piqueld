# Operator guide

Install the NixOS module or place the release binaries and UI bundle on a host
with Docker Engine. Configure the daemon through `/etc/piqueld/config.toml` (or
`PIQUELD_CONFIG`) and keep the state directory and credentials outside the Nix
store. The NixOS module supplies the service account, socket/runtime directory,
loopback registry, and systemd credential wiring.

The first useful checks are:

```console
piquelctl status
piquelctl list
piquelctl --help
```

Create or replace an application from a strict TOML manifest:

```console
piquelctl plan --file app.toml
piquelctl apply --file app.toml
piquelctl show my-app --json
piquelctl logs my-app --tail 200 --follow
```

Use an expected generation for concurrent operators. `apply`, `delete`, state
import, and secret deletion require explicit confirmation in non-interactive
automation. Secret values are read from protected stdin/files and are never
returned by list/show commands:

```console
piquelctl secret set registry-token --file ./registry-token
piquelctl secret list
```

Transfer desired control-plane state with the guarded commands documented in
[`state-archive-v1.md`](state-archive-v1.md):

```console
piquelctl export --output state.tar --mode portable
piquelctl import state.tar --replace --yes
```

A state archive is not a backup of Docker data. Recreate registry blobs, Git
credentials, volume contents, and secret values according to the dependency
report before expecting every application to converge. Routes are reachable only
through the explicitly configured private origin; public tunnel/DNS setup is an
operator-owned integration.
