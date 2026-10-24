# Backups and upgrades

Back up piqueld's state, so you can recover from a lost disk or roll back an
upgrade.

Run these steps on the piqueld host, as root: the data directory,
`/var/lib/piqueld` by default, is readable only by the daemon's user.

## What to back up

The data directory (`server.data_dir`) holds everything piqueld needs: the
database `piqueld.db`, the secret key `secrets.key`, and the state of its
tailnet nodes and ingress gateway. Back it up whole:

- Without `secrets.key`, the secrets, webhook secrets and notification
  destinations in the database can't be decrypted (see
  [secret storage](../reference/configuration.md#secret-storage)).
- Without the tailnet state, the daemon joins the tailnet as a new node, which
  may get another name, and passkeys registered for the
  [tailnet node's origin](../accounts/authentication.md#website-and-initial-setup)
  only work with the old name.

Your applications' data lives in Docker volumes, which aren't part of it. Back
them up separately.

## Back up

With the daemon stopped:

1. Stop it:

   ```console
   sudo systemctl stop piqueld
   ```

2. Copy the directory, keeping its owner and permissions:

   ```console
   sudo cp -a /var/lib/piqueld /safe/location/piqueld-backup
   ```

3. Start it again:

   ```console
   sudo systemctl start piqueld
   ```

While the daemon runs, the database must be copied with SQLite's backup
command, which includes changes still in its write-ahead log. You need
`sqlite3`, from the `sqlite` package.

1. Copy everything but the database:

   ```console
   sudo rsync -a --exclude 'piqueld.db*' /var/lib/piqueld/ /safe/location/piqueld-backup/
   ```

2. Copy the database:

   ```console
   sudo sqlite3 /var/lib/piqueld/piqueld.db ".backup '/safe/location/piqueld-backup/piqueld.db'"
   ```

3. Check the copy:

   ```console
   sudo sqlite3 /safe/location/piqueld-backup/piqueld.db 'PRAGMA integrity_check; PRAGMA user_version;'
   ```

## Upgrade

The daemon updates its database when a new version first starts. These updates
only go forward: an older daemon refuses a database a newer one has updated, so
going back to the previous binary alone isn't a rollback.

1. [Back up](#back-up) the data directory.
2. Keep the previous version and its configuration. On NixOS, the previous
   system generation keeps them.
3. Install the new version.

## Roll back

> **Warning:** restoring a backup also undoes every change made since it was
> taken: application edits, accounts and deployment history.

1. Stop the upgraded daemon:

   ```console
   sudo systemctl stop piqueld
   ```

2. Move its data directory aside, and restore the backup in its place. Don't
   copy the upgraded database's `-wal` and `-shm` files into it.

   ```console
   sudo mv /var/lib/piqueld /var/lib/piqueld.upgraded
   sudo cp -a /safe/location/piqueld-backup /var/lib/piqueld
   ```

3. Go back to the previous version: on NixOS, run
   `sudo nixos-rebuild switch --rollback`; elsewhere, reinstall the previous
   binary.
4. Start the daemon:

   ```console
   sudo systemctl start piqueld
   ```

piqueld then compares what Docker runs with the deployments in the restored
database, and repairs the difference, as it repairs any drift.
