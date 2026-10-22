//! Shared daemon API and its transport adapters.

pub mod http;
mod service;

pub use service::{
    Actor, ApplicationError, ApplicationService, Delivery, ExecSession, Expensive, ListedHead,
    ManifestExport, Mutation, MutationResponse, PreviewMutation, PromotionMutation, Restore,
    SourceChoice, SyncOutcome, WEBHOOK_BODY_LIMIT, WEBHOOK_PATH, WebhookError,
};
