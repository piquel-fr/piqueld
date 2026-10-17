//! Error classification, exit codes, stable error codes, and error reports:
//! human with connection hints, or one JSON object with `--json`.
use crate::{
    cli::Cli,
    output::{DiagnosticReport, Event, HumanWriter},
};
use piqueld_client::{ClientError, PlanView, PreviewLimitReached, TransportFailure};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{fmt, io};

/// Failure classes, each mapped to a stable process exit code.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ErrorKind {
    /// Unexpected client, output, or response failure (exit 1).
    General,
    /// Invalid arguments, configuration, or unconfirmed action (exit 2).
    Input,
    /// Generation precondition, name ambiguity, blocked plan, or a
    /// superseded deployment (exit 3).
    Conflict,
    /// Daemon unreachable, gateway errors, or command timeout (exit 4).
    Unavailable,
    /// An awaited operation ended unsuccessfully (exit 5).
    Operation,
    /// Ctrl-C (exit 130, the shell convention for SIGINT).
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

    /// The code of a CLI-side failure of this class with no more specific one.
    const fn code(self) -> CliCode {
        match self {
            Self::General => CliCode::Failed,
            Self::Input => CliCode::InvalidInput,
            Self::Conflict => CliCode::Conflict,
            Self::Unavailable => CliCode::Unavailable,
            Self::Operation => CliCode::OperationFailed,
            Self::Interrupted => CliCode::Interrupted,
        }
    }
}

/// Stable codes of failures the CLI detects itself, reported with `--json`
/// in place of an API error code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CliCode {
    /// Unexpected client or output failure.
    Failed,
    /// Arguments the command line parser rejected.
    Usage,
    /// Invalid input, or an unconfirmed action.
    InvalidInput,
    /// Invalid connection configuration from a flag, variable, or profile.
    InvalidConfiguration,
    /// An endpoint rejected before connecting.
    InvalidEndpoint,
    /// The request failed before the daemon answered.
    ConnectionFailed,
    /// One request exceeded `--timeout`.
    RequestTimeout,
    /// The whole command exceeded `--timeout`; server-side work goes on.
    Timeout,
    /// The answer is not a valid piqueld API response.
    InvalidResponse,
    /// The daemon became unavailable.
    Unavailable,
    /// A conflict found by the CLI, such as an ambiguous name or a blocked plan.
    Conflict,
    /// The awaited deployment was superseded by a newer one.
    DeploymentSuperseded,
    /// An awaited operation ended unsuccessfully.
    OperationFailed,
    /// Ctrl-C.
    Interrupted,
}

/// A failure's stable code: the daemon's, or the CLI's own.
#[derive(Debug)]
enum Code {
    /// From an API error response.
    Api(String),
    Cli(CliCode),
}

/// Command failure with an exit class, a stable code, a one-line message,
/// and optional API context rendered by `ErrorReport`.
#[derive(Debug)]
pub(crate) struct CliError {
    kind: ErrorKind,
    code: Code,
    /// The message, without the API code a human report appends.
    message: String,
    request_id: Option<String>,
    /// Structured context; an `operation` object gets a dedicated rendering.
    details: Option<Value>,
    /// Selects which connection facts and hint the report shows.
    diagnostic: Option<Box<Diagnostic>>,
}

/// Connection-related failure category, used to pick report context and hints.
#[derive(Debug)]
enum Diagnostic {
    /// The endpoint was rejected before connecting; its value must not be echoed.
    Endpoint,
    /// Invalid configuration from the named source (flag, variable, or profile).
    Configuration(String),
    /// The request failed at the transport level.
    Transport(TransportFailure),
    /// The daemon answered with something that is not a valid piqueld API
    /// response, with its HTTP status when it was an error.
    Response(Option<http::StatusCode>),
    /// The whole command exceeded `--timeout`.
    CommandTimeout,
}

