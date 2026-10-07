-- Saved manifests and deployment candidates become templates in which
-- `${{ namespace.name }}` references a variable. Escape text written before
-- variables existed, so it keeps its literal meaning. `${{` only occurs inside
-- JSON strings, so a textual replace escapes exactly those.
UPDATE applications SET desired_json=replace(desired_json,'${{','$${{');
UPDATE deployment_inputs SET application_json=replace(application_json,'${{','$${{');

-- A deployment snapshot records the manifest as captured, with references
-- unresolved, and the values they rendered to. `manifest_json` keeps the
-- rendered manifest; it is JSON null until a repository-backed manifest is
-- fetched. Earlier deployments captured literal manifests and no variables.
ALTER TABLE deployments ADD COLUMN template_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(template_json));
ALTER TABLE deployments ADD COLUMN variables_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(variables_json));
UPDATE deployments SET template_json=replace(manifest_json,'${{','$${{');
