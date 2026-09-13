//! Checked requested and immutable image references at the runtime boundary.

use crate::{names::validated_string, resource::Sha256Digest};

validated_string!(
    /// A syntactically valid requested container image reference.
    ImageReference, ImageReferenceError,
    "image reference must use a supported repository, optional tag, and optional SHA-256 digest",
    crate::manifest::valid_image_reference
);
validated_string!(
    /// A repository reference pinned to an explicit SHA-256 digest.
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
