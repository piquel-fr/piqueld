# Introduction

piqueld runs your applications on one Linux server with Docker. You describe
each application in a manifest: its services, how to get their images, and how
to reach them. piqueld builds and deploys it, serves it over HTTPS, and keeps
it running.

You manage it from:

- the **dashboard**, in your browser;
- **`piquelctl`**, its command-line tool, which also suits scripts and AI
  agents;
- its **HTTP API**, which both of them use.

## Concepts

- **Application**: a set of services that work together, such as a website
  and its database.
- **Manifest**: the TOML file that describes an application. You can apply a
  manifest file, or edit its settings in the dashboard or with `piquelctl`:
  either way, you change the application's saved manifest.
- **Service**: one container image, run as one or more replicas. Its image is
  pulled from a registry, or built from a Git repository.
- **Environment**: a running copy of an application, with its own
  deployments, secrets, volumes and routes. Each application starts with a
  `production` environment, and you can add others, such as `staging`.
- **Deployment**: one run of the saved manifest in an environment. Saving a
  change never touches what's running; deploying does. piqueld keeps the
  history of every deployment.
- **Release**: what a deployment ran, down to the exact images. You can
  promote a release to another environment, which then runs the same images
  without building them again.
- **Preview**: a short-lived environment for a Git branch or your local
  changes, which you delete when you're done.
- **Route**: an HTTPS address that sends requests to a service, either public
  or only reachable on your tailnet, your private Tailscale network.
- **Link**: a private connection that lets one application's service reach a
  service of another application. Both applications have to agree to it.

## Limits

- piqueld manages a single host. It doesn't run clusters of several machines.
- Services are reached over HTTP and HTTPS only, not other TCP or UDP ports.
- It doesn't manage registry credentials.

## Get started

1. [Install piqueld](getting-started/install.md).
2. [Create your account](getting-started/first-account.md).
3. [Deploy your first application](getting-started/first-app.md).
