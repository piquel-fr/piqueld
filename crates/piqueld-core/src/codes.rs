//! Stable machine-readable codes for validation errors, compilation errors,
//! and planner diagnostics.

/// Manifest does not decode against the strict public schema.
pub const MANIFEST_DECODE_FAILED: &str = "manifest_decode_failed";
/// Manifest `api_version` is not supported.
pub const API_VERSION_UNSUPPORTED: &str = "api_version_unsupported";
/// Manifest `kind` is not supported.
pub const KIND_UNSUPPORTED: &str = "kind_unsupported";
/// Logical name violates the bounded naming rule.
pub const NAME_INVALID: &str = "name_invalid";
/// Application declares no service.
pub const SERVICE_REQUIRED: &str = "service_required";
/// Service logical name is duplicated.
pub const SERVICE_NAME_DUPLICATE: &str = "service_name_duplicate";
/// Volume logical name is duplicated.
pub const VOLUME_NAME_DUPLICATE: &str = "volume_name_duplicate";
/// Service dependency does not name a service in the application.
pub const SERVICE_DEPENDENCY_MISSING: &str = "service_dependency_missing";
/// Service dependency is listed more than once.
pub const SERVICE_DEPENDENCY_DUPLICATE: &str = "service_dependency_duplicate";
/// Service lists more dependencies than an application can have services.
pub const SERVICE_DEPENDENCY_COUNT_EXCESSIVE: &str = "service_dependency_count_excessive";
/// Service dependencies form or lead into a cycle.
pub const SERVICE_DEPENDENCY_CYCLE: &str = "service_dependency_cycle";
/// Rollout monitor window is outside its supported range.
pub const ROLLOUT_MONITOR_INVALID: &str = "rollout_monitor_invalid";
/// An explicit start-first order lets two tasks share a writable volume.
pub const ROLLOUT_START_FIRST_WRITABLE_VOLUME: &str = "rollout_start_first_writable_volume";
/// A routed service declares no health check in an environment that rolls
/// back failed rollouts automatically, so unless its image defines one, a
/// rollout that doesn't serve isn't detected as failed.
pub const ROLLBACK_WITHOUT_HEALTH_CHECK: &str = "rollback_without_health_check";
/// Replica count is outside its supported range.
pub const REPLICAS_OUT_OF_RANGE: &str = "replicas_out_of_range";
/// Image reference is not a safe registry reference.
pub const IMAGE_INVALID: &str = "image_invalid";
/// Git repository URL embeds credentials.
pub const GIT_CREDENTIALS_INLINE: &str = "git_credentials_inline";
/// Git repository URL is empty, oversized, or option-like.
pub const GIT_REPOSITORY_INVALID: &str = "git_repository_invalid";
/// Git branch is not a valid branch name.
pub const GIT_BRANCH_INVALID: &str = "git_branch_invalid";
/// Git commit is not a full lowercase hexadecimal hash.
pub const GIT_COMMIT_INVALID: &str = "git_commit_invalid";
/// Repository path is not relative or leaves the repository.
pub const REPOSITORY_PATH_INVALID: &str = "repository_path_invalid";
/// Repository poll interval is outside its supported range.
pub const SYNC_INTERVAL_INVALID: &str = "sync_interval_invalid";
/// Environment name violates its alphabet rule.
pub const ENVIRONMENT_NAME_INVALID: &str = "environment_name_invalid";
/// Environment value contains forbidden bytes.
pub const ENVIRONMENT_VALUE_INVALID: &str = "environment_value_invalid";
/// Environment map exceeds its entry budget.
pub const ENVIRONMENT_COUNT_EXCESSIVE: &str = "environment_count_excessive";
/// Environment value exceeds its byte budget.
pub const ENVIRONMENT_VALUE_EXCESSIVE: &str = "environment_value_excessive";
/// Docker build argument name violates its alphabet rule.
pub const BUILD_ARG_NAME_INVALID: &str = "build_arg_name_invalid";
/// Docker build argument value contains forbidden bytes.
pub const BUILD_ARG_VALUE_INVALID: &str = "build_arg_value_invalid";
/// Docker build argument map exceeds its entry budget.
pub const BUILD_ARG_COUNT_EXCESSIVE: &str = "build_arg_count_excessive";
/// Docker build argument value exceeds its byte budget.
pub const BUILD_ARG_VALUE_EXCESSIVE: &str = "build_arg_value_excessive";
/// Docker build arguments together exceed their byte budget.
pub const BUILD_ARG_TOTAL_EXCESSIVE: &str = "build_arg_total_excessive";
/// Docker build target is not a valid stage name.
pub const BUILD_TARGET_INVALID: &str = "build_target_invalid";
/// Mount references an undeclared volume.
pub const MOUNT_VOLUME_MISSING: &str = "mount_volume_missing";
/// Mount target is not a normalized absolute container path.
pub const MOUNT_TARGET_UNSAFE: &str = "mount_target_unsafe";
/// Mount target is duplicated within one service.
pub const MOUNT_TARGET_DUPLICATE: &str = "mount_target_duplicate";
/// Service declares more mounts than allowed.
pub const MOUNT_COUNT_EXCESSIVE: &str = "mount_count_excessive";
/// Resource limits configure neither CPU nor memory.
pub const RESOURCE_LIMITS_EMPTY: &str = "resource_limits_empty";
/// CPU limit is zero.
pub const CPU_LIMIT_INVALID: &str = "cpu_limit_invalid";
/// CPU limit exceeds its explicit budget.
pub const CPU_LIMIT_EXCESSIVE: &str = "cpu_limit_excessive";
/// Memory limit is zero or does not fit the runtime value.
pub const MEMORY_LIMIT_INVALID: &str = "memory_limit_invalid";
/// Health-check port is outside 1-65535.
pub const PORT_INVALID: &str = "port_invalid";
/// HTTP health-check path is unsafe.
pub const HEALTHCHECK_PATH_INVALID: &str = "healthcheck_path_invalid";
/// Health-check command has no usable executable or contains NUL.
pub const HEALTHCHECK_COMMAND_INVALID: &str = "healthcheck_command_invalid";
/// Health-check interval is not positive.
pub const HEALTHCHECK_INTERVAL_INVALID: &str = "healthcheck_interval_invalid";
/// Health-check interval exceeds its explicit budget.
pub const HEALTHCHECK_INTERVAL_EXCESSIVE: &str = "healthcheck_interval_excessive";
/// Health-check timeout is not positive or exceeds its interval.
pub const HEALTHCHECK_TIMEOUT_INVALID: &str = "healthcheck_timeout_invalid";
/// Health-check start period exceeds its explicit budget.
pub const HEALTHCHECK_START_PERIOD_EXCESSIVE: &str = "healthcheck_start_period_excessive";
/// Health-check retries are not positive or exceed their explicit budget.
pub const HEALTHCHECK_RETRIES_INVALID: &str = "healthcheck_retries_invalid";
/// Explicit container command does not start with a non-empty executable.
pub const PROCESS_COMMAND_INVALID: &str = "process_command_invalid";
/// Container command exceeds its element or byte budget.
pub const PROCESS_COMMAND_EXCESSIVE: &str = "process_command_excessive";
/// Container arguments exceed their element or byte budget.
pub const PROCESS_ARGUMENTS_EXCESSIVE: &str = "process_arguments_excessive";
/// Container process argument contains NUL.
pub const PROCESS_ARGUMENT_INVALID: &str = "process_argument_invalid";
/// Application declares more services than allowed.
pub const SERVICE_COUNT_EXCESSIVE: &str = "service_count_excessive";
/// Application declares more volumes than allowed.
pub const VOLUME_COUNT_EXCESSIVE: &str = "volume_count_excessive";
/// Application declares more jobs than allowed.
pub const JOB_COUNT_EXCESSIVE: &str = "job_count_excessive";
/// The daemon could not observe an environment's runtime.
pub const RUNTIME_UNAVAILABLE: &str = "runtime_unavailable";
/// Application declares more routes than allowed.
pub const ROUTES_LIMIT: &str = "routes_limit";
/// Route name is used by another route of the application.
pub const ROUTE_NAME_DUPLICATE: &str = "route_name_duplicate";
/// Route hostname is not an exact public hostname or a wildcard of one.
pub const ROUTE_HOSTNAME_INVALID: &str = "route_hostname_invalid";
/// Route hostname is used by another route of the application.
pub const ROUTE_HOSTNAME_DUPLICATE: &str = "route_hostname_duplicate";
/// Route sets neither, or both, a service and port and a redirect.
pub const ROUTE_TARGET_INVALID: &str = "route_target_invalid";
/// Route references an undeclared service.
pub const ROUTE_SERVICE_MISSING: &str = "route_service_missing";
/// Route backend port is zero.
pub const ROUTE_PORT_INVALID: &str = "route_port_invalid";
/// Redirect destination is not an absolute public `http(s)` URL.
pub const ROUTE_REDIRECT_INVALID: &str = "route_redirect_invalid";
/// Redirect status is not a supported redirect code.
pub const ROUTE_REDIRECT_STATUS_INVALID: &str = "route_redirect_status_invalid";
/// Redirect destination is a hostname the route serves itself.
pub const ROUTE_REDIRECT_LOOP: &str = "route_redirect_loop";
/// Application declares more generated secrets, or a service more secret
/// mounts, than allowed.
pub const SECRETS_EXCESSIVE: &str = "secrets_excessive";
/// Secret target is not a unique file path under `/run/secrets`.
pub const SECRET_TARGET_INVALID: &str = "secret_target_invalid";
/// Generated secret name is duplicated.
pub const SECRET_NAME_DUPLICATE: &str = "secret_name_duplicate";
/// Generated secret uses an unsupported byte count or key size.
pub const SECRET_GENERATOR_INVALID: &str = "secret_generator_invalid";
/// Job logical name is duplicated.
pub const JOB_NAME_DUPLICATE: &str = "job_name_duplicate";
/// Job references an undeclared service.
pub const JOB_SERVICE_MISSING: &str = "job_service_missing";
/// A job would start a dependency before that dependency's own jobs succeeded.
pub const JOB_DEPENDENCY_ORDER_INVALID: &str = "job_dependency_order_invalid";
/// Job timeout is outside its supported range.
pub const JOB_TIMEOUT_INVALID: &str = "job_timeout_invalid";
/// An exclusive job has no writable volume, or a startup dependency mounts one.
pub const JOB_VOLUME_ACCESS_INVALID: &str = "job_volume_access_invalid";
/// A job sets an environment variable or mounts at a target its service already does.
pub const JOB_SETTING_CONFLICT: &str = "job_setting_conflict";
/// A job stops its own service, or an exclusive job those sharing its volumes,
/// until rollout.
pub const JOB_STOPS_SERVICES: &str = "job_stops_services";

