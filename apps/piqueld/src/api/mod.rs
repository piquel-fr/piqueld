//! Shared daemon API and its transport adapters.

pub mod http;
mod service;

pub use service::{
    Actor, ApplicationError, ApplicationService, Delivery, ExecSession, ListedHead, ManifestExport,
    Mutation, MutationResponse, PreviewMutation, SyncOutcome, WEBHOOK_BODY_LIMIT, WEBHOOK_PATH,
    WebhookError,
};
