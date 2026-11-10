//! Operation timelines: the steps a deployment ran, folded from its events,
//! and the forecast its environment's next deployment starts from.

use super::*;
use piqueld_core::Phase;
use piqueld_core::timeline::{StepKey, StepState};

/// The steps of `timeline`, as phase, resource and state.
fn steps(timeline: &piqueld_core::timeline::Timeline) -> Vec<(StepKey, StepState)> {
    timeline
        .steps
        .iter()
        .map(|step| (step.key.clone(), step.state))
        .collect()
}

#[tokio::test]
async fn deployments_record_their_steps_and_forecast_the_next_one() {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    let done = harness.applications().timeline(&first.id).await.unwrap();
    assert_eq!(done.operation.state, OperationState::Succeeded);
    assert!(
        done.steps
            .iter()
            .all(|step| step.state == StepState::Succeeded && step.finished_at_ms.is_some()),
        "{done:#?}"
    );
    let phases: Vec<_> = done.steps.iter().map(|step| step.key.phase).collect();
    for phase in [Phase::ResolveImage, Phase::EnsureService] {
        assert!(phases.contains(&phase), "{phases:?}");
    }

    // The next deployment starts from the steps the first one ran, each
    // expected to take as long as it did.
    let second = harness.apply(manifest(), Some(1)).await;
    let forecast = harness.applications().timeline(&second.id).await.unwrap();
    assert_eq!(
        steps(&forecast),
        done.steps
            .iter()
            .map(|step| (step.key.clone(), StepState::Forecast))
            .collect::<Vec<_>>()
    );
    assert!(forecast.steps.iter().all(|step| step.expected_ms.is_some()));
    assert!(
        forecast
            .remaining_ms(forecast.operation.created_at_ms)
            .is_some()
    );

    harness.finish(&second).await;
    let deployed = harness.applications().timeline(&second.id).await.unwrap();
    assert!(
        deployed
            .steps
            .iter()
            .all(|step| step.state == StepState::Succeeded),
        "{deployed:#?}"
    );
}

#[tokio::test]
async fn waiting_for_a_rollout_records_its_tasks_as_they_start_and_run() {
    use piqueld_core::resource::TaskState;
    let harness = ControllerHarness::new().await;
    harness
        .docker
        .slow_services
        .lock()
        .await
        // The convergence loop observes once before it waits.
        .insert("web".into(), 3);
    let operation = harness.apply(manifest(), Some(0)).await;
    harness.finish(&operation).await;
    let timeline = harness
        .applications()
        .timeline(&operation.id)
        .await
        .unwrap();
    let wait = timeline
        .steps
        .iter()
        .find(|step| step.key.phase == Phase::WaitForService)
        .expect("the deployment waited for web");
    assert!(!wait.tasks.is_empty(), "{wait:#?}");
    for lane in &wait.tasks {
        assert!(lane.task.current);
        let states: Vec<_> = lane.spans.iter().map(|span| span.state).collect();
        assert_eq!(states, [TaskState::Starting, TaskState::Running]);
    }
    assert!(wait.monitor_seconds.is_some());
    // A new service has nothing to replace, so nothing stopped serving.
    assert_eq!(wait.gaps(), []);
}
