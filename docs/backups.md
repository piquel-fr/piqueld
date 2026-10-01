# Backup and restore

A consistent backup is the database, the secret master key, and the ingress
gateway's state, taken together. `piqueld backup` writes all of it into one
uncompressed tar archive and is safe to run while the daemon is up:

| Entry | Contents |
| --- | --- |
| `manifest.json` | Archive format, schema version, daemon version, instance ID, creation time |
| `piqueld.db` | `SQLite` online snapshot (`VACUUM INTO`), including committed WAL data |
| `secrets.key` | Secret master key, when one has been generated |
| `ingress/data`, `ingress/config` | Caddy certificates and configuration, when ingress has run |

Archives contain the secret master key, so they are as sensitive as the data
directory itself. They are written with mode `0600`; copy them off the host.

## Creating backups

Run commands as the daemon user with the daemon's configuration, so the archive
and the database are read with the right ownership:

```sh
sudo -u piqueld piqueld --config /etc/piqueld/config.toml backup --output /safe/piqueld.tar
sudo -u piqueld piqueld --config /etc/piqueld/config.toml backup --directory /var/backups/piqueld --keep 7
```

`--output` refuses to overwrite an existing file. `--directory` writes
`piqueld-<unix-ms>.tar`; with `--keep N` it then deletes all but the newest `N`
such archives. If `secrets.key` is replaced while the database is copied (only
lost-key recovery or the first secret does this), the backup fails and can be
retried. Gateway files are copied as they are; a certificate renewed during the
backup is re-issued if needed.

On NixOS, enable the systemd timer instead:

```nix
services.piqueld.backup = {
  enable = true;
  schedule = "daily";                  # systemd OnCalendar
  directory = "/var/backups/piqueld";  # created with mode 0700
  keep = 7;
};
```

## Status

Each successful `piqueld backup` records its completion time in the database.
`GET /api/v1/system/status` reports it as `backup.last_success_at_ms`, with
`backup.stale` set when no backup was recorded or the last one is older than
seven days. `piquelctl status` and the dashboard's system status show the same
information. Automatic pre-migration backups are not counted, since they stay
on the same disk as the data directory.

## Restoring

```sh
sudo systemctl stop piqueld
sudo mv /var/lib/piqueld /var/lib/piqueld.old
sudo install -d -o piqueld -g piqueld -m 0700 /var/lib/piqueld
sudo -u piqueld piqueld --config /etc/piqueld/config.toml restore /safe/piqueld.tar
sudo systemctl start piqueld
```

Restore only writes into an empty or absent data directory and holds the data
directory lock, so it refuses to run alongside the daemon. It rejects archives
whose schema is newer than the binary supports, unknown archive formats, and
entries outside the layout above. The
database must pass `PRAGMA integrity_check` and match its manifest before
anything is moved into place. The restored database keeps its archived schema;
the daemon migrates it on its next start.

Restore reverts application edits, accounts, and deployment history accepted
after the backup. Reconciliation then observes the current Docker state against
the restored intent.

## Upgrades and rollback

Migrations are forward-only, and an older daemon rejects a newer schema. Before
applying any migration, the daemon writes
`<data_dir>/backups/pre-<schema>-<unix-ms>.tar` and keeps the newest three. If
that backup fails, startup stops and no migration is applied.

To roll back an upgrade, stop the daemon, move the migrated data directory
aside, restore the pre-migration archive with the previous binary, and start
that binary:

```sh
sudo systemctl stop piqueld
sudo mv /var/lib/piqueld /var/lib/piqueld.upgraded
sudo install -d -o piqueld -g piqueld -m 0700 /var/lib/piqueld
sudo -u piqueld /path/to/previous/piqueld --config /etc/piqueld/config.toml \
  restore /var/lib/piqueld.upgraded/backups/pre-<schema>-<unix-ms>.tar
```

The previous binary must be one that ships `piqueld restore`. For upgrades from
older releases, use the manual SQLite backup described in
[migrations](migrations.md#upgrade-and-rollback).
