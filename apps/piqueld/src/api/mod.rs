//! Shared daemon API and its transport adapters.

pub mod http;
mod service;

pub use service::{
    Actor, ApplicationError, ApplicationService, ExecSession, ManifestExport, Mutation,
    MutationResponse, PreviewMutation,
};
