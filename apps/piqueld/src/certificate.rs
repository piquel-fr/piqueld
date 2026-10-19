//! TLS certificate inspection shared by ingress and the daemon's tailnet node.

/// The validity period of a certificate chain's leaf, in Unix milliseconds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Validity {
    /// Start of the validity period.
    pub(crate) not_before_ms: i64,
    /// End of the validity period.
    pub(crate) not_after_ms: i64,
}

/// Why a chain's leaf certificate could not be read. Callers prefix their own
/// verb, as in "invalid certificate PEM: ...".
#[derive(Debug, thiserror::Error)]
pub(crate) enum LeafError {
    /// The first PEM block could not be decoded.
    #[error("certificate PEM: {0}")]
    Pem(x509_parser::nom::Err<x509_parser::error::PEMError>),
    /// The first PEM block is not an X.509 certificate.
    #[error("certificate: {0}")]
    X509(x509_parser::nom::Err<x509_parser::error::X509Error>),
}

impl Validity {
    /// Reads the validity of the first certificate in the PEM `chain`.
    ///
    /// # Errors
    /// Returns [`LeafError`] when the first PEM block is not an X.509 certificate.
    pub(crate) fn of_leaf(chain: &[u8]) -> Result<Self, LeafError> {
        let (_, pem) = x509_parser::pem::parse_x509_pem(chain).map_err(LeafError::Pem)?;
        let leaf = pem.parse_x509().map_err(LeafError::X509)?;
        let validity = leaf.validity();
        Ok(Self {
            not_before_ms: validity.not_before.timestamp().saturating_mul(1000),
            not_after_ms: validity.not_after.timestamp().saturating_mul(1000),
        })
    }
}
