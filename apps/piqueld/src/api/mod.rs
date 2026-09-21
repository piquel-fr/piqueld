//! Shared daemon API and its transport adapters.

pub mod http;
mod service;

pub use service::{
    ApplicationError, ApplicationService, ManifestExport, Mutation, MutationResponse,
};
