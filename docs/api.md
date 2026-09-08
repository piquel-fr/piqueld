# HTTP API

The API is rooted at `/api/v1` and served over a Unix socket and optional
loopback TCP listener. JSON responses have a `data` envelope. Lists contain
`items` and an opaque `next_cursor`; errors contain a code, safe message,
optional details, and request ID. Clients poll for status.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/api/v1/system/status` | Daemon status |
| GET | `/api/v1/openapi.json` | API schema |
| GET | `/api/v1/applications` | Paginated applications |
| GET | `/api/v1/applications/{id}` | Desired application state |
| GET | `/api/v1/applications/{id}/detail` | Desired state, runtime summary, latest operation, and diagnostics |
| GET | `/api/v1/applications/{id}/status` | Reconciliation status |
| POST | `/api/v1/applications/plan` | Preview a manifest |
| POST | `/api/v1/applications/apply` | Apply a manifest by application name |
| DELETE | `/api/v1/applications/{id}` | Request deletion; no request body |
| GET | `/api/v1/operations/{id}` | Inspect an operation |

Apply and plan accept JSON `{ "manifest": ... }` using `ApplyApplicationRequest`,
or a complete TOML manifest with `Content-Type: application/toml` or `text/toml`.
A name identifies the application: applying a new name creates it, and applying
an existing name updates its desired state. There are no separate create,
replace, or explicit reconcile routes.

Every apply resolves image references again. Before accepting a changed target,
the server rejects known ownership and configuration conflicts from its runtime
plan. The server compares resolved state
with the current target before scheduling work. An equivalent request returns
the existing operation; if that operation failed or was cancelled, it is reset
to `requested` under the same ID. A different target cancels earlier active work
and becomes the latest operation. Clients send neither idempotency keys nor
generation checks. Repeating a mutable tag can deploy a new digest if the tag
has changed.

Mutations return HTTP 202 with `AcceptedOperation` containing `application_id`
and `operation_id`. Operations have kind `apply` or `delete` and state
`requested`, `running`, `succeeded`, `failed`, or `cancelled`. They expose
operation-level diagnostics; execution plans and individual action progress are
not stored as a step journal. Earlier operations remain inspectable subject to
retention settings.

Deletion retains named volumes. It remains `running`, recording errors and
retrying on later scans, until Docker observation confirms that services and
networks are absent. Only then does the application disappear from active reads
and the operation succeed.

Plan returns HTTP 200 with a `PlanView`. It does not commit application state or
schedule work. Inspect its blocking diagnostics before applying; Docker state
can change between preview and execution.

Shared application and operation states and HTTP request/response contracts live
in `piqueld-core`. The client adds transport and reexports those contracts. Detail responses expose bounded
runtime summaries rather than raw Docker labels, environment, or internal errors.
See [the CLI guide](piquelctl.md) and [generated schema](openapi-v1.json).

The unauthenticated TCP API accepts only loopback hosts. The optional dashboard
is served below `/dashboard/`; `/health` is an unversioned TCP liveness endpoint.
The Unix socket serves the API alone. Unknown API paths return JSON errors.
