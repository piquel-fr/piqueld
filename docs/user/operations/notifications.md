# Notifications

Get a message in Discord, or a JSON webhook of your own, when a deployment
fails, a service degrades, or someone gains administrator access.

## Before you start

- You need the `notifications:manage` permission to add destinations, and
  `system:read` to see them. Administrators have both; see
  [permissions](../accounts/authorization.md).
- A webhook URL: a Discord channel's webhook, or an endpoint of your own that
  accepts JSON. It must use HTTPS, except on `localhost`.

## Add a destination

On the dashboard's **Notifications** page, select **Add destination**. Or, on
the host or any machine `piquelctl` is signed in on:

1. Save the webhook URL in a file. The URL is a credential: anyone who has it
   can post to your channel.

   ```console
   echo 'https://discord.com/api/webhooks/…' > discord-url
   ```

2. Create the destination:

   ```console
   piquelctl notification set ops --kind discord --url-file discord-url
   ```

   `ops` is its name. A new destination receives every category but
   successful deployments, about every application, without previews.

3. Send a test notification:

   ```console
   $ piquelctl notification test ops
   ops acknowledged the test notification.
   ```

The URL is stored encrypted and can't be read back, from the dashboard or the
CLI.

In Discord, each notification shows as a card coloured by category, such as
red for failures and green for recoveries, with the application, environment
and branch, and the cause and next step of a failure. Its title links to the
relevant page of the dashboard.

## Choose what it receives

Each destination picks its categories:

| Category | Dashboard label | Sent when |
| --- | --- | --- |
| `build_failures` | Build failures | A build fails |
| `deployment_failures` | Deployment failures | A deployment fails |
| `service_degradation` | Service degradation | A deployed service stays unhealthy for two minutes |
| `daemon_failures` | Daemon failures | Docker, the Swarm or the ingress gateway stays unavailable for two minutes, or the daemon hits an internal error |
| `recovery` | Recoveries | A failure the destination was sent is resolved |
| `security` | Security events | Someone gains administrator access, a privileged token is created, the host operator issues a sign-in or recovery link, requests from one address keep being refused, a credential is used from a new address, the secret key is recovered, or a notification destination changes |
| `daemon_started` | Daemon starts | The daemon starts, including after an update |
| `deployment_succeeded` | Deployments | A deployment succeeds |

For example, to send only deployment failures and recoveries about `shop`, and
to mention a Discord role with each failure:

```console
piquelctl notification set ops \
  --categories deployment_failures,recovery \
  --apps shop \
  --message 'deployment_failures=<@&123456789012345678>'
```

- `--categories` replaces the list. `--message CATEGORY=TEXT` adds a text to a
  category's notifications, which also subscribes the destination to it. In
  Discord, that text is the message, so mentions in it notify people.
- `--apps` limits it to some applications, and `--all-apps` sends it every
  application, including later ones. Daemon notifications, such as security
  events, reach every destination, whatever its applications.
- `--previews true` adds notifications about [previews](../reference/manifest.md#previews).
- `--enabled false` pauses a destination.

Options you don't give keep their current value. `piquelctl notification list`
shows what each destination receives.

Changes apply at once and never replay the past: a category you add starts with
the next event.

## Read the delivery history

The **Deliveries** list on the **Notifications** page, or
`piquelctl notification deliveries`, shows every notification sent, newest
first:

![The Notifications page: the destination local, receiving 7 categories about every application, and three delivered notifications: a security event, a daemon start, and a deployment failure of broken / production](../../images/operations/notifications.png)

A delivery that fails is retried with increasing delays, for up to 24 hours.
After that, retry it by hand with `piquelctl notification retry <delivery-id>`,
which needs the `system:operate` permission.

## When piqueld notifies

- A failure notifies once, until it's resolved: retrying a failing
  deployment doesn't notify again.
- Service and dependency problems notify only once they've lasted two minutes,
  so a quick restart doesn't page you.
- A recovery goes only to the destinations that were sent the failure.
- When a deployment fixes a failure, the destination receives the recovery
  instead of a `deployment_succeeded` notification.
- `daemon_started` is sent once the daemon is serving and Docker Swarm is
  ready, so a start that fails earlier sends nothing. After an update, its
  message names the previous version, such as
  `piqueld 0.2.0 started (previously 0.1.0)`.

## Receive notifications in your own service

A `json` destination receives a POST request per notification. This is a
deployment failure, shortened:

```json
{
  "version": 2,
  "instance_id": "instance-01a16b10b8307c61b5ac3d27d4bf7937",
  "delivery_id": "delivery-01a16b3c72347691b00a9c4fac1e1f9b",
  "category": "deployment_failures",
  "title": "Deployment failed",
  "subject": {
    "application": "broken",
    "environment": "production",
    "branch": null,
    "preview": false
  },
  "message": null,
  "event": {
    "kind": "operation_failed",
    "message": "container image was rejected by the registry while pull image",
    "error_code": "image_resolution_rejected",
    "diagnostic": {
      "summary": "container image was rejected by the registry while pull image",
      "next_action": "Check the image reference and registry credentials, then retry the deployment.",
      "causes": ["Docker operation: pull image", "Docker returned HTTP status 404"]
    }
  }
}
```

- `subject` names the application, environment or preview, and branch, as they
  were when the notification was sent. They're `null` for daemon
  notifications.
- `message` is the destination's text for the category, or `null`.
- `event` is the history event behind the notification.
- A test notification has a `null` category.

A notification may arrive more than once. Every request carries an
`Idempotency-Key` header with the delivery ID: ignore a key you've already
handled. Answer with any 2xx status to acknowledge it. piqueld retries
connection errors, timeouts, 408, 429 and 5xx responses, honouring an integer
`Retry-After` up to one hour, and gives up on any other response. Requests time
out after 10 seconds, and redirects aren't followed. Payloads aren't signed, so
keep the URL secret.

## Troubleshooting

- **The test fails.** `piquelctl notification test` says why. Check that the
  daemon's host can reach the URL, and that it accepts a POST request.
- **A notification never arrived.** Look it up in the delivery history. If it
  isn't there, check the destination's categories, applications and previews
  with `piquelctl notification list`.
- **Destinations disappeared.** Recovering a lost
  [secret key](../reference/configuration.md#secret-storage) deletes them,
  since their URLs were encrypted with it. Add them again.
