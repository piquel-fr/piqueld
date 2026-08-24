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
/// Replica count is outside its supported range.
pub const REPLICAS_OUT_OF_RANGE: &str = "replicas_out_of_range";
/// Image reference is not a safe registry reference.
pub const IMAGE_INVALID: &str = "image_invalid";
/// Environment name violates its alphabet rule.
pub const ENVIRONMENT_NAME_INVALID: &str = "environment_name_invalid";
/// Environment value contains forbidden bytes.
pub const ENVIRONMENT_VALUE_INVALID: &str = "environment_value_invalid";
/// Environment map exceeds its entry budget.
pub const ENVIRONMENT_COUNT_EXCESSIVE: &str = "environment_count_excessive";
/// Environment value exceeds its byte budget.
pub const ENVIRONMENT_VALUE_EXCESSIVE: &str = "environment_value_excessive";
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

/// Service source has no immutable resolution yet.
pub const SOURCE_UNRESOLVED: &str = "source_unresolved";
/// Resolved source does not immutably match the requested service source.
pub const SOURCE_RESOLUTION_MISMATCH: &str = "source_resolution_mismatch";

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
