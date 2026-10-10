-- Previews are environments of another kind: disposable deployments of one
-- branch of the application's manifest repository, configured by
-- `[spec.previews]`. They reuse every environment table. A preview is named
-- by its slug, so the existing unique name index keeps preview slugs and
-- environment names of one application apart.
ALTER TABLE environments ADD COLUMN kind TEXT NOT NULL DEFAULT 'environment'
    CHECK (kind = 'environment' OR (kind = 'preview' AND branch IS NOT NULL AND pinned_commit IS NULL));
-- Distinguishes several previews of one branch.
ALTER TABLE environments ADD COLUMN preview_slot TEXT CHECK (preview_slot IS NULL OR kind = 'preview');

-- Creating a preview is idempotent on its branch and slot. Creation never
-- needs the application revision; this index is what keeps it unique.
CREATE UNIQUE INDEX preview_key ON environments(application_id, branch, COALESCE(preview_slot, ''))
    WHERE kind = 'preview';

-- Every Docker volume a preview's deployments created, recorded before they
-- create it, including volumes later manifests dropped. Deleting the preview
-- removes them all and verifies they are gone before the preview row goes.
CREATE TABLE preview_volumes (
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (environment_id, name)
) STRICT;
