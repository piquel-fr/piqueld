//! Checked requested and immutable image references at the runtime boundary.

use crate::{
    names::validated_string,
    resource::{Sha256Digest, image_name},
};

validated_string!(
    /// A syntactically valid requested container image reference.
    ///
    /// The schema accepts a lowercase repository path with an optional registry,
    /// tag, and content digest. Registry hostnames are case-insensitive and may
    /// include a valid TCP port. Repository paths follow Docker naming rules,
    /// tags use Docker's 128-character format, and digests use a lowercase
    /// algorithm name with an encoded value of at least 32 characters.
    /// URLs, credentials, IPv6 registry authorities, whitespace, and query or
    /// fragment suffixes are rejected.
    #[schema(
        pattern = r"^(?=.{1,512}$)(?=.{1,255}(?::[A-Za-z0-9_][A-Za-z0-9_.-]{0,127})?(?:@[a-z][a-z0-9]*(?:[_+.-][a-z][a-z0-9]*)*:[A-Za-z0-9=_-]{32,})?$)(?:(?:(?:[Ll][Oo][Cc][Aa][Ll][Hh][Oo][Ss][Tt]|[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)+)(?::0*(?:[1-9][0-9]{0,3}|[1-5][0-9]{4}|6[0-4][0-9]{3}|65[0-4][0-9]{2}|655[0-2][0-9]|6553[0-5]))?|[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?:0*(?:[1-9][0-9]{0,3}|[1-5][0-9]{4}|6[0-4][0-9]{3}|65[0-4][0-9]{2}|655[0-2][0-9]|6553[0-5]))/)?[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*(?:/[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*)*(?::[A-Za-z0-9_][A-Za-z0-9_.-]{0,127})?(?:@[a-z][a-z0-9]*(?:[_+.-][a-z][a-z0-9]*)*:[A-Za-z0-9=_-]{32,})?$"
    )]
    ImageReference,
    ImageReferenceError,
    "image reference must use a supported repository, optional tag, and optional SHA-256 digest",
    crate::manifest::valid_image_reference
);
validated_string!(
    /// A repository reference pinned to an explicit SHA-256 digest.
    ///
    /// The schema accepts the same registry, repository, and optional tag forms
    /// as [`ImageReference`], but requires a trailing SHA-256 digest containing
    /// exactly 64 lowercase hexadecimal digits.
    #[schema(
        pattern = r"^(?=.{1,512}$)(?=.{1,255}(?::[A-Za-z0-9_][A-Za-z0-9_.-]{0,127})?@sha256:[0-9a-f]{64}$)(?:(?:(?:[Ll][Oo][Cc][Aa][Ll][Hh][Oo][Ss][Tt]|[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)+)(?::0*(?:[1-9][0-9]{0,3}|[1-5][0-9]{4}|6[0-4][0-9]{3}|65[0-4][0-9]{2}|655[0-2][0-9]|6553[0-5]))?|[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?:0*(?:[1-9][0-9]{0,3}|[1-5][0-9]{4}|6[0-4][0-9]{3}|65[0-4][0-9]{2}|655[0-2][0-9]|6553[0-5]))/)?[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*(?:/[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*)*(?::[A-Za-z0-9_][A-Za-z0-9_.-]{0,127})?@sha256:[0-9a-f]{64}$"
    )]
    RepositoryDigest,
    RepositoryDigestError,
    "repository digest must use repository@sha256:<64 lowercase hexadecimal digits>",
    crate::resource::immutable_digest_reference
);
validated_string!(
    /// An immutable runtime image: a repository digest or a local SHA-256 image ID.
    ///
    /// ```compile_fail
    /// use piqueld_core::{ImageReference, ImmutableImage};
    /// let runtime: ImmutableImage = ImageReference::parse("alpine:latest").unwrap();
    /// ```
    ///
    /// The schema accepts either a [`RepositoryDigest`] or a local Docker image
    /// ID written as `sha256:` followed by exactly 64 lowercase hexadecimal
    /// digits. Mutable tags and unqualified repository names are rejected.
    #[schema(
        pattern = r"^(?:sha256:[0-9a-f]{64}|(?=.{1,512}$)(?=.{1,255}(?::[A-Za-z0-9_][A-Za-z0-9_.-]{0,127})?@sha256:[0-9a-f]{64}$)(?:(?:(?:[Ll][Oo][Cc][Aa][Ll][Hh][Oo][Ss][Tt]|[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)+)(?::0*(?:[1-9][0-9]{0,3}|[1-5][0-9]{4}|6[0-4][0-9]{3}|65[0-4][0-9]{2}|655[0-2][0-9]|6553[0-5]))?|[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?:0*(?:[1-9][0-9]{0,3}|[1-5][0-9]{4}|6[0-4][0-9]{3}|65[0-4][0-9]{2}|655[0-2][0-9]|6553[0-5]))/)?[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*(?:/[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*)*(?::[A-Za-z0-9_][A-Za-z0-9_.-]{0,127})?@sha256:[0-9a-f]{64})$"
    )]
    ImmutableImage,
    ImmutableImageError,
    "runtime image must be a repository digest or local SHA-256 image ID",
    |value: &str| crate::resource::immutable_digest_reference(value)
        || Sha256Digest::parse(value).is_ok()
);

