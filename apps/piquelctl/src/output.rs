use crate::{
    cli::Cli,
    error::{CliError, ErrorKind, Result},
};
use piqueld_client::{ActionReason, ActionRisk, Operation, PlanView};
use serde::Serialize;
use serde_json::json;
use std::io::{self, IsTerminal, Write};

pub(crate) fn render_operation(cli: &Cli, operation: &Operation) -> Result<()> {
    if cli.json {
        return emit_json(operation);
    }
    writeln!(
        cli.output(),
        "Operation: {}\n  State:       {}\n  Application: {}",
        operation.id,
        operation.state,
        operation.application_id
    )?;
    if let Some(phase) = operation.phase.as_deref() {
        writeln!(cli.output(), "  Phase:       {}", humanize(phase))?;
    }
    if let Some(resource) = operation.resource.as_deref() {
        writeln!(cli.output(), "  Resource:    {resource}")?;
    }
    if let Some(message) = &operation.error_message {
        eprintln!("\nDiagnostic: {message}");
    }
    Ok(())
}

pub(crate) fn render_plan(plan: &PlanView, output: &mut impl Write) -> io::Result<()> {
    writeln!(output, "Application: {}", plan.application_id)?;
    if plan.identical {
        return writeln!(output, "\nNo changes. The manifest is already current.");
    }
    if !plan.changes.is_empty() {
        writeln!(output, "\nChanges:")?;
        for change in &plan.changes {
            let (marker, value) = match (&change.before, &change.after) {
                (None, Some(after)) => ('+', HumanOutput::value(after)),
                (Some(before), None) => ('-', HumanOutput::value(before)),
                (Some(before), Some(after)) => (
                    '~',
                    format!(
                        "{} → {}",
                        HumanOutput::value(before),
                        HumanOutput::value(after)
                    ),
                ),
                (None, None) => ('~', "absent".into()),
            };
            writeln!(output, "  {marker} {:<32} {}", change.field, value)?;
        }
    }
    let summary = plan.plan.summary();
    writeln!(
        output,
        "\nActions: {} total · {} runtime mutations · {} destructive · {} blocking",
        summary.action_count,
        summary.mutation_count,
        summary.destructive_count,
        summary.blocking_conflicts,
    )?;
    for (index, action) in plan.plan.actions.iter().enumerate() {
        writeln!(
            output,
            "  {:>2}. {}\n      {} · {}",
            index + 1,
            sentence_case(&action.human_description()),
            risk_text(action.kind.risk()),
            reason_text(&action.reason),
        )?;
    }
    for diagnostic in &plan.plan.diagnostics {
        writeln!(
            output,
            "\nDiagnostic: {} [{}]\n  {}{}",
            diagnostic.code,
            diagnostic.resource,
            diagnostic.message,
            if diagnostic.blocking {
                " (blocking)"
            } else {
                ""
            },
        )?;
    }
    Ok(())
}

pub(crate) fn blocked_plan_error(plan: &PlanView, include_plan_details: bool) -> CliError {
    let codes = plan
        .plan
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.blocking)
        .map(|diagnostic| diagnostic.code.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut error = CliError::new(
        ErrorKind::Conflict,
        if codes.is_empty() {
            "plan contains blocking diagnostics".into()
        } else {
            format!("plan is blocked ({codes})")
        },
    );
    if include_plan_details {
        error = error.with_details(json!({"plan": plan}));
    }
    error
}

fn risk_text(risk: ActionRisk) -> &'static str {
    match risk {
        ActionRisk::None => "no risk",
        ActionRisk::Availability => "availability",
        ActionRisk::DataAdjacent => "data-adjacent",
        ActionRisk::Destructive => "destructive",
    }
}

fn humanize(value: &str) -> String {
    let mut words = value.split('_');
    let Some(first) = words.next() else {
        return String::new();
    };
    let mut text = first.to_owned();
    for word in words {
        text.push(' ');
        text.push_str(word);
    }
    if let Some(first) = text.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    text
}

fn sentence_case(value: &str) -> String {
    let mut text = value.to_ascii_lowercase();
    if let Some(first) = text.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    text
}

fn reason_text(reason: &ActionReason) -> String {
    match reason {
        ActionReason::Missing => "missing".into(),
        ActionReason::Drift { fields } if fields.is_empty() => "drift".into(),
        ActionReason::Drift { fields } => format!("drift ({})", fields.join(", ")),
        ActionReason::Obsolete => "obsolete".into(),
        ActionReason::ConvergencePending => "convergence pending".into(),
        ActionReason::ResolutionRequired => "resolution required".into(),
        ActionReason::ApplicationDeletion => "application deletion".into(),
        ActionReason::VolumeRetentionPolicy => "volume retention policy".into(),
    }
}

pub(crate) fn emit_json<T: Serialize>(value: &T) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value).map_err(|error| {
        CliError::new(
            ErrorKind::General,
            format!("could not encode JSON: {error}"),
        )
    })?;
    writeln!(output).map_err(|error| {
        CliError::new(ErrorKind::General, format!("could not write JSON: {error}"))
    })?;
    Ok(())
}

/// Human output has a single formatting boundary; JSON bypasses it entirely.
pub(crate) struct HumanOutput {
    quiet: bool,
    color: bool,
}
impl HumanOutput {
    /// API change values may contain serialized configuration; human output unwraps it.
    fn value(value: &str) -> String {
        serde_json::from_str(value)
            .map_or_else(|_| value.to_owned(), |value| Self::configuration(&value))
    }

