use super::{Auth, AuthError, DAY, Result};
use piqueld_core::auth::{CredentialView, Directory, InvitationView, Manage, Managed, PasskeyView};
use sqlx::Row;
impl Auth {
    pub(crate) async fn directory(&self) -> Result<Directory> {
        let mut tx = self.0.store.pool.begin().await?;
        let users =
            sqlx::query("SELECT id,username,display_name FROM auth_users ORDER BY username")
                .fetch_all(&mut *tx)
                .await?
                .iter()
                .map(Self::user_row)
                .collect();
        let passkeys = sqlx::query("SELECT id,user_id,name FROM auth_passkeys ORDER BY created_at")
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(|r| PasskeyView {
                id: r.get("id"),
                user_id: r.get("user_id"),
                name: r.get("name"),
            })
            .collect();
        let credentials = sqlx::query("SELECT id,user_id,kind,name,last_used_at,expires_at FROM auth_credentials WHERE (expires_at IS NULL OR expires_at>?) AND (kind!='browser' OR last_used_at>?) ORDER BY created_at")
            .bind(Self::now()).bind(Self::now()-DAY).fetch_all(&mut *tx).await?.iter().map(|r| CredentialView { id:r.get("id"),user_id:r.get("user_id"),kind:r.get("kind"),name:r.get("name"),last_used_at:r.get("last_used_at"),expires_at:r.get("expires_at") }).collect();
        let invitations = sqlx::query("SELECT id,issuer_id,expires_at FROM auth_invitations WHERE expires_at>? ORDER BY expires_at").bind(Self::now()).fetch_all(&mut *tx).await?.iter().map(|r| InvitationView { id:r.get("id"),issuer_id:r.get("issuer_id"),expires_at:r.get("expires_at") }).collect();
        tx.commit().await?;
        Ok(Directory {
            users,
            passkeys,
            credentials,
            invitations,
        })
    }
    pub(crate) async fn manage(&self, actor: &str, command: Manage) -> Result<Managed> {
        let (_writer, mut tx) = self.0.store.begin_immediate().await?;
        let mut result = Managed::default();
        match command {
            Manage::UpdateUser {
                user_id,
                username,
                display_name,
            } => {
                Self::validate_profile(&username, &display_name)?;
                sqlx::query("UPDATE auth_users SET username=?,display_name=? WHERE id=?")
                    .bind(username)
                    .bind(display_name)
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await?;
            }
            Manage::DeleteUser { user_id } => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_users")
                    .fetch_one(&mut *tx)
                    .await?;
                if count <= 1 {
                    return Err(AuthError::Invalid("the last account cannot be deleted"));
                }
                sqlx::query("DELETE FROM auth_users WHERE id=?")
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await?;
            }
            Manage::RemovePasskey { id } => {
                sqlx::query("DELETE FROM auth_passkeys WHERE id=?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            Manage::RenamePasskey { id, name } => {
                if name.is_empty() || name.len() > 200 {
                    return Err(AuthError::Invalid("passkey name must contain 1–200 bytes"));
                }
                sqlx::query("UPDATE auth_passkeys SET name=? WHERE id=?")
                    .bind(name)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            Manage::RevokeCredential { id } => {
                sqlx::query("DELETE FROM auth_credentials WHERE id=?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            Manage::RevokeAll { user_id } => {
                sqlx::query("DELETE FROM auth_credentials WHERE user_id=?")
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await?;
            }
            Manage::RevokeInvitation { id } => {
                sqlx::query("DELETE FROM auth_invitations WHERE id=?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            Manage::CreateInvitation => {
                let secret = Self::secret()?;
                sqlx::query("INSERT INTO auth_invitations(id,issuer_id,secret_hash,expires_at) VALUES(?,?,?,?)").bind(Self::id()).bind(actor).bind(Self::hash(&secret)).bind(Self::now()+DAY).execute(&mut *tx).await?;
                result.invitation_url =
                    Some(format!("{}/dashboard/auth#invite={secret}", self.origin()));
            }
            Manage::CreateToken {
                user_id,
                name,
                days,
            } => {
                if name.is_empty() || name.len() > 200 || days == Some(0) {
                    return Err(AuthError::Invalid(
                        "token requires a name and a positive lifetime (or no expiry)",
                    ));
                }
                let expires = days.map(|days| Self::now() + i64::from(days) * DAY);
                result.token = Some(Self::issue(&mut tx, &user_id, "token", &name, expires).await?);
            }
        }
        tx.commit().await?;
        Ok(result)
    }
}