impl ImageReference {
    /// The reference's repository, as written, pinned to `digest`.
    ///
    /// ```text
    /// "alpine:3.20", sha256:abc…              -> "alpine@sha256:abc…"
    /// "ghcr.io/team/app@sha256:abc…", sha256:abc… -> "ghcr.io/team/app@sha256:abc…"
    /// ```
    #[must_use]
    pub fn pinned(&self, digest: &Sha256Digest) -> RepositoryDigest {
        // A valid reference's repository is at most 255 bytes, so with a
        // SHA-256 digest it is always a valid repository digest.
        RepositoryDigest(format!("{}@{digest}", image_name(self.as_str())))
    }
}

impl From<RepositoryDigest> for ImmutableImage {
    fn from(value: RepositoryDigest) -> Self {
        Self(value.into())
    }
}
impl From<Sha256Digest> for ImmutableImage {
    fn from(value: Sha256Digest) -> Self {
        Self(value.into())
    }
}

/// The image a Git source was built into.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum BuiltImage {
    /// An image only the local engine has, under Docker's content-addressed
    /// image ID. Rebuilding it may produce another one.
    Local(Sha256Digest),
    /// An image a build machine pushed to its registry, which Docker pulls
    /// it from, again if it goes missing.
    Pushed(RepositoryDigest),
}

impl BuiltImage {
    /// Parses a local image ID or a repository digest.
    ///
    /// # Errors
    /// Rejects anything else, such as a tag.
    pub fn parse(value: impl Into<String>) -> Result<Self, ImmutableImageError> {
        let value = value.into();
        match Sha256Digest::parse(value.as_str()) {
            Ok(id) => Ok(Self::Local(id)),
            Err(_) => RepositoryDigest::parse(value)
                .map(Self::Pushed)
                .map_err(|_| ImmutableImageError),
        }
    }

    /// The image ID or repository digest, as Docker takes it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Local(id) => id.as_str(),
            Self::Pushed(digest) => digest.as_str(),
        }
    }
}

impl std::fmt::Display for BuiltImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<Sha256Digest> for BuiltImage {
    fn from(id: Sha256Digest) -> Self {
        Self::Local(id)
    }
}

impl From<RepositoryDigest> for BuiltImage {
    fn from(digest: RepositoryDigest) -> Self {
        Self::Pushed(digest)
    }
}

impl From<BuiltImage> for ImmutableImage {
    fn from(image: BuiltImage) -> Self {
        match image {
            BuiltImage::Local(id) => id.into(),
            BuiltImage::Pushed(digest) => digest.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutable_references_cannot_be_decoded_as_runtime_images() {
        for value in ["alpine:latest", "ghcr.io/example/app:v1"] {
            assert!(ImageReference::parse(value).is_ok());
            let wire = serde_json::to_string(value).unwrap();
            assert!(serde_json::from_str::<RepositoryDigest>(&wire).is_err());
            assert!(serde_json::from_str::<ImmutableImage>(&wire).is_err());
        }
        let digest = format!("sha256:{}", "a".repeat(64));
        let repository = format!("alpine@{digest}");
        assert!(RepositoryDigest::parse(&repository).is_ok());
        assert!(ImmutableImage::parse(&repository).is_ok());
        // A built image is stored as either, and reads back as what it was.
        for (value, pushed) in [(&digest, false), (&repository, true)] {
            let image: BuiltImage = serde_json::from_value(value.as_str().into()).unwrap();
            assert_eq!(matches!(image, BuiltImage::Pushed(_)), pushed);
            assert_eq!(image, BuiltImage::parse(value.as_str()).unwrap());
            assert_eq!(serde_json::to_value(&image).unwrap(), value.as_str());
        }
        assert!(BuiltImage::parse("alpine:latest").is_err());
        assert!(ImmutableImage::parse(&digest).is_ok());
        assert!(RepositoryDigest::parse(&digest).is_err());
        assert!(ImmutableImage::parse(format!("alpine@sha256:{}", "A".repeat(64))).is_err());
    }
}
