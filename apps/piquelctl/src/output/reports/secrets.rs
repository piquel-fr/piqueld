//! Reports about stored secrets and the secret key.

use piqueld_client::{EnvironmentView, SecretMetadata, StoredSecret};

// Secret metadata list, flagging unreadable values and pending deletions.
report!(Vec<SecretMetadata> as [SecretMetadata], self, out, {
    for secret in self {
        out.line(format_args!(
            "{}  generation {}{}{}",
            secret.name,
            secret.generation,
            if secret.unavailable {
                "  value unavailable; the next deploy generates a new one"
            } else {
                ""
            },
            if secret.deleting {
                "  deletion pending; retry delete"
            } else {
                ""
            }
        ))?;
    }
    Ok(())
});

/// Stored secrets with their access, naming environments.
pub(crate) struct StoredSecretsReport<'a> {
    pub(crate) secrets: &'a [StoredSecret],
    pub(crate) environments: &'a [EnvironmentView],
}
report!(StoredSecretsReport<'_> as [StoredSecret] = secrets, self, out, {
    for secret in self.secrets {
        let metadata = &secret.metadata;
        out.line(format_args!(
            "{}  generation {}  {}{}{}",
            metadata.name,
            metadata.generation,
            secret.access.describe(self.environments),
            if metadata.unavailable {
                "  value unavailable; replace value and deploy"
            } else {
                ""
            },
            if metadata.deleting {
                "  deletion pending; retry delete"
            } else {
                ""
            }
        ))?;
    }
    Ok(())
});

/// One stored secret after a change, naming the environments that may mount it.
pub(crate) struct StoredSecretReport<'a> {
    pub(crate) secret: &'a StoredSecret,
    pub(crate) environments: &'a [EnvironmentView],
}
report!(StoredSecretReport<'_> as StoredSecret = secret, self, out, {
    out.line(format_args!(
        "Saved {} generation {}, mountable by {}. Deploy to use it.",
        self.secret.metadata.name,
        self.secret.metadata.generation,
        self.secret.access.describe(self.environments)
    ))
});

report!(SecretMetadata, self, out, {
    out.line(format_args!(
        "Generated {} generation {}. Deploy the environment to use it.",
        self.name, self.generation
    ))
});

report!(piqueld_client::SecretKeyRecovery, self, out, {
    out.line(format_args!(
        "Secret key recovered: {} values discarded across {} secrets in {} environments and {} applications.",
        self.discarded_versions,
        self.affected_secrets,
        self.affected_environments,
        self.affected_applications,
    ))?;
    out.line(
        "Running services keep their Docker secrets. Supply replacement values, then deploy.",
    )?;
    if self.deleted_destinations > 0 {
        out.line(format_args!(
            "{} notification destinations were deleted with their URLs; create them again.",
            self.deleted_destinations
        ))?;
    }
    out.line("The next stored value generates a new key; back it up with the database.")
});
