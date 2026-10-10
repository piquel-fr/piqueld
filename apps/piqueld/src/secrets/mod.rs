//! Authenticated encryption for application secrets. Values never enter API metadata.
mod generate;
pub(crate) use generate::Generate;

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
};
use piqueld_core::{ApplicationId, EnvironmentId};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};
use zeroize::{Zeroize, Zeroizing};

/// Master-key failures whose kind is safe to expose as a diagnostic fact.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub(crate) enum KeyFailure {
    /// Encrypted values exist but the key file does not.
    #[error("secret master key is missing; restore secrets.key from backup")]
    Missing,
    /// The key file is not a private regular file owned by the daemon user.
    #[error("secret master key must be a private file owned by the daemon user")]
    NotPrivate,
    /// The key file does not contain a 256-bit key.
    #[error("secret master key must contain exactly 32 bytes; restore the original key")]
    InvalidLength,
    /// The key does not authenticate stored ciphertext.
    #[error("secret authentication failed; verify the original master key and database backup")]
    AuthenticationFailed,
}

impl KeyFailure {
    /// Sanitized causal fact recorded in diagnostics.
    pub(crate) const fn fact(self) -> &'static str {
        match self {
            Self::Missing => "Secret master key: missing",
            Self::NotPrivate => "Secret master key: not a private file owned by the daemon user",
            Self::InvalidLength => "Secret master key: invalid length",
            Self::AuthenticationFailed => "Secret master key: does not authenticate stored values",
        }
    }
}

