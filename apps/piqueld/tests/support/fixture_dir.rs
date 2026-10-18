//! Directories whose ownership and permissions the daemon checks.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// `path` relative to the working directory when below it, so permission
/// checks inspect only the owned fixture tree, never the synthetic root or
/// `/tmp` of a Nix build sandbox, whose owners are unmapped in its user
/// namespace.
pub fn relative(path: &Path) -> &Path {
    let cwd = std::env::current_dir().expect("working directory");
    path.strip_prefix(cwd).unwrap_or(path)
}

/// The permission bits of `path`, without following a symlink.
pub fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("fixture metadata")
        .permissions()
        .mode()
        & 0o777
}

/// Sets the permission bits of `path` to `mode`.
pub fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .expect("fixture permissions are set");
}
