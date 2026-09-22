//! Durable opaque sessions and short-lived, explicitly approved device logins.
use super::{Auth, AuthError, DAY, MAX_PENDING, Result};
use piqueld_core::auth::{DeviceStart, DeviceToken, User};
use sqlx::{Row, SqliteConnection};

#[derive(Clone)]
pub(crate) struct Identity {
    pub user: User,
    pub credential_id: String,
}
pub(super) struct Device {
    user_code: String,
    expires: i64,
    pub(super) next_poll: i64,
    approved_by: Option<String>,
}
impl Auth {
    pub(super) async fn issue(
        db: &mut SqliteConnection,
        user_id: &str,
        kind: &str,
        name: &str,
        expires: Option<i64>,
    ) -> Result<String> {
        let token = Self::secret()?;
        sqlx::query("INSERT INTO auth_credentials(id,user_id,secret_hash,kind,name,created_at,last_used_at,expires_at) VALUES(?,?,?,?,?,?,?,?)")
            .bind(Self::id()).bind(user_id).bind(Self::hash(&token)).bind(kind).bind(name).bind(Self::now()).bind(Self::now()).bind(expires).execute(db).await?;
        Ok(token)
    }
    pub(crate) async fn authenticate(&self, secret: &str) -> Result<Identity> {
        if secret.len() != 43 {
            return Err(AuthError::Unauthorized);
        }
        let now = Self::now();
        let row = sqlx::query("SELECT c.id AS credential_id,c.last_used_at,u.id,u.username,u.display_name FROM auth_credentials c JOIN auth_users u ON u.id=c.user_id WHERE c.secret_hash=? AND (c.expires_at IS NULL OR c.expires_at>?) AND (c.kind!='browser' OR c.last_used_at>?)")
            .bind(Self::hash(secret)).bind(now).bind(now-DAY).fetch_optional(&self.0.store.pool).await?.ok_or(AuthError::Unauthorized)?;
        let identity = Identity {
            user: Self::user_row(&row),
            credential_id: row.get("credential_id"),
        };
        // Keep ordinary requests read-only. A refresh queues with reconciliation
        // writers and rechecks validity after waiting, so revocation still wins.
        if row.get::<i64, _>("last_used_at") <= now - 60 {
            let (_writer, mut tx) = self.0.store.begin_immediate().await?;
            let now = Self::now();
            let updated = sqlx::query("UPDATE auth_credentials SET last_used_at=MAX(last_used_at,?) WHERE id=? AND (expires_at IS NULL OR expires_at>?) AND (kind!='browser' OR last_used_at>?)")
                .bind(now).bind(&identity.credential_id).bind(now).bind(now-DAY).execute(&mut *tx).await?.rows_affected();
            if updated == 0 {
                return Err(AuthError::Unauthorized);
            }
            tx.commit().await?;
        }
        Ok(identity)
    }
    pub(crate) async fn logout(&self, credential_id: &str) -> Result<()> {
        let (_writer, mut tx) = self.0.store.begin_immediate().await?;
        sqlx::query("DELETE FROM auth_credentials WHERE id=?")
            .bind(credential_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub(crate) async fn device_start(&self) -> Result<DeviceStart> {
        let mut devices = self.0.devices.lock().await;
        devices.retain(|_, device| device.expires > Self::now());
        if devices.len() >= MAX_PENDING {
            return Err(AuthError::Busy);
        }
        let device_code = Self::secret()?;
        let user_code = loop {
            let mut bytes = [0_u8; 8];
            getrandom::fill(&mut bytes).map_err(|error| AuthError::Random(error.to_string()))?;
            let alphabet = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
            let mut code: String = bytes
                .iter()
                .map(|byte| char::from(alphabet[usize::from(*byte) % alphabet.len()]))
                .collect();
            code.insert(4, '-');
            if !devices.values().any(|device| device.user_code == code) {
                break code;
            }
        };
        devices.insert(
            Self::hash(&device_code),
            Device {
                user_code: user_code.clone(),
                expires: Self::now() + 600,
                next_poll: 0,
                approved_by: None,
            },
        );
        Ok(DeviceStart {
            device_code,
            user_code,
            verification_uri: format!("{}/dashboard/auth#device", self.origin()),
            expires_in: 600,
            interval: 5,
        })
    }
    pub(crate) async fn device_approve(&self, code: &str, identity: &Identity) -> Result<()> {
        let mut devices = self.0.devices.lock().await;
        let normalized = code.trim().to_ascii_uppercase();
        let device = devices
            .values_mut()
            .find(|device| device.user_code == normalized && device.expires > Self::now())
            .ok_or(AuthError::Invalid("device code is invalid or expired"))?;
        if device.approved_by.is_some() {
            return Err(AuthError::Invalid("device code has already been approved"));
        }
        device.approved_by = Some(identity.credential_id.clone());
        Ok(())
    }
    pub(crate) async fn device_poll(&self, code: &str) -> Result<DeviceToken> {
        let mut devices = self.0.devices.lock().await;
        let key = Self::hash(code);
        let device = devices.get_mut(&key).ok_or(AuthError::Unauthorized)?;
        if device.expires <= Self::now() {
            devices.remove(&key);
            return Err(AuthError::Unauthorized);
        }
        let now = Self::now();
        if device.next_poll > now {
            return Ok(DeviceToken {
                status: "slow_down".into(),
                token: None,
                user: None,
            });
        }
        device.next_poll = now + 5;
        let Some(approved_by) = device.approved_by.clone() else {
            return Ok(DeviceToken {
                status: "authorization_pending".into(),
                token: None,
                user: None,
            });
        };
        let (_writer, mut tx) = self.0.store.begin_immediate().await?;
        let row = sqlx::query("SELECT u.id,u.username,u.display_name FROM auth_users u JOIN auth_credentials c ON c.user_id=u.id WHERE c.id=? AND (c.expires_at IS NULL OR c.expires_at>?) AND (c.kind!='browser' OR c.last_used_at>?)")
            .bind(approved_by).bind(now).bind(now-DAY).fetch_optional(&mut *tx).await?.ok_or(AuthError::Unauthorized)?;
        let user = Self::user_row(&row);
        let token =
            Self::issue(&mut tx, &user.id, "cli", "piquelctl", Some(now + 30 * DAY)).await?;
        tx.commit().await?;
        devices.remove(&key);
        Ok(DeviceToken {
            status: "complete".into(),
            token: Some(token),
            user: Some(user),
        })
    }
}
