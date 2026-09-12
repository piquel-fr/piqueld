use crate::{
    cli::Cli,
    error::{CliError, ErrorKind, Result},
};
use piqueld_client::{ActionReason, ActionRisk, Operation, PlanView};
use serde::Serialize;
use serde_json::json;
use std::io::{self, Write};

pub(crate) fn report_operation(operation: &Operation) {
    let phase = operation.phase.as_deref().map(humanize).unwrap_or_default();
    match (phase.is_empty(), operation.resource.as_deref()) {
        (true, None) => eprintln!("  {}", operation.state),
        (false, None) => eprintln!("  {:<10} · {phase}", operation.state),
        (true, Some(resource)) => eprintln!("  {:<10} · {resource}", operation.state),
        (false, Some(resource)) => {
            eprintln!("  {:<10} · {phase} · {resource}", operation.state);
        }
    }
}

pub(crate) fn render_operation(cli: &Cli, operation: &Operation) -> Result<()> {
    if cli.json {
        return emit_json(operation);
    }
    writeln!(
        io::stdout().lock(),
        "Operation: {}\n  State:       {}\n  Application: {}",
        operation.id,
        operation.state,
        operation.application_id
    )?;
    if let Some(phase) = operation.phase.as_deref() {
        writeln!(io::stdout().lock(), "  Phase:       {}", humanize(phase))?;
    }
    if let Some(resource) = operation.resource.as_deref() {
        writeln!(io::stdout().lock(), "  Resource:    {resource}")?;
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
                (None, Some(after)) => ('+', after.clone()),
                (Some(before), None) => ('-', before.clone()),
                (Some(before), Some(after)) => ('~', format!("{before} → {after}")),
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
