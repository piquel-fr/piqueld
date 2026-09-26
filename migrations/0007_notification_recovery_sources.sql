-- A recovery is eligible only at destinations that received its failure alerts.
CREATE TABLE notification_recovery_sources (
    recovery_id TEXT NOT NULL REFERENCES notification_deliveries(id) ON DELETE CASCADE,
    failure_id TEXT NOT NULL REFERENCES notification_deliveries(id) ON DELETE CASCADE,
    PRIMARY KEY(recovery_id, failure_id)
);
CREATE INDEX recovery_failure ON notification_recovery_sources(failure_id);

-- Earlier queued recoveries have no trustworthy per-destination failure pairing.
UPDATE notification_deliveries
SET state='cancelled', last_error='Recovery delivery has no recorded failure destination'
WHERE category='recovery' AND state='pending';
