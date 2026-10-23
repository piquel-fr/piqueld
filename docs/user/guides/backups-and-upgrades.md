# Backups and upgrades

The daemon migrates its database when a new version first starts. Migrations
are forward-only: an older daemon rejects a database with a newer schema, so
reverting the binary alone is not a rollback.

## Before upgrading

Create a SQLite backup in a private directory, for example:

```sh
sqlite3 /var/lib/piqueld/piqueld.db ".backup '/safe/location/piqueld-before-upgrade.db'"
sqlite3 /safe/location/piqueld-before-upgrade.db 'PRAGMA integrity_check; PRAGMA user_version;'
```

The SQLite backup command includes committed WAL data. Preserve the matching
daemon configuration and previous binary too.

## Rolling back

Stop the upgraded daemon, restore the backup as `piqueld.db` in a fresh data
directory, and configure the previous daemon to use that directory. Do not
reuse WAL/SHM files from the upgraded database. Restoring also reverts
application edits and deployment history accepted after the backup;
reconciliation observes the current Docker state against that restored intent.