impl Diagnostic {
    /// The CLI code of a failure this diagnostic explains.
    const fn code(&self) -> CliCode {
        match self {
            Self::Endpoint => CliCode::InvalidEndpoint,
            Self::Configuration(_) => CliCode::InvalidConfiguration,
            Self::Transport(TransportFailure::Timeout) => CliCode::RequestTimeout,
            Self::Transport(_) => CliCode::ConnectionFailed,
            Self::Response(_) => CliCode::InvalidResponse,
            Self::CommandTimeout => CliCode::Timeout,
        }
    }

    /// What to check next, for a report with `cli`'s connection settings.
    fn hint(&self, cli: &Cli) -> &'static str {
        match self {
            Self::Endpoint => {
                "Check the selected endpoint configuration; use a Unix socket or a plain HTTP origin."
            }
            Self::Configuration(_) => {
                "Check the configuration source above. Profiles require exactly one socket or URL and an optional positive timeout."
            }
            Self::Transport(TransportFailure::Connect(kind)) => match kind {
                io::ErrorKind::NotFound if cli.url.is_none() => {
                    "Check the socket path and whether the daemon has created its socket."
                }
                io::ErrorKind::PermissionDenied if cli.url.is_none() => {
                    "Check whether your user has access to the socket and its parent directories."
                }
                io::ErrorKind::PermissionDenied => {
                    "Check whether local network access is permitted for this process."
                }
                io::ErrorKind::ConnectionRefused => {
                    "Check whether the daemon is listening at the selected endpoint."
                }
                _ => {
                    "Check the selected endpoint and whether the daemon is listening and accessible."
                }
            },
            Self::Transport(TransportFailure::Timeout) => {
                "Check daemon responsiveness and whether the configured timeout is sufficient."
            }
            Self::CommandTimeout => {
                "Check daemon responsiveness and whether the timeout allows the command to finish. A server-side operation may still be running."
            }
            Self::Transport(TransportFailure::Exchange) | Self::Response(_) => {
                "Check that the selected endpoint serves the piqueld API and inspect the daemon logs."
            }
        }
    }
}

impl CliError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: Code::Cli(kind.code()),
            message: message.into(),
            request_id: None,
            details: None,
            diagnostic: None,
        }
    }

    /// Arguments the command line parser rejected.
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Input, message).code(CliCode::Usage)
    }

    /// Replaces the class's default code with a more specific one.
    pub(crate) fn code(mut self, code: CliCode) -> Self {
        self.code = Code::Cli(code);
        self
    }

    /// Attaches a connection diagnostic and its code; the public builders
    /// below wrap this.
    fn diagnostic(mut self, diagnostic: Diagnostic) -> Self {
        self.code = Code::Cli(diagnostic.code());
        self.diagnostic = Some(Box::new(diagnostic));
        self
    }

    /// Marks the error as caused by configuration from `source`.
    pub(crate) fn configuration(self, source: String) -> Self {
        self.diagnostic(Diagnostic::Configuration(source))
    }

    /// Marks the error as a whole-command timeout.
    pub(crate) fn command_timeout(self) -> Self {
        self.diagnostic(Diagnostic::CommandTimeout)
    }

    /// Whether one request exceeded `--timeout`.
    pub(crate) fn is_request_timeout(&self) -> bool {
        matches!(self.code, Code::Cli(CliCode::RequestTimeout))
    }

    /// Marks the error as an invalid daemon response.
    pub(crate) fn invalid_response(self) -> Self {
        self.diagnostic(Diagnostic::Response(None))
    }

    /// Undecodable response body, reported as an invalid API response.
    fn decode_failure(source: impl fmt::Display) -> Self {
        Self::new(
            ErrorKind::General,
            format!("the daemon returned an invalid public API response: {source}"),
        )
        .invalid_response()
    }

    pub(crate) fn exit_code(&self) -> u8 {
        self.kind.exit_code()
    }

    /// The JSON form of the error, without connection facts.
    pub(crate) fn body(&self) -> ErrorBody<'_> {
        ErrorBody {
            code: match &self.code {
                Code::Api(code) => ErrorCode::Api(code),
                Code::Cli(code) => ErrorCode::Cli(*code),
            },
            message: &self.message,
            details: self.details.clone(),
            request_id: self.request_id.as_deref(),
        }
    }

    pub(crate) fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Keep blocking reasons visible even when quiet mode hides the plan result.
    ///
    /// ```text
    /// plan is blocked (<code> [<resource>]: <message>, <code> [<resource>]: <message>)
    /// ```
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

