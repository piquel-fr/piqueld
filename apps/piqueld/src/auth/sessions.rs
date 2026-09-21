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
        let mut tx = self.0.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("UPDATE auth_credentials SET last_used_at=? WHERE secret_hash=? AND (expires_at IS NULL OR expires_at>?) AND (kind!='browser' OR last_used_at>?) RETURNING id,user_id,kind")
            .bind(Self::now()).bind(Self::hash(secret)).bind(Self::now()).bind(Self::now()-DAY).fetch_optional(&mut *tx).await?.ok_or(AuthError::Unauthorized)?;
        let user = sqlx::query("SELECT id,username,display_name FROM auth_users WHERE id=?")
            .bind(row.get::<String, _>("user_id"))
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Identity {
            user: Self::user_row(&user),
            credential_id: row.get("id"),
        })
    }
    pub(crate) async fn logout(&self, credential_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM auth_credentials WHERE id=?")
            .bind(credential_id)
            .execute(&self.0.pool)
            .await?;
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
        let mut tx = self.0.pool.begin_with("BEGIN IMMEDIATE").await?;
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
