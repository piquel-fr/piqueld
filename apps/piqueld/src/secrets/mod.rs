//! Authenticated encryption for application secrets. Values never enter API metadata.
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};
use zeroize::{Zeroize, Zeroizing};

pub(crate) struct SecretCipher(XChaCha20Poly1305);
pub(crate) struct Envelope {
    pub(crate) nonce: Vec<u8>,
    pub(crate) ciphertext: Vec<u8>,
}
impl SecretCipher {
    /// Called under the store writer lock; encrypted data forbids key regeneration.
    pub(crate) fn load(path: &Path, encrypted_data_exists: bool) -> anyhow::Result<Self> {
        use anyhow::{Context, bail};
        if !path.try_exists().context("inspect secret master key")? {
            if encrypted_data_exists {
                bail!("secret master key is missing; restore secrets.key from backup");
            }
            Self::create(path)?;
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
            bail!("secret master key must be a private file owned by the daemon user");
        }
        let mut bytes = Zeroizing::new(Vec::with_capacity(33));
        file.take(33)
            .read_to_end(&mut bytes)
            .context("read secret master key")?;
        if bytes.len() != 32 {
            bail!("secret master key must contain exactly 32 bytes; restore the original key");
        }
        Ok(Self(XChaCha20Poly1305::new_from_slice(&bytes).map_err(
            |_| anyhow::anyhow!("invalid secret key length"),
        )?))
    }

    /// Durably creates a fresh private key, without replacing any existing file.
    pub(crate) fn create(path: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        let parent = Self::directory(path);
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).context("create private secret key file")?;
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
        Self::sync_directory(path)
    }

    fn directory(path: &Path) -> &Path {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    }

    pub(crate) fn replacement_path(path: &Path, id: uuid::Uuid) -> std::path::PathBuf {
        path.with_file_name(format!("secrets.key.{}.pending", id.simple()))
    }

    /// Rename only after the staged key has authenticated the committed verifier.
    pub(crate) fn install(staged: &Path, path: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        std::fs::rename(staged, path).context("install replacement secret key")?;
        Self::sync_directory(path)
    }

    pub(crate) fn sync_directory(path: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        File::open(Self::directory(path))?
            .sync_all()
            .context("sync secret key directory")
    }
    /// After the journal is cleared, any staged files are unused keys left by
    /// aborted attempts. Never remove the installed key or unrelated files.
    pub(crate) fn cleanup_staged_keys(path: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        let parent = Self::directory(path);
        let mut removed = false;
        for entry in std::fs::read_dir(parent).context("list staged secret keys")? {
            let entry = entry.context("inspect staged secret key")?;
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|n| n.strip_prefix("secrets.key."))
                .and_then(|n| n.strip_suffix(".pending"))
            else {
                continue;
            };
            if uuid::Uuid::parse_str(id).is_ok() {
                std::fs::remove_file(entry.path()).context("remove unused staged secret key")?;
                removed = true;
            }
        }
        if removed {
            Self::sync_directory(path)?;
        }
        Ok(())
    }

    fn context(application: &str, name: &str, generation: i64) -> Vec<u8> {
        format!("piqueld-secret-v1\0{application}\0{name}\0{generation}").into_bytes()
    }
    pub(crate) fn encrypt(
        &self,
        application: &str,
        name: &str,
        generation: i64,
        plaintext: &[u8],
    ) -> anyhow::Result<Envelope> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let aad = Self::context(application, name, generation);
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
    pub(crate) fn decrypt(
        &self,
        application: &str,
        name: &str,
        generation: i64,
        envelope: &Envelope,
    ) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        if envelope.nonce.len() != 24 {
            anyhow::bail!("invalid encrypted secret nonce");
        }
        let aad = Self::context(application, name, generation);
        let plaintext=self.0.decrypt(XNonce::from_slice(&envelope.nonce),Payload{msg:&envelope.ciphertext,aad:&aad}).map_err(|_|anyhow::anyhow!("secret authentication failed; verify the original master key and database backup"))?;
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
        let mut encrypted = cipher
            .encrypt("app-a", "token", 1, b"sensitive value")
            .unwrap();
        assert_ne!(encrypted.ciphertext, b"sensitive value");
        assert_eq!(
            cipher
                .decrypt("app-a", "token", 1, &encrypted)
                .unwrap()
                .as_slice(),
            b"sensitive value"
        );
        for (app, name, generation) in [
            ("app-b", "token", 1),
            ("app-a", "other", 1),
            ("app-a", "token", 2),
        ] {
            assert!(cipher.decrypt(app, name, generation, &encrypted).is_err());
        }
        encrypted.ciphertext[0] ^= 1;
        assert!(cipher.decrypt("app-a", "token", 1, &encrypted).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(SecretCipher::load(&path, true).is_err());
        assert!(
            !path.exists(),
            "missing original keys must never be regenerated"
        );
    }
}
