-- Journaled phases are typed: `phase` only names what an action does.
-- Typed configuration edits recorded the edited field there; it moves into
-- the event's structured detail. Progress names that were never actions take
-- the name of the action they stood for, and any other value is cleared.
ALTER TABLE events ADD COLUMN detail_json TEXT;

UPDATE events
SET detail_json = json_object('type', 'edited', 'field', phase), phase = NULL
WHERE kind = 'application_edited' AND phase IS NOT NULL;

UPDATE events SET phase = 'run_job' WHERE phase = 'running_job';
UPDATE operations SET phase = 'run_job' WHERE phase = 'running_job';
UPDATE events SET phase = 'preparing' WHERE phase = 'preparing_sources';
UPDATE operations SET phase = 'preparing' WHERE phase = 'preparing_sources';

CREATE TEMP TABLE known_phases(name TEXT PRIMARY KEY);
INSERT INTO known_phases(name) VALUES
    ('preparing'),
    ('fetching_manifest'),
    ('building_git'),
    ('resolving_image'),
    ('planning'),
    ('run_job'),
    ('remove_jobs'),
    ('ensure_network'),
    ('ensure_volume'),
    ('ensure_service'),
    ('wait_for_service'),
    ('wait_for_service_removal'),
    ('remove_service'),
    ('remove_network'),
    ('retain_volume'),
    ('detach_network'),
    ('remove_volumes'),
    ('remove_secrets'),
    ('routing'),
    ('observation'),
    ('ensure_swarm'),
    ('remove_image'),
    ('ingress_create_network'),
    ('ingress_start_gateway'),
    ('ingress_stop_gateway'),
    ('ingress_remove_container'),
    ('ingress_pull_image'),
    ('ingress_replace_gateway'),
    ('ingress_start_tunnel'),
    ('ingress_start_tailnet'),
    ('ingress_recover_gateway'),
    ('ingress_configure_routes'),
    ('ingress_dns_create'),
    ('ingress_dns_update'),
    ('ingress_dns_delete'),
    ('ingress_certificate_issue'),
    ('ingress_certificate_renew'),
    ('ingress_certificate_delete'),
    ('ingress_certificate_challenge_delete');
UPDATE events SET phase = NULL
WHERE phase IS NOT NULL AND phase NOT IN (SELECT name FROM known_phases);
UPDATE operations SET phase = NULL
WHERE phase IS NOT NULL AND phase NOT IN (SELECT name FROM known_phases);
DROP TABLE known_phases;
