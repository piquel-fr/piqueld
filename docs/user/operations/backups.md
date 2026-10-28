# Backups and upgrades

Back up piqueld's state, so you can recover from a lost disk or roll back an
upgrade.

Run these steps on the piqueld host, as root: the data directory is readable
only by the daemon's user. The commands use `/var/lib/piqueld`, its default
location. If you set
[`server.data_dir`](../reference/configuration-settings.md#serverdata_dir),
use its value instead. Some steps need `sqlite3`, from the `sqlite` package.

## What to back up

The data directory,
[`server.data_dir`](../reference/configuration-settings.md#serverdata_dir),
holds everything piqueld needs: the database `piqueld.db`, the secret key
`secrets.key`, and the state of its tailnet nodes and ingress gateway. Back it
up whole:

- Without `secrets.key`, the secrets, webhook secrets and notification
  destinations in the database can't be decrypted (see
  [secret storage](../reference/configuration.md#secret-storage)).
- Without the tailnet state, the daemon joins the tailnet as a new node, which
  may get another name, and passkeys registered for the
  [tailnet node's origin](../getting-started/install.md#piquelds-own-tailnet-node)
  only work with the old name.

Your applications' data lives in Docker volumes, which aren't part of it. Back
them up separately.

## Back up

With the daemon stopped:

1. Stop it:

   ```console
   sudo systemctl stop piqueld
   ```

2. If [ingress](../guides/ingress.md) is enabled, stop the containers piqueld
   runs for it, which keep writing to the data directory. Your applications'
   routes stay unreachable until the daemon starts again.

   ```console
   instance=$(sudo sqlite3 /var/lib/piqueld/piqueld.db 'SELECT instance_id FROM instance_metadata')
   sudo docker stop $(sudo docker ps --quiet --filter label=io.piqueld.ingress=true --filter label=io.piqueld.instance=$instance)
   ```

3. Create the backup directory, then copy the data directory's contents into
   it, keeping their owner and permissions. If the backup directory already
   exists, `mkdir` fails and nothing is copied, rather than mixing two
   backups:

   ```console
   sudo mkdir --mode=0700 /safe/location/piqueld-backup &&
     sudo cp -a /var/lib/piqueld/. /safe/location/piqueld-backup/
   ```

4. Start it again:

   ```console
   sudo systemctl start piqueld
   ```

While the daemon runs, the database must be copied with SQLite's backup
command, which includes changes still in its write-ahead log. Copy the
database before the other files: the daemon may create `secrets.key` while you
copy, and the backup then includes it.

> **Warning:** don't run `piquelctl secrets recover-key` or replace
> `secrets.key` until the backup is done. The backup could otherwise hold a key
> that can't decrypt its database. If you can't rule that out, stop the daemon
> and back up as above.

1. Create the backup directory:

   ```console
   sudo mkdir --mode=0700 /safe/location/piqueld-backup
   ```

2. Copy the database:

   ```console
   sudo sqlite3 /var/lib/piqueld/piqueld.db ".backup '/safe/location/piqueld-backup/piqueld.db'"
   ```

3. Give the copy the database's owner and permissions. Without them, the
   daemon can't write to a restored copy.

   ```console
   sudo chown piqueld:piqueld /safe/location/piqueld-backup/piqueld.db
   sudo chmod 0600 /safe/location/piqueld-backup/piqueld.db
   ```

4. Copy the other files, keeping their owner and permissions:

   ```console
   sudo rsync -a --exclude 'piqueld.db*' /var/lib/piqueld/ /safe/location/piqueld-backup/
   ```

5. Check the copy:

   ```console
   sudo sqlite3 /safe/location/piqueld-backup/piqueld.db 'PRAGMA integrity_check; PRAGMA user_version;'
   ```

   It prints `ok`, then the database's version.

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

2. If [ingress](../guides/ingress.md) is enabled, stop the containers piqueld
   runs for it, which keep using the data directory you're about to move.
   Your applications' routes stay unreachable until the daemon starts again.
   The filter on this installation's ID leaves any other piqueld on the same
   Docker alone:

   ```console
   instance=$(sudo sqlite3 /var/lib/piqueld/piqueld.db 'SELECT instance_id FROM instance_metadata')
   sudo docker stop $(sudo docker ps --quiet --filter label=io.piqueld.ingress=true --filter label=io.piqueld.instance=$instance)
   ```

3. Move the data directory aside:

   ```console
   sudo mv /var/lib/piqueld /var/lib/piqueld.upgraded
   ```

4. Restore the backup in its place. Don't copy the upgraded database's `-wal`
   and `-shm` files into it.

   ```console
   sudo cp -a /safe/location/piqueld-backup /var/lib/piqueld
   ```

5. Give the daemon's user every restored file. On a new host, its user ID may
   differ from the backup's.

   ```console
   sudo chown -R piqueld:piqueld /var/lib/piqueld
   ```

6. Go back to the previous version: on NixOS, run
   `sudo nixos-rebuild switch --rollback`; elsewhere, reinstall the previous
   binary.
7. Start the daemon:

   ```console
   sudo systemctl start piqueld
   ```

   It starts the ingress containers again, on the restored directory.

8. If an application [receives telemetry](telemetry.md#receive-telemetry-in-an-application),
   restart its collector, which still listens in the directory you moved
   aside:

   ```console
   sudo docker restart $(sudo docker ps --quiet --filter volume=/run/piqueld/telemetry)
   ```

   It may fail to start a few times, until piqueld removes the stale socket
   the backup holds.

piqueld then compares what Docker runs with the deployments in the restored
database, and repairs the difference, as it repairs any drift.
