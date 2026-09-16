use crate::{
    cli::Cli,
    output::{DiagnosticReport, HumanWriter},
};
use piqueld_client::{ClientError, PlanView, TransportFailure};
use serde_json::Value;
use std::{fmt, io, process::ExitCode};

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

    fn decode_failure(source: impl fmt::Display) -> Self {
        Self::new(
            ErrorKind::General,
            format!("the daemon returned an invalid public API response: {source}"),
        )
        .invalid_response()
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.kind.exit_code())
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

    /// Keep blocking reasons visible even when quiet mode hides the plan result.
    pub(crate) fn blocked_plan(plan: &PlanView) -> Self {
        let diagnostics = plan
            .plan
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.blocking)
            .map(|diagnostic| {
                format!(
                    "{} [{}]: {}",
                    diagnostic.code, diagnostic.resource, diagnostic.message
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        Self::new(
            ErrorKind::Conflict,
            if diagnostics.is_empty() {
                "plan contains blocking diagnostics".into()
            } else {
                format!("plan is blocked ({diagnostics})")
            },
        )
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
            ClientError::Decode { source } => Self::decode_failure(source),
            ClientError::TextDecode { source } => Self::decode_failure(source),
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

pub(crate) struct ErrorReport<'a> {
    error: &'a CliError,
    cli: &'a Cli,
    application: Option<&'a str>,
}
impl<'a> ErrorReport<'a> {
    /// Borrow resolved configuration at emission time, never a stale startup copy.
    pub(crate) fn new(error: &'a CliError, cli: &'a Cli) -> Self {
        Self {
            error,
            cli,
            application: None,
        }
    }
    pub(crate) fn warning(error: &'a CliError, cli: &'a Cli, application: &'a str) -> Self {
        Self {
            error,
            cli,
            application: Some(application),
        }
    }
    /// Render only evidence available from configuration and the failed exchange.
    fn connection(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        let cli = self.cli;
        let Some(diagnostic) = self.error.diagnostic.as_deref() else {
            return Ok(());
        };
        if let Diagnostic::Configuration(source) = diagnostic {
            out.label("  Configuration source", source)?;
        } else {
            // Rejected endpoint input can contain credentials; never echo it.
            if !matches!(diagnostic, Diagnostic::Endpoint) {
                out.label("  Endpoint", crate::support::transport_description(cli))?;
            }
            out.label("  Endpoint source", &cli.connection_sources.endpoint)?;
        }
        if matches!(
            diagnostic,
            Diagnostic::CommandTimeout | Diagnostic::Transport(TransportFailure::Timeout)
        ) {
            out.label("  Timeout", crate::support::format_duration(cli.timeout))?;
            out.label("  Timeout source", &cli.connection_sources.timeout)?;
        }
        let hint = match diagnostic {
            Diagnostic::Endpoint => {
                "Check the selected endpoint configuration; use a Unix socket or a plain HTTP origin."
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
        out.label("  Hint", hint)
    }

    fn details(details: &Value, out: &mut HumanWriter<'_>) -> io::Result<()> {
        let Some(operation) = details.get("operation") else {
            return out.label("Details", details);
        };
        out.blank()?;
        out.heading("Context:")?;
        for (label, field) in [
            ("Operation", "id"),
            ("Application", "application_id"),
            ("Phase", "phase"),
            ("Resource", "resource"),
            ("Code", "error_code"),
            ("Message", "error_message"),
        ] {
            if let Some(value) = operation.get(field).and_then(Value::as_str) {
                out.label(label, value)?;
            }
        }
        if let Some(application) = operation.get("application_id").and_then(Value::as_str) {
            out.label(
                "Hint",
                format_args!("retry with `piquelctl reconcile {application}`"),
            )?;
        }
        Ok(())
    }
}
impl DiagnosticReport for ErrorReport<'_> {
    fn render(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        let error = self.error;
        if let Some(application) = self.application {
            out.label(
                "Warning",
                format_args!("{application}: status unavailable: {}", error.message),
            )?;
        } else {
            out.label("Error", &error.message)?;
        }
        if let Some(code) = &error.api_code {
            out.label("  API code", code)?;
        }
        if let Some(id) = &error.request_id {
            out.label("  Request ID", id)?;
        }
        if let Some(details) = &error.details {
            Self::details(details, out)?;
        }
        self.connection(out)
    }
}