    fn configuration(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::Null => "none".into(),
            serde_json::Value::String(value) => value.clone(),
            serde_json::Value::Array(values) if values.is_empty() => "none".into(),
            serde_json::Value::Array(values) => values
                .iter()
                .map(Self::configuration)
                .collect::<Vec<_>>()
                .join(", "),
            serde_json::Value::Object(values) => {
                if values.get("type").and_then(serde_json::Value::as_str) == Some("image")
                    && let Some(image) = values.get("image").and_then(serde_json::Value::as_str)
                {
                    return format!("image {image}");
                }
                if values.is_empty() {
                    return "none".into();
                }
                values
                    .iter()
                    .map(|(key, value)| {
                        format!("{}: {}", key.replace('_', " "), Self::configuration(value))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }
            value => value.to_string(),
        }
    }

    fn line(line: &str, output: &mut impl Write) -> io::Result<()> {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let change_color = match trimmed.as_bytes().get(..2) {
            Some(b"+ ") => Some("32"),
            Some(b"- ") => Some("31"),
            Some(b"~ ") => Some("33"),
            _ => None,
        };
        if let Some(color) = change_color {
            let end = trimmed[2..]
                .find(char::is_whitespace)
                .map_or(trimmed.trim_end().len(), |end| end + 2);
            let (field, value) = line.split_at(indent + end);
            return write!(output, "\x1b[{color}m{field}\x1b[0m{value}");
        }
        // Labels are standalone words followed by a colon, never colons inside values.
        if let Some(end) = trimmed
            .find(':')
            .filter(|end| !trimmed[..*end].contains(char::is_whitespace))
        {
            let (label, value) = line.split_at(indent + end + 1);
            return write!(output, "\x1b[1;36m{label}\x1b[0m{value}");
        }
        output.write_all(line.as_bytes())
    }
}
impl Cli {
    pub(crate) fn output(&self) -> HumanOutput {
        HumanOutput {
            quiet: self.quiet,
            color: io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none()
                && std::env::var("TERM").as_deref() != Ok("dumb"),
        }
    }
}
impl Write for HumanOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.quiet {
            Ok(bytes.len())
        } else {
            io::stdout().lock().write(bytes)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        io::stdout().lock().flush()
    }
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
        if self.quiet {
            return Ok(());
        }
        let text = args.to_string();
        let mut output = io::stdout().lock();
        for line in text.split_inclusive('\n') {
            if self.color {
                Self::line(line, &mut output)?;
            } else {
                output.write_all(line.as_bytes())?;
            }
        }
        Ok(())
    }
}

enum ProgressStyle {
    Plain,
    Terminal,
    Color,
}

pub(crate) struct Progress {
    quiet: bool,
    style: ProgressStyle,
    started: std::time::Instant,
    last: Option<String>,
    drawn: bool,
    last_second: u64,
}
impl Progress {
    pub(crate) fn new(cli: &Cli, id: &str) -> Self {
        let terminal = io::stderr().is_terminal() && std::env::var("TERM").as_deref() != Ok("dumb");
        if !cli.quiet {
            eprintln!("\nProgress: {id}");
        }
        Self {
            quiet: cli.quiet,
            style: if !terminal {
                ProgressStyle::Plain
            } else if std::env::var_os("NO_COLOR").is_some() {
                ProgressStyle::Terminal
            } else {
                ProgressStyle::Color
            },
            started: std::time::Instant::now(),
            last: None,
            drawn: false,
            last_second: 0,
        }
    }
    pub(crate) fn update(&mut self, operation: &Operation) {
        if self.quiet {
            return;
        }
        let line = format!(
            "{:<10} · {}{}",
            operation.state,
            operation.phase.as_deref().map(humanize).unwrap_or_default(),
            operation
                .resource
                .as_ref()
                .map_or_else(String::new, |r| format!(" · {r}"))
        );
        let elapsed = self.started.elapsed().as_secs();
        if self.last.as_ref() == Some(&line)
            && (matches!(self.style, ProgressStyle::Plain) || elapsed == self.last_second)
        {
            return;
        }
        if matches!(self.style, ProgressStyle::Plain) {
            eprintln!("  {line}  [{elapsed}s]");
        } else {
            let code = match operation.state {
                piqueld_client::OperationState::Succeeded => "1;32",
                piqueld_client::OperationState::Failed => "1;31",
                _ => "1;36",
            };
            if matches!(self.style, ProgressStyle::Color) {
                eprint!("\r\x1b[2K  \x1b[{code}m{line}\x1b[0m  [{elapsed}s]");
            } else {
                eprint!("\r\x1b[2K  {line}  [{elapsed}s]");
            }
            let _ = io::stderr().flush();
            self.drawn = true;
        }
        self.last = Some(line);
        self.last_second = elapsed;
    }
}
impl Drop for Progress {
    fn drop(&mut self) {
        if self.drawn {
            eprintln!();
        }
    }
}

#[cfg(test)]
mod presentation_tests {
    use super::HumanOutput;

    #[test]
    fn preview_configuration_is_readable() {
        assert_eq!(
            HumanOutput::value(r#"{"image":"nginx:alpine","type":"image"}"#),
            "image nginx:alpine"
        );
        assert_eq!(HumanOutput::value("[]"), "none");
        assert_eq!(HumanOutput::value("null"), "none");
        assert_eq!(HumanOutput::value("<redacted>"), "<redacted>");
    }

    #[test]
    fn change_colors_end_before_values_and_their_colons() {
        for (marker, color) in [("+", "32"), ("-", "31"), ("~", "33")] {
            let mut output = Vec::new();
            HumanOutput::line(
                &format!("  {marker} services.web.source   image nginx:alpine\n"),
                &mut output,
            )
            .unwrap();
            assert_eq!(
                String::from_utf8(output).unwrap(),
                format!(
                    "\x1b[{color}m  {marker} services.web.source\x1b[0m   image nginx:alpine\n"
                )
            );
        }
    }
}
