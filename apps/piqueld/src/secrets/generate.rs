//! Values for secrets that manifests declare instead of setting manually.
use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openssl::{pkey::PKey, rsa::Rsa};
use piqueld_core::manifest::{SecretEncoding, SecretGenerator};
use zeroize::Zeroizing;

/// Produces a fresh value for a declared secret. RSA generation is CPU-bound,
/// so async callers run it on a blocking thread.
pub(crate) trait Generate {
    fn generate(&self) -> anyhow::Result<Zeroizing<Vec<u8>>>;
}

impl Generate for SecretGenerator {
    fn generate(&self) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        match *self {
            Self::Random { bytes, encoding } => {
                let mut raw = Zeroizing::new(vec![0; usize::from(bytes)]);
                getrandom::fill(&mut raw).context("read random secret bytes")?;
                Ok(match encoding {
                    SecretEncoding::Hex => {
                        const DIGITS: &[u8; 16] = b"0123456789abcdef";
                        let mut text = Zeroizing::new(Vec::with_capacity(raw.len() * 2));
                        for byte in raw.iter() {
                            text.push(DIGITS[usize::from(byte >> 4)]);
                            text.push(DIGITS[usize::from(byte & 0xf)]);
                        }
                        text
                    }
                    SecretEncoding::Base64url => {
                        Zeroizing::new(URL_SAFE_NO_PAD.encode(&*raw).into_bytes())
                    }
                })
            }
            Self::Rsa { bits } => {
                let key = Rsa::generate(u32::from(bits)).context("generate RSA key")?;
                let pem = PKey::from_rsa(key)
                    .and_then(|key| key.private_key_to_pem_pkcs8())
                    .context("encode RSA key as PKCS#8 PEM")?;
                Ok(Zeroizing::new(pem))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_values_are_fresh_and_encoded() {
        let hex = SecretGenerator::Random {
            bytes: 32,
            encoding: SecretEncoding::Hex,
        };
        let first = hex.generate().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(first, hex.generate().unwrap());

        let base64 = SecretGenerator::Random {
            bytes: 32,
            encoding: SecretEncoding::Base64url,
        };
        let value = base64.generate().unwrap();
        assert_eq!(URL_SAFE_NO_PAD.decode(&*value).unwrap().len(), 32);
    }

    #[test]
    fn rsa_keys_are_pkcs8_pem() {
        let pem = SecretGenerator::Rsa { bits: 2048 }.generate().unwrap();
        assert!(pem.starts_with(b"-----BEGIN PRIVATE KEY-----"));
        let key = PKey::private_key_from_pem(&pem).unwrap();
        assert_eq!(key.rsa().unwrap().size() * 8, 2048);
    }
}
