use super::*;
use crate::testing::prebuilt;
use piqueld_core::{EnvironmentId, manifest::ApplicationTemplate};

/// How a test deployment finished.
#[derive(Clone, Copy)]
enum Outcome {
    /// Still requested.
    Pending,
    Failed {
        old: bool,
    },
    Succeeded {
        old: bool,
        promoted: bool,
    },
}

/// A store keeping three deployments per environment, and every one for a
/// day, and one successful one, with `production` of a saved application.
async fn store() -> (tempfile::TempDir, Store, EnvironmentId) {
    let (temp, mut store) = Store::temporary().await;
    store.deployment_retention = DeploymentRetention {
        days: 1,
        per_environment: 3,
        succeeded: 1,
    };
    let template = ApplicationTemplate::from(&prebuilt("app-history-test"));
    let template = template.to_manifest().validate_template().unwrap();
    let saved = store.save_template(template, false).await;
    let production = store.saved_production(&saved).await;
    (temp, store, production)
}

/// Requests a deployment of `environment` for each of `outcomes`, oldest
/// first, and records how it finished, returning their IDs.
async fn deploy(store: &Store, environment: &EnvironmentId, outcomes: &[Outcome]) -> Vec<String> {
    let mut ids = Vec::new();
    for outcome in outcomes {
        let id = store.request_deploy(environment, None).await.unwrap().id;
        let (state, old, succeeded, promoted) = match *outcome {
            Outcome::Pending => {
                ids.push(id);
                continue;
            }
            Outcome::Failed { old } => ("failed", old, false, false),
            Outcome::Succeeded { old, promoted } => ("succeeded", old, true, promoted),
        };
        let finished = if old { 0 } else { crate::now_ms() };
        sqlx::query("UPDATE operations SET state=?1,finished_at_ms=?2,promoted=?3 WHERE id=?4")
            .bind(state)
            .bind(finished)
            .bind(promoted)
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE deployments SET succeeded_at_ms=?1 WHERE id=?2")
            .bind(succeeded.then_some(finished))
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        ids.push(id);
    }
    ids
}

/// The IDs of `environment`'s remaining deployments, oldest first.
async fn remaining(store: &Store, environment: &EnvironmentId) -> Vec<String> {
    let mut ids = store
        .deployments(environment, None, 100)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|deployment| deployment.operation.id)
        .collect::<Vec<_>>();
    ids.reverse();
    ids
}

/// Runs `sql` on `store`, binding `id`.
async fn execute(store: &Store, sql: &str, id: &str) {
    sqlx::query(sql)
        .bind(id)
        .execute(&store.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn deployments_go_once_past_both_limits_unless_current_latest_or_restorable() {
    let (_temp, store, production) = store().await;
    let old = Outcome::Failed { old: true };
    let recent = Outcome::Failed { old: false };
    let ids = deploy(
        &store,
        &production,
        &[
            old,
            old,
            old,
            Outcome::Succeeded {
                old: true,
                promoted: true,
            },
            Outcome::Succeeded {
                old: true,
                promoted: false,
            },
            recent,
            old,
            recent,
            Outcome::Pending,
        ],
    )
    .await;
    execute(
        &store,
        "INSERT INTO job_cleanup(operation_id) VALUES(?1)",
        &ids[0],
    )
    .await;
    execute(
        &store,
        "UPDATE deployments SET origin_json=json_object('type','restore','deployment',?1) WHERE id=(SELECT MAX(id) FROM deployments)",
        &ids[1],
    )
    .await;
    store.prune_deployment_history().await.unwrap();
    // Only the third goes: the first still needs job cleanup, the latest
    // restored the second, the fourth is current, the fifth the newest
    // successful, the sixth recent, and the seventh among the newest three.
    let kept = |indexes: &[usize]| {
        indexes
            .iter()
            .map(|&index| ids[index].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        remaining(&store, &production).await,
        kept(&[0, 1, 3, 4, 5, 6, 7, 8])
    );

    // A preview keeps only those, of the deployments that finished over an
    // hour ago, however few it has.
    execute(
        &store,
        "UPDATE environments SET kind='preview',branch='feature' WHERE id=?1",
        production.as_str(),
    )
    .await;
    store.prune_deployment_history().await.unwrap();
    assert_eq!(
        remaining(&store, &production).await,
        kept(&[0, 1, 3, 4, 5, 7, 8])
    );
    // Its origins, which follow restores back, still resolve.
    store.preview_origins(&production).await.unwrap();
}

#[tokio::test]
async fn pruning_removes_expired_builds_and_unused_releases_of_pruned_deployments() {
    let (_temp, store, production) = store().await;
    let ids = deploy(
        &store,
        &production,
        &[
            Outcome::Failed { old: true },
            Outcome::Failed { old: true },
            Outcome::Failed { old: true },
            Outcome::Failed { old: true },
            Outcome::Pending,
        ],
    )
    .await;
    let application = store
        .get(&production)
        .await
        .unwrap()
        .environment
        .application_id;
    let application = application.as_str();
    // Builds of the pruned first deployment, one with its output expired,
    // and an expired one of the retained latest deployment.
    for (operation, expired) in [(&ids[0], true), (&ids[0], false), (&ids[4], true)] {
        sqlx::query("INSERT INTO builds(environment_id,operation_id,service,source_json,state,started_at_ms,log_expired) VALUES(?1,?2,'web','{}','succeeded',0,?3)")
            .bind(production.as_str())
            .bind(operation)
            .bind(expired)
            .execute(&store.pool)
            .await
            .unwrap();
    }
    // The oldest release, which the latest deployment runs, then 51 that no
    // deployment uses.
    for index in 0..=KEPT_RELEASES + 1 {
        sqlx::query("INSERT INTO releases(id,application_id,content_hash,release_json,created_at_ms) VALUES(?1,?2,?1,'{}',0)")
            .bind(format!("rel-test-{index:03}"))
            .bind(application)
            .execute(&store.pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE deployments SET release_id='rel-test-000' WHERE id=?1")
        .bind(&ids[4])
        .execute(&store.pool)
        .await
        .unwrap();

    store.prune_deployment_history().await.unwrap();
    assert_eq!(remaining(&store, &production).await, ids[2..]);
    let builds: Vec<(String, bool)> =
        sqlx::query_as("SELECT operation_id,log_expired FROM builds ORDER BY id")
            .fetch_all(&store.pool)
            .await
            .unwrap();
    assert_eq!(builds, [(ids[0].clone(), false), (ids[4].clone(), true)]);
    // The oldest unused release goes; the newest 50, and the one in use, stay.
    let releases: Vec<String> = sqlx::query_scalar("SELECT id FROM releases ORDER BY id")
        .fetch_all(&store.pool)
        .await
        .unwrap();
    assert_eq!(releases.len(), 51);
    assert_eq!(releases[..2], ["rel-test-000", "rel-test-002"]);
}

#[tokio::test]
async fn deployment_analytics_over_pruned_history_are_incomplete() {
    let (_temp, store, production) = store().await;
    let old = Outcome::Failed { old: true };
    deploy(&store, &production, &[old, old, old, old, Outcome::Pending]).await;
    // History recorded since the epoch, so only pruning can make it partial.
    execute(
        &store,
        "UPDATE history_coverage SET started_at_ms=0 WHERE singleton=?1",
        "1",
    )
    .await;
    let incomplete = async || {
        store
            .deployment_analytics(None, &crate::Visibility::ALL, 0, crate::now_ms())
            .await
            .unwrap()
            .incomplete
    };
    assert!(!incomplete().await);
    store.prune_deployment_history().await.unwrap();
    assert!(incomplete().await);
}
