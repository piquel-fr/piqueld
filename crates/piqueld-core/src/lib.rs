//! Pure, deterministic contracts shared by every piqueld interface.
//!
//! This crate deliberately has no transport, persistence, container-runtime, or
//! user-interface dependencies.

pub mod error;

pub use error::{ErrorCode, PublicError};
