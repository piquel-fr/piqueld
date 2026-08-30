use crate::cli::Cli;
use piqueld_client::ClientError;
use serde_json::Value;
use std::{fmt, process::ExitCode};

#[derive(Clone, Copy, Debug)]
pub(crate) enum ErrorKind {
    General,
    Input,
    Conflict,
    Unavailable,
    Operation,
    Interrupted,
}

impl ErrorKind {
    const fn exit_code(self) -> u8 {
        match self {
            Self::General => 1,
            Self::Input => 2,
            Self::Conflict => 3,
            Self::Unavailable => 4,
            Self::Operation => 5,
            Self::Interrupted => 130,
        }
    }
}

#[derive(Debug)]
pub(crate) struct CliError {
    kind: ErrorKind,
    message: String,
    api_code: Option<String>,
    request_id: Option<String>,
    details: Option<Value>,
}

impl CliError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            api_code: None,
            request_id: None,
            details: None,
        }
    }

    pub(crate) fn api(mut self, code: String, request_id: String, details: Value) -> Self {
        self.api_code = Some(code);
        self.request_id = (!request_id.is_empty()).then_some(request_id);
        self.details = (!details.is_null()).then_some(details);
        self
    }

    pub(crate) fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl From<ClientError> for CliError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Endpoint { .. } => Self::new(
                ErrorKind::Input,
                "HTTP endpoint must be a loopback http:// origin without credentials, path, query, or fragment",
            ),
            ClientError::Transport { message } => Self::new(
                ErrorKind::Unavailable,
                format!("could not connect to the piqueld API: {message}"),
            ),
            ClientError::Decode { .. } => Self::new(
                ErrorKind::General,
                "the daemon returned an invalid public API response",
            ),
            ClientError::Api { status, error } => {
                let kind = match status.as_u16() {
                    400 | 404 | 413 | 415 | 422 => ErrorKind::Input,
                    // 412 has no current producer; kept for forward compatibility.
                    409 | 412 => ErrorKind::Conflict,
                    502..=504 => ErrorKind::Unavailable,
                    _ => ErrorKind::General,
                };
                Self::new(kind, format!("{} ({})", error.message, error.code)).api(
                    error.code,
                    error.request_id,
                    error.details,
                )
            }
        }
    }
}

pub(crate) type Result<T> = std::result::Result<T, CliError>;

pub(crate) fn finish_error(cli: &Cli, error: CliError) -> ExitCode {
    if cli.json {
        eprintln!(
            "piquelctl: {}{}{}{}",
            error.message,
            error
                .api_code
                .as_deref()
                .map_or_else(String::new, |code| format!("; API code {code}")),
            error
                .request_id
                .as_deref()
                .map_or_else(String::new, |id| format!("; request ID {id}")),
            error
                .details
                .as_ref()
                .map_or_else(String::new, |details| format!("; details {details}")),
        );
    } else {
        eprintln!("piquelctl: {}", error.message);
        if let Some(code) = error.api_code {
            eprintln!("API code: {code}");
        }
        if let Some(request_id) = error.request_id {
            eprintln!("request ID: {request_id}");
        }
        if let Some(details) = error.details {
            eprintln!("details: {details}");
        }
    }
    ExitCode::from(error.kind.exit_code())
}