/// The human one-line message: an API error's message with its code, and
/// the HTTP status of an invalid response.
impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)?;
        match (&self.code, self.diagnostic.as_deref()) {
            (Code::Api(code), Some(Diagnostic::Response(Some(status)))) => {
                write!(formatter, " ({code}, HTTP {status})")
            }
            (Code::Api(code), _) => write!(formatter, " ({code})"),
            (Code::Cli(_), _) => Ok(()),
        }
    }
}

impl std::error::Error for CliError {}

/// I/O errors surface from output writes, so they are phrased as such. Input
/// and credential-file I/O map their own errors.
impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::new(
            ErrorKind::General,
            format!("could not write output: {error}"),
        )
    }
}

/// Classifies client errors: endpoint rejections are input errors, transport failures
/// are unavailability, and API errors map by HTTP status. Redirects and malformed error
/// bodies also get the invalid-response diagnostic and include the HTTP status.
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
                .then_some(Diagnostic::Response(Some(status)));
                Self {
                    kind,
                    code: Code::Api(error.code),
                    message: error.message,
                    request_id: (!error.request_id.is_empty()).then_some(error.request_id),
                    details: (!error.details.is_null()).then_some(error.details),
                    diagnostic: diagnostic.map(Box::new),
                }
            }
        }
    }
}

pub(crate) type Result<T> = std::result::Result<T, CliError>;

