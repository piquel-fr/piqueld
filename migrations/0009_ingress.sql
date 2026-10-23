-- Ingress: each application's routes and the hostnames they reserve. Routing
-- intent is durable before gateway I/O, so an interrupted gateway update is
-- reapplied from it. The accepted projection retains hostname ownership until
-- a removal has actually reached the gateway. Reservation checks cover saved
-- configuration, captured deployment inputs, resolved targets and both
-- projections in one transaction.
CREATE TABLE application_routes (
    application_id TEXT PRIMARY KEY REFERENCES applications(id) ON DELETE CASCADE,
    desired_json TEXT NOT NULL CHECK(json_valid(desired_json)),
    applied_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(applied_json))
) STRICT;

CREATE TABLE hostname_reservations (
    hostname TEXT PRIMARY KEY,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE
) STRICT;
CREATE INDEX hostname_reservations_application ON hostname_reservations(application_id);

-- Hostnames the installation serves itself, such as the website origin. Routes
-- may not claim them or their subdomains. Replaced from configuration at startup.
CREATE TABLE installation_hostnames (
    hostname TEXT PRIMARY KEY
) STRICT;
