use crate::{
    cli::Cli,
    error::{CliError, ErrorKind, Result},
};
use piqueld_client::{ActionReason, ActionRisk, Operation, PlanView};
use serde::Serialize;
use serde_json::json;
use std::io::{self, Write};

pub(crate) fn report_operation(operation: &Operation) {
    eprintln!("operation {}: {}", operation.id, operation.state);
}

pub(crate) fn render_operation(cli: &Cli, operation: &Operation) -> Result<()> {
    if cli.json {
        return emit_json(operation);
    }
    writeln!(
        io::stdout().lock(),
        "operation {}: {}",
        operation.id,
        operation.state
    )?;
    writeln!(
        io::stdout().lock(),
        "application {}",
        operation.application_id
    )?;
    if let Some(message) = &operation.error_message {
        eprintln!("diagnostic: {message}");
    }
    Ok(())
}

pub(crate) fn render_plan(plan: &PlanView, output: &mut impl Write) -> io::Result<()> {
    writeln!(output, "application {}", plan.application_id)?;
    let summary = plan.plan.summary();
    writeln!(
        output,
        "{} action(s), {} mutation(s), {} destructive action(s), {} blocking conflict(s)",
        summary.action_count,
        summary.mutation_count,
        summary.destructive_count,
        summary.blocking_conflicts,
    )?;
    for (index, action) in plan.plan.actions.iter().enumerate() {
        writeln!(
            output,
            "  {:>3} [{}] {} ({})",
            index + 1,
            risk_text(action.kind.risk()),
            action.human_description(),
            reason_text(&action.reason),
        )?;
    }
    for diagnostic in &plan.plan.diagnostics {
        writeln!(
            output,
            "  diagnostic {} [{}]: {}{}",
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

pub(crate) fn render_plan_stderr(plan: &PlanView) -> io::Result<()> {
    let stderr = io::stderr();
    let mut output = stderr.lock();
    render_plan(plan, &mut output)
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
