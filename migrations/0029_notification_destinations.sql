-- Notification destinations, managed through the API instead of the daemon
-- configuration. Each URL is encrypted with the secret master key; lost-key
-- recovery deletes the destinations.
CREATE TABLE notification_destinations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('json', 'discord')),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    -- Every application, or only those in notification_destination_applications.
    all_applications INTEGER NOT NULL CHECK (all_applications IN (0, 1)),
    previews INTEGER NOT NULL CHECK (previews IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

-- The categories each destination receives, with the text sent alongside.
-- `after_event_id` is the last event before the destination started
-- receiving the category, so adding a category, enabling the destination or
-- changing its URL never replays history.
CREATE TABLE notification_destination_categories (
    destination_id TEXT NOT NULL REFERENCES notification_destinations(id) ON DELETE CASCADE,
    category TEXT NOT NULL,
    message TEXT,
    after_event_id INTEGER NOT NULL,
    PRIMARY KEY (destination_id, category)
) STRICT;

CREATE TABLE notification_destination_applications (
    destination_id TEXT NOT NULL REFERENCES notification_destinations(id) ON DELETE CASCADE,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    PRIMARY KEY (destination_id, application_id)
) STRICT;

-- Which events each destination is notified of: daemon history, and events
-- of the applications it selects, of a preview only when it takes previews.
-- Queries narrow it to one event or destination.
CREATE VIEW notification_audience AS
SELECT d.id AS destination_id, e.id AS event_id
FROM notification_destinations d JOIN events e
WHERE e.scope = 'daemon' OR (
    (d.all_applications = 1 OR EXISTS (
        SELECT 1 FROM notification_destination_applications a
        WHERE a.destination_id = d.id AND a.application_id = e.application_id))
    AND (d.previews = 1 OR NOT EXISTS (
        SELECT 1 FROM environments v WHERE v.id = e.environment_id AND v.kind = 'preview'))
);

-- Destinations now carry their own activation watermarks.
DROP TABLE notification_routes;

-- A delivery belongs to the destination it was made for until that
-- destination is deleted or gets a new URL or kind, which detaches it: it
-- keeps the name for history, and recoveries of its failures are never sent.
ALTER TABLE notification_deliveries ADD COLUMN destination_id TEXT
    REFERENCES notification_destinations(id) ON DELETE SET NULL;
ALTER TABLE notification_deliveries DROP COLUMN destination_fingerprint;
CREATE INDEX delivery_destination ON notification_deliveries(destination_id);

-- Configured destinations are gone, so nothing pending can be sent.
UPDATE notification_deliveries
SET state = 'cancelled', last_error = 'Destinations moved out of the daemon configuration'
WHERE state = 'pending';
