# Applications

These guides cover what an application can do once you've
[deployed your first one](../getting-started/first-app.md): its services and
configuration, then building and shipping it.

- [Configuring services](services.md): run several services, with health
  checks, startup dependencies and resource limits.
- [Secrets and variables](secrets-and-variables.md): give services passwords
  and API keys, and set values in one place.
- [Volumes and files](volumes-and-files.md): keep data across deployments,
  and mount configuration files.
- [Running jobs before a rollout](jobs.md): migrate a database before the new
  version starts.
- [Connecting applications](links.md): let one application's services reach
  another's over a private network.
- [Running commands in a container](commands.md): run a console or an
  administration task in a running service.
- [Building images from Git](git-builds.md): build a service's image from a
  repository with a Dockerfile.
- [Deploying on push](deploy-on-push.md): keep the manifest in Git, and
  deploy every push.
- [Environments and promotion](environments.md): run staging and production,
  and promote what staging runs to production.
- [Restoring a deployment](restore.md): return an environment to a
  deployment it ran before.
- [Previews](previews.md): try a branch, or your uncommitted changes, in a
  short-lived copy of the application.
