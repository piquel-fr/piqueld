//! What an operation's timeline expects, from its environment's history.
use super::{Store, StoreError, decode_name};
use piqueld_core::Operation;
use piqueld_core::timeline::{ExpectedStep, StepKey};
use std::collections::BTreeMap;

/// How many recent successful operations a timeline's expectations come from.
const BASELINE_OPERATIONS: i64 = 5;

/// The steps a timeline forecasts, and how long steps usually take.
pub struct Baseline {
    /// The steps of the environment's latest successful operation of the
    /// same kind, in the order they ran.
    pub forecast: Vec<StepKey>,
    /// The median duration of each step across recent successful operations.
    pub expected: Vec<ExpectedStep>,
}

impl Store {
    /// The baseline of `operation`'s timeline: what the environment's recent
    /// successful operations of its kind ran, in two indexed reads.
    ///
    /// # Errors
    /// Returns storage errors, or corruption for an unknown stored phase.
    pub async fn timeline_baseline(&self, operation: &Operation) -> Result<Baseline, StoreError> {
        let environment = operation.environment_id.as_str();
        let kind = operation.kind.as_str();
        let mut tx = self.snapshot().await?;
        let previous = sqlx::query!(
            r#"SELECT id AS "id!", attempt FROM operations
            WHERE environment_id=?1 AND kind=?2 AND state='succeeded' AND id<>?3
            ORDER BY created_at_ms DESC, id DESC LIMIT ?4"#,
            environment,
            kind,
            operation.id,
            BASELINE_OPERATIONS,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let Some(latest) = previous.first() else {
            return Ok(Baseline {
                forecast: Vec::new(),
                expected: Vec::new(),
            });
        };
        let ids = serde_json::to_string(&previous.iter().map(|row| &row.id).collect::<Vec<_>>())
            .map_err(StoreError::corrupt)?;
        let actions = sqlx::query!(
            r#"SELECT operation_id AS "operation_id!", attempt, phase AS "phase!", resource,
            duration_ms AS "duration_ms!"
            FROM events
            WHERE operation_id IN (SELECT value FROM json_each(?1)) AND kind='action_succeeded'
            AND phase IS NOT NULL AND duration_ms IS NOT NULL
            ORDER BY id"#,
            ids,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        let mut forecast = Vec::new();
        let mut durations = BTreeMap::<(String, Option<String>), Vec<u64>>::new();
        for action in actions {
            let duration = u64::try_from(action.duration_ms).map_err(StoreError::corrupt)?;
            if action.operation_id == latest.id && action.attempt == Some(latest.attempt) {
                let key = StepKey {
                    phase: decode_name(&action.phase)?,
                    resource: action.resource.clone(),
                };
                if !forecast.contains(&key) {
                    forecast.push(key);
                }
            }
            durations
                .entry((action.phase, action.resource))
                .or_default()
                .push(duration);
        }
        let expected = durations
            .into_iter()
            .map(|((phase, resource), mut durations)| {
                durations.sort_unstable();
                Ok(ExpectedStep {
                    key: StepKey {
                        phase: decode_name(&phase)?,
                        resource,
                    },
                    duration_ms: durations[durations.len() / 2],
                })
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(Baseline { forecast, expected })
    }
}
