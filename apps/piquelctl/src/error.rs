use crate::cli::Cli;
use piqueld_client::{ClientError, TransportFailure};
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
    diagnostic: Option<Box<Diagnostic>>,
}

#[derive(Debug)]
enum Diagnostic {
    Endpoint,
    Configuration(String),
    Transport(TransportFailure),
    Response,
    CommandTimeout,
}

impl CliError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            api_code: None,
            request_id: None,
            details: None,
            diagnostic: None,
        }
    }

    fn diagnostic(mut self, diagnostic: Diagnostic) -> Self {
        self.diagnostic = Some(Box::new(diagnostic));
        self
    }

    pub(crate) fn configuration(self, source: String) -> Self {
        self.diagnostic(Diagnostic::Configuration(source))
    }

    pub(crate) fn command_timeout(self) -> Self {
        self.diagnostic(Diagnostic::CommandTimeout)
    }

    pub(crate) fn invalid_response(self) -> Self {
        self.diagnostic(Diagnostic::Response)
    }

    /// Render only evidence available from configuration and the failed exchange.
    pub(crate) fn render_connection(&self, cli: &Cli) {
        let Some(diagnostic) = self.diagnostic.as_deref() else {
            return;
        };
        if let Diagnostic::Configuration(source) = diagnostic {
            eprintln!("  Configuration source: {source}");
        } else {
            // Rejected endpoint input can contain credentials; never echo it.
            if !matches!(diagnostic, Diagnostic::Endpoint) {
                eprintln!("  Endpoint: {}", crate::support::transport_description(cli));
            }
            eprintln!("  Endpoint source: {}", cli.connection_sources.endpoint);
        }
        if matches!(
            diagnostic,
            Diagnostic::CommandTimeout | Diagnostic::Transport(TransportFailure::Timeout)
        ) {
            eprintln!(
                "  Timeout: {}",
                crate::support::format_duration(cli.timeout)
            );
            eprintln!("  Timeout source: {}", cli.connection_sources.timeout);
        }
        let hint = match diagnostic {
            Diagnostic::Endpoint => {
                "Check the selected endpoint configuration; use a Unix socket or a plain loopback HTTP origin."
            }
            Diagnostic::Configuration(_) => {
                "Check the configuration source above. Profiles require exactly one socket or URL and an optional positive timeout."
            }
            Diagnostic::Transport(TransportFailure::Connect(kind)) => match kind {
                std::io::ErrorKind::NotFound if cli.url.is_none() => {
                    "Check the socket path and whether the daemon has created its socket."
                }
                std::io::ErrorKind::PermissionDenied if cli.url.is_none() => {
                    "Check whether your user has access to the socket and its parent directories."
                }
                std::io::ErrorKind::PermissionDenied => {
                    "Check whether local network access is permitted for this process."
                }
                std::io::ErrorKind::ConnectionRefused => {
                    "Check whether the daemon is listening at the selected endpoint."
                }
                _ => {
                    "Check the selected endpoint and whether the daemon is listening and accessible."
                }
            },
            Diagnostic::Transport(TransportFailure::Timeout) => {
                "Check daemon responsiveness and whether the configured timeout is sufficient."
            }
            Diagnostic::CommandTimeout => {
                "Check daemon responsiveness and whether the timeout allows the command to finish. A server-side operation may still be running."
            }
            Diagnostic::Transport(TransportFailure::Exchange) | Diagnostic::Response => {
                "Check that the selected endpoint serves the piqueld API and inspect the daemon logs."
            }
        };
        eprintln!("  Hint: {hint}");
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

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::new(
            ErrorKind::General,
            format!("could not write output: {error}"),
        )
    }
}

impl From<ClientError> for CliError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Endpoint { message } => {
                Self::new(ErrorKind::Input, message).diagnostic(Diagnostic::Endpoint)
            }
            ClientError::Transport { message, kind } => Self::new(
                ErrorKind::Unavailable,
                format!("piqueld API request failed: {message}"),
            )
            .diagnostic(Diagnostic::Transport(kind)),
            ClientError::Decode { source } => Self::new(
                ErrorKind::General,
                format!("the daemon returned an invalid public API response: {source}"),
            )
            .invalid_response(),
            ClientError::Api { status, error } => {
                let kind = match status.as_u16() {
                    400 | 404 | 413 | 415 | 422 => ErrorKind::Input,
                    // 412 has no current producer; kept for forward compatibility.
                    409 | 412 => ErrorKind::Conflict,
                    502..=504 => ErrorKind::Unavailable,
                    _ => ErrorKind::General,
                };
                let diagnostic = (error.code == "invalid_error_response"
                    || status.is_redirection())
                .then_some(Diagnostic::Response);
                let message = if diagnostic.is_some() {
                    format!("{} ({}, HTTP {status})", error.message, error.code)
                } else {
                    format!("{} ({})", error.message, error.code)
                };
                let mut result =
                    Self::new(kind, message).api(error.code, error.request_id, error.details);
                result.diagnostic = diagnostic.map(Box::new);
                result
            }
        }
    }
}

pub(crate) type Result<T> = std::result::Result<T, CliError>;

pub(crate) fn finish_error(cli: &Cli, error: &CliError) -> ExitCode {
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
        if let Some(application) = error
            .details
            .as_ref()
            .and_then(|details| details.get("operation"))
            .and_then(|operation| operation.get("application_id"))
            .and_then(Value::as_str)
        {
            eprintln!("hint: retry with `piquelctl reconcile {application}`");
        }
    } else {
        eprintln!("Error: {}", error.message);
        if let Some(code) = &error.api_code {
            eprintln!("  API code:   {code}");
        }
        if let Some(request_id) = &error.request_id {
            eprintln!("  Request ID: {request_id}");
        }
        if let Some(details) = &error.details {
            render_details(details);
        }
    }
    error.render_connection(cli);
    ExitCode::from(error.kind.exit_code())
}

fn render_details(details: &Value) {
    let Some(operation) = details.get("operation") else {
        if let Ok(details) = serde_json::to_string_pretty(details) {
            eprintln!("\nDetails:\n{details}");
        }
        return;
    };

    eprintln!("\nContext:");
    for (label, field) in [
        ("Operation", "id"),
        ("Application", "application_id"),
        ("Phase", "phase"),
        ("Resource", "resource"),
        ("Code", "error_code"),
        ("Message", "error_message"),
    ] {
        if let Some(value) = operation.get(field).and_then(Value::as_str) {
            eprintln!("  {label:<12} {value}");
        }
    }
    if let Some(application) = operation.get("application_id").and_then(Value::as_str) {
        eprintln!("\nHint: retry with `piquelctl reconcile {application}`");
    }
}