/// Template text has a `${{` without a well-formed `namespace.name` reference.
pub const TEMPLATE_INVALID: &str = "template_invalid";
/// Reference names a namespace that is reserved for a later feature.
pub const VARIABLE_RESERVED: &str = "variable_reserved";
/// Reference names an unknown namespace.
pub const VARIABLE_NAMESPACE_UNKNOWN: &str = "variable_namespace_unknown";
/// Reference names an unknown system variable.
pub const VARIABLE_UNKNOWN: &str = "variable_unknown";
/// Reference names a variable declared nowhere in the manifest.
pub const VARIABLE_UNDECLARED: &str = "variable_undeclared";
/// Reference appears where values cannot reference variables.
pub const VARIABLE_NOT_ALLOWED: &str = "variable_not_allowed";
/// Reference names a system variable this deployment does not have.
pub const VARIABLE_UNAVAILABLE: &str = "variable_unavailable";
/// Declared variable has no value for the environment being rendered.
pub const VARIABLE_VALUE_MISSING: &str = "variable_value_missing";
/// Rendered value does not have the field's type.
pub const VARIABLE_TYPE_INVALID: &str = "variable_type_invalid";
/// Variable name violates its alphabet rule.
pub const VARIABLE_NAME_INVALID: &str = "variable_name_invalid";
/// Manifest declares more variables or environment blocks than allowed.
pub const VARIABLE_COUNT_EXCESSIVE: &str = "variable_count_excessive";
/// Variable value exceeds its byte budget.
pub const VARIABLE_VALUE_EXCESSIVE: &str = "variable_value_excessive";
/// Previews need an application whose manifest comes from a repository.
pub const PREVIEW_REQUIRES_REPOSITORY: &str = "preview_requires_repository";
/// Creating a preview would exceed a `[previews]` count limit.
pub const PREVIEW_LIMIT_REACHED: &str = "preview_limit_reached";
/// A preview service runs with a `[previews]` default limit it does not set.
pub const PREVIEW_LIMITS_DEFAULTED: &str = "preview_limits_defaulted";
/// A preview service asks for more replicas than `[previews] max_replicas`.
pub const PREVIEW_REPLICAS_CAPPED: &str = "preview_replicas_capped";
/// A local snapshot bundle is larger than `[snapshots] max_upload_bytes`.
pub const SNAPSHOT_TOO_LARGE: &str = "snapshot_too_large";
/// A local snapshot upload is not a valid Git bundle of one snapshot commit.
pub const SNAPSHOT_INVALID: &str = "snapshot_invalid";
/// The base commit a thin snapshot bundle needs cannot be fetched from the
/// application's repository; upload a complete bundle instead.
pub const SNAPSHOT_BASE_UNAVAILABLE: &str = "snapshot_base_unavailable";
/// A local snapshot upload stalled, or took too long to receive.
pub const SNAPSHOT_TIMED_OUT: &str = "snapshot_timed_out";
/// Only environments of a repository-backed application follow a branch.
pub const MANIFEST_REPOSITORY_REQUIRED: &str = "manifest_repository_required";
/// A promoted environment also sets a branch, commit, or sync.
pub const ENVIRONMENT_SOURCE_CONFLICT: &str = "environment_source_conflict";
/// An environment pinned to a commit cannot sync.
pub const ENVIRONMENT_SYNC_PINNED: &str = "environment_sync_pinned";
/// A promotion source is not an environment that can be one.
pub const PROMOTION_SOURCE_INVALID: &str = "promotion_source_invalid";
/// An environment would, directly or through others, promote from itself.
pub const PROMOTION_CYCLE: &str = "promotion_cycle";
/// The manifest declares the environment, so it owns its source and existence.
pub const ENVIRONMENT_DECLARED: &str = "environment_declared";
/// An edit would declare an environment that doesn't exist.
pub const ENVIRONMENT_UNKNOWN: &str = "environment_unknown";
/// Manifest that must be literal still references variables.
pub const VARIABLE_UNRESOLVED: &str = "variable_unresolved";
/// A declared secret is also set in the application's secret store.
pub const SECRET_NAME_CONFLICT: &str = "secret_name_conflict";
/// A file's target is a mount or secret target, another file's, or reserved.
pub const FILE_TARGET_INVALID: &str = "file_target_invalid";
/// A file reads the manifest's repository, but the application has none.
pub const FILE_REQUIRES_REPOSITORY: &str = "file_requires_repository";
/// Inline file content is empty, which Swarm configs cannot be.
pub const FILE_EMPTY: &str = "file_empty";
/// Inline file content exceeds the per-file budget.
pub const FILE_TOO_LARGE: &str = "file_too_large";
/// A service mounts more files than allowed.
pub const FILE_COUNT_EXCESSIVE: &str = "file_count_excessive";
/// An application's files together exceed their byte budget.
pub const FILE_TOTAL_EXCESSIVE: &str = "file_total_excessive";

