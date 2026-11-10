-- An operation's current phase and resource are no longer stored on it: they
-- are those of the latest journaled step of its current attempt (an action,
-- or a recorded plan), so the event journal stays their only record. A failed
-- operation names its latest failed action, if any, since steps run at once
-- (sources) may finish after it; a retried request is no failure. A requested
-- operation has none until an attempt starts.
CREATE INDEX event_operation_step ON events(operation_id, attempt, id)
    WHERE phase IS NOT NULL;

CREATE VIEW operation_progress AS
SELECT o.id AS operation_id,
    CASE WHEN o.state = 'requested' THEN NULL ELSE step.phase END AS phase,
    CASE WHEN o.state = 'requested' THEN NULL ELSE step.resource END AS resource
FROM operations o
LEFT JOIN events step ON step.id = COALESCE(
    CASE WHEN o.state = 'failed' THEN (
        SELECT failed.id FROM events failed
        WHERE failed.operation_id = o.id AND failed.attempt = o.attempt
            AND failed.phase IS NOT NULL AND failed.kind = 'action_failed'
        ORDER BY failed.id DESC LIMIT 1
    ) END,
    (
        SELECT latest.id FROM events latest
        WHERE latest.operation_id = o.id AND latest.attempt = o.attempt
            AND latest.phase IS NOT NULL
        ORDER BY latest.id DESC LIMIT 1
    )
);

ALTER TABLE operations DROP COLUMN phase;
ALTER TABLE operations DROP COLUMN resource;
