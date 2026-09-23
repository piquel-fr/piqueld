-- Deployment cursors and ordering use operation IDs, not creation timestamps.
DROP INDEX deployment_application;
CREATE INDEX deployment_application ON deployments(application_id,id DESC);