/// Service source has no immutable resolution yet.
pub const SOURCE_UNRESOLVED: &str = "source_unresolved";
/// A mounted secret has no pinned Docker secret version yet.
pub const SECRET_UNRESOLVED: &str = "secret_unresolved";
/// Resolved source does not immutably match the requested service source.
pub const SOURCE_RESOLUTION_MISMATCH: &str = "source_resolution_mismatch";
/// A repository file a service mounts was not captured with its manifest.
pub const FILE_UNRESOLVED: &str = "file_unresolved";
/// An application declares too many exports or imports, or an export
/// grants too many applications.
pub const LINK_COUNT_EXCESSIVE: &str = "link_count_excessive";
/// An export references a service the application does not declare.
pub const EXPORT_SERVICE_MISSING: &str = "export_service_missing";
/// A service is exported, or an application granted, more than once.
pub const EXPORT_DUPLICATE: &str = "export_duplicate";
/// An export grants no application.
pub const EXPORT_APPLICATIONS_EMPTY: &str = "export_applications_empty";
/// A service of another application is imported more than once.
pub const IMPORT_DUPLICATE: &str = "import_duplicate";
/// An import does not connect: its environment is missing, does not export
/// the service to the importer, or already links to the most importers.
pub const IMPORT_UNCONNECTED: &str = "import_unconnected";
/// More than one service of an application receives telemetry.
pub const TELEMETRY_RECEIVER_DUPLICATE: &str = "telemetry_receiver_duplicate";
/// A service receiving telemetry runs more than one replica.
pub const TELEMETRY_RECEIVER_REPLICAS: &str = "telemetry_receiver_replicas";
/// A service receives telemetry, but its environment is not granted it, so
/// it runs without the telemetry socket.
pub const TELEMETRY_NOT_GRANTED: &str = "telemetry_not_granted";
/// A release's manifest renders a build input differently for another
/// environment than for the release, so its images do not apply there.
pub const RELEASE_INCOMPATIBLE: &str = "release_incompatible";

/// Same-name runtime resource is not owned by this application.
pub const UNOWNED_NAME_COLLISION: &str = "unowned_name_collision";
/// Immutable runtime configuration differs from desired state.
pub const IMMUTABLE_CONFIGURATION_DRIFT: &str = "immutable_configuration_drift";
/// Foreign or unowned resource is skipped by the plan.
pub const FOREIGN_RESOURCE_IGNORED: &str = "foreign_resource_ignored";
/// An owned service update failed in the runtime.
pub const SERVICE_UPDATE_FAILED: &str = "service_update_failed";
/// Obsolete owned resources await removal until earlier changes converge.
pub const CLEANUP_DEFERRED: &str = "cleanup_deferred";
/// A release image is missing and can't be pulled again, so the release
/// can't be deployed.
pub const IMAGE_UNAVAILABLE: &str = "image_unavailable";