/// Human rendering of a `CliError` on stderr, with connection context from the
/// resolved CLI configuration.
pub(crate) struct ErrorReport<'a> {
    error: &'a CliError,
    cli: &'a Cli,
    /// Set for per-application warnings, which render as `Warning` instead of `Error`.
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
    /// Non-fatal report for one application whose status could not be fetched.
    pub(crate) fn warning(error: &'a CliError, cli: &'a Cli, application: &'a str) -> Self {
        Self {
            error,
            cli,
            application: Some(application),
        }
    }
    /// Only evidence available from configuration and the failed exchange: the
    /// endpoint (or configuration source), and timeout facts for timeouts. Each
    /// is a human label, its JSON key, and its value.
    fn connection_facts(
        &self,
        diagnostic: &Diagnostic,
    ) -> Vec<(&'static str, &'static str, String)> {
        let cli = self.cli;
        let mut facts = Vec::new();
        if let Diagnostic::Configuration(source) = diagnostic {
            facts.push((
                "  Configuration source",
                "configuration_source",
                source.clone(),
            ));
        } else {
            // Rejected endpoint input can contain credentials; never echo it.
            if !matches!(diagnostic, Diagnostic::Endpoint) {
                facts.push(("  Endpoint", "endpoint", cli.transport()));
            }
            facts.push((
                "  Endpoint source",
                "endpoint_source",
                cli.connection_sources.endpoint.to_string(),
            ));
        }
        if matches!(
            diagnostic,
            Diagnostic::CommandTimeout | Diagnostic::Transport(TransportFailure::Timeout)
        ) {
            facts.push((
                "  Timeout",
                "timeout",
                crate::support::format_duration(cli.timeout),
            ));
            facts.push((
                "  Timeout source",
                "timeout_source",
                cli.connection_sources.timeout.to_string(),
            ));
        }
        facts
    }

    /// The connection facts a human report shows, as JSON details, with the
    /// HTTP status of an invalid response.
    fn connection_details(&self) -> Option<Value> {
        let diagnostic = self.error.diagnostic.as_deref()?;
        let mut details = self
            .connection_facts(diagnostic)
            .into_iter()
            .map(|(_, key, value)| (key.to_owned(), Value::String(value)))
            .collect::<Map<_, _>>();
        if let Diagnostic::Response(Some(status)) = diagnostic {
            details.insert("http_status".into(), json!(status.as_u16()));
        }
        Some(Value::Object(details))
    }

    /// The one-line message of a per-application warning.
    fn warning_message(&self) -> Option<String> {
        self.application
            .map(|application| format!("{application}: status unavailable: {}", self.error))
    }

    /// Renders the connection facts and a hint chosen from the diagnostic.
    fn connection(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        let Some(diagnostic) = self.error.diagnostic.as_deref() else {
            return Ok(());
        };
        for (label, _, value) in self.connection_facts(diagnostic) {
            out.label(label, value)?;
        }
        out.label("  Hint", diagnostic.hint(self.cli))
    }

    /// Renders error details. Operation failures get a labelled context block and a
    /// reconcile hint, and `preview_limit_reached` lists the previews it counts;
    /// any other details are printed as raw JSON.
    fn details(details: &Value, out: &mut HumanWriter<'_>) -> io::Result<()> {
        // Local and daemon manifest validation share the `ValidationErrors` shape.
        if let Some(errors) = details.get("errors").and_then(Value::as_array) {
            for error in errors {
                let field = |name| error.get(name).and_then(Value::as_str).unwrap_or("?");
                out.line(format_args!(
                    "  {}: {} ({})",
                    field("path"),
                    field("message"),
                    field("code")
                ))?;
            }
            return Ok(());
        }
        if let Ok(reached) = serde_json::from_value::<PreviewLimitReached>(details.clone()) {
            for counted in &reached.previews {
                out.label(
                    "  Preview",
                    format_args!(
                        "{} of branch {}{}, {}{}",
                        counted.preview.slug,
                        counted.preview.branch,
                        counted
                            .preview
                            .slot
                            .as_ref()
                            .map_or_else(String::new, |slot| format!(" (slot {slot})")),
                        counted.last_deployment.as_ref().map_or_else(
                            || "never deployed".into(),
                            |deployment| format!(
                                "last deployed at Unix ms {}",
                                deployment.created_at_ms
                            )
                        ),
                        if counted.deleting { " (deleting)" } else { "" },
                    ),
                )?;
            }
            return out.label(
                "  Hint",
                "delete one with `piquelctl preview delete <APP> <SLUG>`",
            );
        }
        let Some(operation) = details.get("operation") else {
            return out.label("Details", details);
        };
        out.blank()?;
        out.heading("Context:")?;
        for (label, field) in [
            ("Operation", "id"),
            ("Environment", "environment_id"),
            ("Phase", "phase"),
            ("Resource", "resource"),
            ("Code", "error_code"),
            ("Message", "error_message"),
        ] {
            if let Some(value) = operation.get(field).and_then(Value::as_str) {
                out.label(label, value)?;
            }
        }
        if let Some(environment) = operation.get("environment_id").and_then(Value::as_str) {
            out.label(
                "Hint",
                format_args!("retry with `piquelctl env reconcile <APP> {environment}`"),
            )?;
        }
        Ok(())
    }
}
impl DiagnosticReport for ErrorReport<'_> {
    fn render(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        let error = self.error;
        match self.warning_message() {
            Some(message) => out.label("Warning", message)?,
            None => out.label("Error", error)?,
        }
        if let Code::Api(code) = &error.code {
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

    fn event(&self) -> Event<'_> {
        if let Some(message) = self.warning_message() {
            return Event::Warning { message };
        }
        let mut body = self.error.body();
        if body.details.is_none() {
            body.details = self.connection_details();
        }
        Event::Error(body)
    }
}

/// A failure with `--json`: the API's error envelope, whose `code` is the
/// daemon's or a [`CliCode`]. `request_id` is present for API errors.
#[derive(Serialize)]
pub(crate) struct ErrorBody<'a> {
    code: ErrorCode<'a>,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<&'a str>,
}

/// An API error code, or the CLI's own, serialized as a string.
#[derive(Serialize)]
#[serde(untagged)]
enum ErrorCode<'a> {
    Api(&'a str),
    Cli(CliCode),
}
