use crate::{
    cli::Cli,
    error::{CliError, ErrorKind, Result},
};
use piqueld_client::{OperationView, PlanView};
use serde::Serialize;
use serde_json::json;
use std::{
    fmt,
    io::{self, Write},
};

pub(crate) fn report_operation(operation: &OperationView) {
    eprintln!("operation {}: {}", operation.id, operation.state);
    for step in &operation.steps {
        eprintln!(
            "  {:>3} {}: {} (attempt {})",
            step.position, step.action, step.state, step.attempt
        );
        if let Some(message) = &step.error_message {
            eprintln!("      {message}");
        }
    }
}

pub(crate) fn render_operation(cli: &Cli, operation: &OperationView) -> Result<()> {
    if cli.json {
        return emit_json(operation);
    }
    println!("operation {}: {}", operation.id, operation.state);
    println!(
        "application {} generation {}",
        operation.application_id, operation.generation
    );
    for step in &operation.steps {
        println!("  {} {}: {}", step.position, step.action, step.state);
    }
    if let Some(message) = &operation.error_message {
        eprintln!("diagnostic: {message}");
    }
    Ok(())
}

pub(crate) fn render_plan(plan: &PlanView, output: &mut impl Write) -> io::Result<()> {
    writeln!(
        output,
        "application {} proposed generation {}",
        plan.application_id, plan.proposed_generation
    )?;
    writeln!(
        output,
        "{} action(s), {} mutation(s), {} destructive action(s), {} blocking conflict(s)",
        plan.plan.summary.action_count,
        plan.plan.summary.mutation_count,
        plan.plan.summary.destructive_count,
        plan.plan.summary.blocking_conflicts,
    )?;
    for action in &plan.plan.actions {
        writeln!(
            output,
            "  {:>3} [{:?}] {} ({})",
            action.sequence,
            action.risk,
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

pub(crate) fn render_plan_stderr(plan: &PlanView) {
    eprintln!(
        "plan: {} action(s), {} mutation(s), {} destructive action(s), {} blocking conflict(s)",
        plan.plan.summary.action_count,
        plan.plan.summary.mutation_count,
        plan.plan.summary.destructive_count,
        plan.plan.summary.blocking_conflicts,
    );
    for action in &plan.plan.actions {
        eprintln!(
            "  {:>3} [{:?}] {} ({})",
            action.sequence,
            action.risk,
            action.human_description(),
            reason_text(&action.reason),
        );
    }
    for diagnostic in &plan.plan.diagnostics {
        eprintln!(
            "  diagnostic {} [{}]: {}{}",
            diagnostic.code,
            diagnostic.resource,
            diagnostic.message,
            if diagnostic.blocking {
                " (blocking)"
            } else {
                ""
            },
        );
    }
}

pub(crate) fn blocked_plan_error(plan: &PlanView) -> CliError {
    let codes = plan
        .plan
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.blocking)
        .map(|diagnostic| diagnostic.code.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    CliError::new(
        ErrorKind::Conflict,
        if codes.is_empty() {
            "plan contains blocking diagnostics".into()
        } else {
            format!("plan is blocked ({codes})")
        },
    )
    .with_details(json!({"plan": plan}))
}

fn reason_text(reason: &impl fmt::Debug) -> String {
    format!("{reason:?}")
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
