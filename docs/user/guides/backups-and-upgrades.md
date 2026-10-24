# Backups and upgrades

The daemon keeps its state in its data directory (`server.data_dir`): the
SQLite database `piqueld.db`, the secret master key `secrets.key`, and the
state of its tailnet nodes and ingress gateway. Application data lives in
Docker volumes, which are not part of it.

## Backing up

Back up the whole data directory. Without `secrets.key`, the secrets, webhook
secrets and notification destinations in the database can't be decrypted (see
[secret storage](../reference/configuration.md#secret-storage)). Without the
tailnet state, the daemon joins the tailnet as a new node, which may get
another name, and passkeys registered for the
[tailnet node's origin](../getting-started/install.md#piquelds-own-tailnet-node)
only work with the old name.

Stop the daemon and copy the directory, keeping its owner and permissions. To
back up while the daemon runs, copy the database with SQLite's backup command,
which includes committed WAL data, then copy the rest of the directory
alongside it, without `piqueld.db` and its `-wal` and `-shm` files:

```sh
sqlite3 /var/lib/piqueld/piqueld.db ".backup '/safe/location/piqueld.db'"
sqlite3 /safe/location/piqueld.db 'PRAGMA integrity_check; PRAGMA user_version;'
```

## Upgrading

The daemon migrates its database when a new version first starts. Migrations
are forward-only: an older daemon rejects a database with a newer schema, so
reverting the binary alone is not a rollback. Back up the data directory before
upgrading, and keep the previous binary and configuration.

## Rolling back

Stop the upgraded daemon, restore the backup into a fresh data directory, and
configure the previous daemon to use it. Do not reuse the upgraded database's
WAL/SHM files. Restoring also reverts application edits and deployment history
accepted after the backup; reconciliation observes the current Docker state
against that restored intent.