/// What a ciphertext belongs to. Its encryption context binds the owner, so a
/// value cannot be moved to another owner. A `production` environment shares
/// its application's ID, so the two stores use distinct contexts.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SecretOwner<'a> {
    /// An environment's generated secret.
    Environment(&'a EnvironmentId),
    /// A secret in an application's store.
    Application(&'a ApplicationId),
    /// The verifier proving the master key matches the database.
    KeyVerifier,
    /// The secret GitHub signs an application's push webhooks with.
    Webhook(&'a ApplicationId),
}

impl<'a> SecretOwner<'a> {
    /// Context version and owner ID. Environment and verifier contexts
    /// predate application stores and are unchanged.
    fn context(self) -> (&'static str, &'a str) {
        match self {
            Self::Environment(id) => ("piqueld-secret-v1", id.as_str()),
            Self::Application(id) => ("piqueld-application-secret-v1", id.as_str()),
            Self::KeyVerifier => ("piqueld-secret-v1", "piqueld"),
            Self::Webhook(id) => ("piqueld-webhook-secret-v1", id.as_str()),
        }
    }
}

/// `XChaCha20-Poly1305` cipher keyed by the on-disk master key (`secrets.key`).
pub(crate) struct SecretCipher(XChaCha20Poly1305);
/// A stored secret value: a random 24-byte nonce and its authenticated ciphertext.
pub(crate) struct Envelope {
    pub(crate) nonce: Vec<u8>,
    pub(crate) ciphertext: Vec<u8>,
}
impl SecretCipher {
    /// Loads the master key, first generating one when the file is absent and the
    /// database has no encrypted values yet (`bound` is false).
    ///
    /// A new key is written to a temporary file and installed without overwriting,
    /// then the directory is synced. The key is opened without following symlinks
    /// and must be a regular file owned by the daemon user with no group or other
    /// access, holding exactly 32 bytes.
    ///
    /// Called under the store writer lock; a key bound to the database is never regenerated.
    pub(crate) fn load(path: &Path, bound: bool) -> anyhow::Result<Self> {
        use anyhow::{Context, bail};
        if !path.try_exists().context("inspect secret master key")? {
            if bound {
                bail!(KeyFailure::Missing);
            }
            let parent = path.parent().context("locate secret key directory")?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)
                .context("create private secret key file")?;
            let mut key = XChaCha20Poly1305::generate_key(&mut OsRng);
            let result = temporary.write_all(key.as_slice());
            key.zeroize();
            result.context("write secret master key")?;
            temporary
                .as_file()
                .sync_all()
                .context("sync secret master key")?;
            temporary
                .persist_noclobber(path)
                .map_err(|e| e.error)
                .context("install secret master key")?;
            File::open(parent)?
                .sync_all()
                .context("sync secret key directory")?;
        }
        let descriptor = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .context("open secret master key without following symlinks")?;
        let file = File::from(descriptor);
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            bail!(KeyFailure::NotPrivate);
        }
        let mut bytes = Zeroizing::new(Vec::with_capacity(33));
        file.take(33)
            .read_to_end(&mut bytes)
            .context("read secret master key")?;
        if bytes.len() != 32 {
            bail!(KeyFailure::InvalidLength);
        }
        Ok(Self(XChaCha20Poly1305::new_from_slice(&bytes).map_err(
            |_| anyhow::anyhow!("invalid secret key length"),
        )?))
    }
    /// Moves an unusable key aside after its values were discarded, so the next value
    /// write generates a fresh key. The file is kept rather than deleted: it may belong
    /// to a different database backup. A missing key needs nothing.
    pub(crate) fn retire(path: &Path, now_ms: i64) -> anyhow::Result<()> {
        use anyhow::Context;
        let mut retired = path.as_os_str().to_owned();
        retired.push(format!(".retired-{now_ms}"));
        match std::fs::rename(path, &retired) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            result => result.context("move unusable secret master key aside")?,
        }
        File::open(path.parent().context("locate secret key directory")?)?
            .sync_all()
            .context("sync secret key directory")
    }
    /// Associated data binding a ciphertext to its owner, secret name, and
    /// generation, so a value cannot be moved to another secret or version.
    ///
    /// ```text
    /// piqueld-secret-v1\0<environment>\0<name>\0<generation>
    /// piqueld-application-secret-v1\0<application>\0<name>\0<generation>
    /// ```
    fn context(owner: SecretOwner<'_>, name: &str, generation: i64) -> Vec<u8> {
        let (version, owner) = owner.context();
        format!("{version}\0{owner}\0{name}\0{generation}").into_bytes()
    }
    /// Encrypts one secret value under a fresh random nonce.
    pub(crate) fn encrypt(
        &self,
        owner: SecretOwner<'_>,
        name: &str,
        generation: i64,
        plaintext: &[u8],
    ) -> anyhow::Result<Envelope> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let aad = Self::context(owner, name, generation);
        let ciphertext = self
            .0
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("secret encryption failed"))?;
        Ok(Envelope {
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }
    /// Decrypts a value, returning [`KeyFailure::AuthenticationFailed`] when the key
    /// or identity does not match. The plaintext is zeroized on drop.
    pub(crate) fn decrypt(
        &self,
        owner: SecretOwner<'_>,
        name: &str,
        generation: i64,
        envelope: &Envelope,
    ) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        if envelope.nonce.len() != 24 {
            anyhow::bail!("invalid encrypted secret nonce");
        }
        let aad = Self::context(owner, name, generation);
        let plaintext = self
            .0
            .decrypt(
                XNonce::from_slice(&envelope.nonce),
                Payload {
                    msg: &envelope.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| KeyFailure::AuthenticationFailed)?;
        Ok(Zeroizing::new(plaintext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_and_ciphertext_are_bound_to_their_original_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secrets.key");
        let cipher = SecretCipher::load(&path, false).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let (environment, other) = (
            EnvironmentId::parse("app-aaaaaaaa").unwrap(),
            EnvironmentId::parse("app-bbbbbbbb").unwrap(),
        );
        // A production environment shares its application's ID.
        let application = ApplicationId::parse("app-aaaaaaaa").unwrap();
        let owner = SecretOwner::Environment(&environment);
        let mut encrypted = cipher
            .encrypt(owner, "token", 1, b"sensitive value")
            .unwrap();
        assert_ne!(encrypted.ciphertext, b"sensitive value");
        assert_eq!(
            cipher
                .decrypt(owner, "token", 1, &encrypted)
                .unwrap()
                .as_slice(),
            b"sensitive value"
        );
        for (owner, name, generation) in [
            (SecretOwner::Environment(&other), "token", 1),
            (SecretOwner::Application(&application), "token", 1),
            (owner, "other", 1),
            (owner, "token", 2),
        ] {
            assert!(cipher.decrypt(owner, name, generation, &encrypted).is_err());
        }
        encrypted.ciphertext[0] ^= 1;
        assert!(cipher.decrypt(owner, "token", 1, &encrypted).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(SecretCipher::load(&path, true).is_err());
        assert!(
            !path.exists(),
            "missing original keys must never be regenerated"
        );
    }
}
