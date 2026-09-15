//! Checked requested and immutable image references at the runtime boundary.

use crate::{names::validated_string, resource::Sha256Digest};

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
    ImageReference, ImageReferenceError,
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
    RepositoryDigest, RepositoryDigestError,
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
    ImmutableImage, ImmutableImageError,
    "runtime image must be a repository digest or local SHA-256 image ID",
    |value: &str| crate::resource::immutable_digest_reference(value) || Sha256Digest::parse(value).is_ok()
);

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
        assert!(ImmutableImage::parse(&digest).is_ok());
        assert!(RepositoryDigest::parse(&digest).is_err());
        assert!(ImmutableImage::parse(format!("alpine@sha256:{}", "A".repeat(64))).is_err());
    }
}
