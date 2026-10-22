//! Notification destinations: where notifications go, what each receives,
//! and its URL, encrypted with the secret master key.
use crate::secrets::{Envelope, SecretCipher, SecretOwner};
use crate::store::{Actor, SecurityEvent, Store, StoreError, new_id, now_ms};
use piqueld_core::{
    ApplicationId,
    access::{GlobalPermission, Permission},
    observability::{
        ApplicationSelection, CategoryRule, DestinationKind, DestinationName, DestinationRequest,
        DestinationSettings, NotificationCategory, NotificationDestination, NotificationMessage,
    },
};
use sqlx::{Sqlite, Transaction};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

/// A destination change refused before anything is stored.
#[derive(Debug, thiserror::Error)]
pub enum DestinationError {
    /// Creating a destination needs its URL.
    #[error("a new destination needs a URL")]
    UrlRequired,
    /// See [`WebhookUrl`].
    #[error(
        "destination URLs need HTTPS, or HTTP to a loopback address, a host, and no user information or fragment"
    )]
    UrlInvalid,
    /// A selected application does not exist.
    #[error("application {0} was not found")]
    UnknownApplication(ApplicationId),
}

/// A destination URL: HTTPS, or HTTP to a loopback address, with a host and
/// no user information or fragment. It often carries a credential, so it is
/// only ever stored encrypted and never returned.
struct WebhookUrl(Zeroizing<String>);

impl WebhookUrl {
    /// Checks `url`, see [`WebhookUrl`].
    fn parse(url: String) -> Result<Self, DestinationError> {
        let url = Zeroizing::new(url);
        let parsed = reqwest::Url::parse(&url).map_err(|_| DestinationError::UrlInvalid)?;
        let loopback_http = parsed.scheme() == "http"
            && parsed.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
        if (parsed.scheme() == "https" || loopback_http)
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.fragment().is_none()
        {
            Ok(Self(url))
        } else {
            Err(DestinationError::UrlInvalid)
        }
    }
}

/// A destination as a delivery uses it: its URL still encrypted, see
/// [`Store::destination_url`].
pub(crate) struct DestinationRow {
    /// Stable identity; a new URL or kind keeps it but detaches deliveries.
    id: String,
    /// Payload format.
    pub(crate) kind: DestinationKind,
    /// The text of the delivery's category, sent with it.
    pub(crate) message: Option<String>,
    /// Encrypted URL.
    envelope: Envelope,
}

impl DestinationRow {
    /// Builds a row from stored columns.
    pub(super) fn new(
        id: String,
        kind: &str,
        message: Option<String>,
        nonce: Vec<u8>,
        ciphertext: Vec<u8>,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            id,
            kind: DestinationKind::parse(kind).ok_or(StoreError::Corrupt)?,
            message,
            envelope: Envelope { nonce, ciphertext },
        })
    }
}

/// A destination row just saved.
struct SavedDestination {
    id: String,
    created_at_ms: i64,
    updated_at_ms: i64,
    /// Whether it is a new receiver: new, or with a new URL or kind. Its
    /// earlier deliveries are detached, and nothing before is sent to it.
    replaced: bool,
    /// Whether it was enabled again: nothing that happened while it was
    /// disabled is sent to it.
    resumed: bool,
    /// How it changed, for its security event: `created`, `changed` or `disabled`.
    change: &'static str,
}

/// Which of a destination's pending deliveries to cancel.
enum Cancel<'a> {
    /// Every one.
    All,
    /// Those of one category.
    Category(&'a str),
    /// Those of events outside its `notification_audience`, but recoveries,
    /// which follow the failures it was sent.
    OutsideAudience,
}

/// The permission every destination change needs.
const MANAGE: Permission = Permission::Global(GlobalPermission::NotificationsManage);

impl Store {
    /// Lists every destination by name, without URLs, leaving out
    /// categories this version does not know.
    ///
    /// # Errors
    /// Returns storage errors.
    pub async fn notification_destinations(
        &self,
    ) -> Result<Vec<NotificationDestination>, StoreError> {
        let rows = sqlx::query!(
            "SELECT id AS \"id!\",name,kind,enabled,all_applications,previews,created_at_ms,updated_at_ms
            FROM notification_destinations ORDER BY name"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let categories = sqlx::query!(
            "SELECT destination_id,category,message FROM notification_destination_categories"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let applications = sqlx::query!(
            "SELECT destination_id,application_id FROM notification_destination_applications"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        rows.into_iter()
            .map(|row| {
                let categories = categories
                    .iter()
                    .filter(|rule| rule.destination_id == row.id)
                    // A category this version does not know, such as one a
                    // newer version added before a downgrade, is left out.
                    .filter_map(|rule| {
                        NotificationCategory::parse(&rule.category).map(|category| (category, rule))
                    })
                    .map(|(category, rule)| {
                        let message = rule
                            .message
                            .clone()
                            .map(NotificationMessage::parse)
                            .transpose()
                            .map_err(StoreError::corrupt)?;
                        Ok((category, CategoryRule { message }))
                    })
                    .collect::<Result<BTreeMap<_, _>, StoreError>>()?;
                let applications = if row.all_applications != 0 {
                    ApplicationSelection::All
                } else {
                    ApplicationSelection::Only(
                        applications
                            .iter()
                            .filter(|selected| selected.destination_id == row.id)
                            .map(|selected| {
                                ApplicationId::parse(selected.application_id.clone())
                                    .map_err(StoreError::corrupt)
                            })
                            .collect::<Result<_, _>>()?,
                    )
                };
                Ok(NotificationDestination {
                    name: DestinationName::parse(row.name).map_err(StoreError::corrupt)?,
                    settings: DestinationSettings {
                        kind: DestinationKind::parse(&row.kind).ok_or(StoreError::Corrupt)?,
                        enabled: row.enabled != 0,
                        categories,
                        applications,
                        previews: row.previews != 0,
                    },
                    created_at_ms: row.created_at_ms,
                    updated_at_ms: row.updated_at_ms,
                })
            })
            .collect()
    }

    /// Creates or replaces the destination `name`, which `actor` needs
    /// `notifications:manage` for, checked in the same transaction.
    ///
    /// A new URL or kind makes the destination a new receiver: its pending
    /// deliveries are cancelled, and its earlier deliveries detached, so no
    /// recovery of a failure sent before is sent to it. Disabling it, or
    /// removing a category, cancels the pending deliveries affected. Neither
    /// a new receiver, enabling it again, nor adding a category replays
    /// earlier events. Records a `notification_destination_changed` security
    /// event, which the destination itself only receives when it was kept.
    ///
    /// # Errors
    /// Returns refusal, [`DestinationError`], secret storage or database errors.
    pub async fn put_notification_destination(
        &self,
        actor: Actor<'_>,
        name: &DestinationName,
        request: DestinationRequest,
    ) -> Result<NotificationDestination, StoreError> {
        let url = request.url.map(WebhookUrl::parse).transpose()?;
        let settings = request.settings;
        let (_writer, mut tx) = self.begin_immediate().await?;
        actor.require_on(&mut tx, MANAGE, None).await?;
        if let ApplicationSelection::Only(applications) = &settings.applications {
            for application in applications {
                let id = application.as_str();
                sqlx::query_scalar!(
                    "SELECT 1 AS \"found!: i64\" FROM applications WHERE id=?1",
                    id
                )
                .fetch_optional(&mut *tx)
                .await
                .map_err(StoreError::database)?
                .ok_or_else(|| DestinationError::UnknownApplication(application.clone()))?;
            }
        }
        let saved = self
            .save_destination(&mut tx, name, &settings, url.as_ref())
            .await?;
        Self::settle_deliveries(&mut tx, &saved, &settings).await?;
        let message = format!("Notification destination {name} was {}", saved.change);
        Self::security_event_on(
            &mut tx,
            SecurityEvent::DestinationChanged,
            &message,
            actor.attribution(),
        )
        .await?;
        // Recorded after the event above, so a new receiver never gets it.
        Self::save_rules(&mut tx, &saved, &settings).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(NotificationDestination {
            name: name.clone(),
            settings,
            created_at_ms: saved.created_at_ms,
            updated_at_ms: saved.updated_at_ms,
        })
    }

    /// Inserts or updates the destination row of `name`, encrypting `url`
    /// unless it is unchanged; a new destination needs one.
    async fn save_destination(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        name: &DestinationName,
        settings: &DestinationSettings,
        url: Option<&WebhookUrl>,
    ) -> Result<SavedDestination, StoreError> {
        let name = name.as_str();
        let existing = sqlx::query!(
            "SELECT id AS \"id!\",kind,enabled,nonce,ciphertext,created_at_ms
            FROM notification_destinations WHERE name=?1",
            name,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let cipher = match url {
            Some(_) => Some(self.verified_secret_cipher(tx).await?),
            None => None,
        };
        let now = now_ms();
        let kind = settings.kind.as_str();
        let (enabled, previews) = (settings.enabled, settings.previews);
        let all_applications = settings.applications == ApplicationSelection::All;
        let Some(row) = existing else {
            let (Some(url), Some(cipher)) = (url, &cipher) else {
                return Err(DestinationError::UrlRequired.into());
            };
            let id = new_id("destination");
            let envelope = Self::encrypt_url(cipher, &id, url)?;
            sqlx::query!(
                "INSERT INTO notification_destinations(id,name,kind,enabled,nonce,ciphertext,all_applications,previews,created_at_ms,updated_at_ms)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)",
                id,
                name,
                kind,
                enabled,
                envelope.nonce,
                envelope.ciphertext,
                all_applications,
                previews,
                now,
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
            return Ok(SavedDestination {
                id,
                created_at_ms: now,
                updated_at_ms: now,
                replaced: true,
                resumed: false,
                change: "created",
            });
        };
        let stored = Envelope {
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        };
        let new_url = match (url, &cipher) {
            (Some(url), Some(cipher)) => {
                let owner = SecretOwner::NotificationDestination(&row.id);
                // An undecryptable URL is replaced like a changed one.
                let changed = cipher
                    .decrypt(owner, "url", 1, &stored)
                    .map_or(true, |old| old.as_slice() != url.0.as_bytes());
                changed
                    .then(|| Self::encrypt_url(cipher, &row.id, url))
                    .transpose()?
            }
            _ => None,
        };
        let replaced = new_url.is_some() || row.kind != kind;
        let envelope = new_url.unwrap_or(stored);
        sqlx::query!(
            "UPDATE notification_destinations
            SET kind=?1,enabled=?2,nonce=?3,ciphertext=?4,all_applications=?5,previews=?6,updated_at_ms=?7
            WHERE id=?8",
            kind,
            enabled,
            envelope.nonce,
            envelope.ciphertext,
            all_applications,
            previews,
            now,
            row.id,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(SavedDestination {
            id: row.id,
            created_at_ms: row.created_at_ms,
            updated_at_ms: now,
            replaced,
            resumed: row.enabled == 0 && enabled,
            change: if row.enabled != 0 && !enabled {
                "disabled"
            } else {
                "changed"
            },
        })
    }

    /// Cancels the pending deliveries `settings` no longer allow, but those
    /// outside the destination's new audience (see `save_rules`), and
    /// detaches every delivery from a replaced destination.
    async fn settle_deliveries(
        tx: &mut Transaction<'_, Sqlite>,
        saved: &SavedDestination,
        settings: &DestinationSettings,
    ) -> Result<(), StoreError> {
        let id = &saved.id;
        if saved.replaced || !settings.enabled {
            Self::cancel_deliveries(tx, id, Cancel::All, saved.updated_at_ms).await?;
        }
        let current = sqlx::query_scalar!(
            "SELECT category FROM notification_destination_categories WHERE destination_id=?1",
            id,
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        for category in current {
            let kept = NotificationCategory::parse(&category)
                .is_some_and(|category| settings.categories.contains_key(&category));
            if !kept {
                let cancel = Cancel::Category(&category);
                Self::cancel_deliveries(tx, id, cancel, saved.updated_at_ms).await?;
            }
        }
        if saved.replaced {
            sqlx::query!(
                "UPDATE notification_deliveries SET destination_id=NULL WHERE destination_id=?1",
                id,
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        Ok(())
    }

    /// Replaces a saved destination's categories and applications, then
    /// cancels its pending deliveries of events now outside its audience.
    /// Added categories, and every category of a replaced or resumed
    /// destination, start after the latest event.
    async fn save_rules(
        tx: &mut Transaction<'_, Sqlite>,
        saved: &SavedDestination,
        settings: &DestinationSettings,
    ) -> Result<(), StoreError> {
        let id = &saved.id;
        let restart = saved.replaced || saved.resumed;
        let watermark =
            sqlx::query_scalar!("SELECT COALESCE(MAX(id),0) AS \"id!: i64\" FROM events")
                .fetch_one(&mut **tx)
                .await
                .map_err(StoreError::database)?;
        let categories = serde_json::to_string(&settings.categories.keys().collect::<Vec<_>>())
            .map_err(StoreError::corrupt)?;
        sqlx::query!(
            "DELETE FROM notification_destination_categories
            WHERE destination_id=?1 AND category NOT IN (SELECT value FROM json_each(?2))",
            id,
            categories,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        for (category, rule) in &settings.categories {
            let category = category.as_str();
            let message = rule.message.as_ref().map(NotificationMessage::as_str);
            sqlx::query!(
                "INSERT INTO notification_destination_categories(destination_id,category,message,after_event_id)
                VALUES(?1,?2,?3,?4)
                ON CONFLICT(destination_id,category) DO UPDATE SET message=excluded.message,
                after_event_id=IIF(?5,excluded.after_event_id,after_event_id)",
                id,
                category,
                message,
                watermark,
                restart,
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        sqlx::query!(
            "DELETE FROM notification_destination_applications WHERE destination_id=?1",
            id,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        if let ApplicationSelection::Only(applications) = &settings.applications {
            for application in applications {
                let application = application.as_str();
                sqlx::query!(
                    "INSERT INTO notification_destination_applications(destination_id,application_id) VALUES(?1,?2)",
                    id,
                    application,
                )
                .execute(&mut **tx)
                .await
                .map_err(StoreError::database)?;
            }
        }
        Self::cancel_deliveries(tx, id, Cancel::OutsideAudience, saved.updated_at_ms).await
    }

    /// Deletes the destination `name`, cancelling its pending deliveries;
    /// delivery history keeps its name. `actor` needs `notifications:manage`,
    /// checked in the same transaction. Records a
    /// `notification_destination_changed` security event.
    ///
    /// # Errors
    /// Returns refusal, absence or database errors.
    pub async fn delete_notification_destination(
        &self,
        actor: Actor<'_>,
        name: &DestinationName,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        actor.require_on(&mut tx, MANAGE, None).await?;
        let name_str = name.as_str();
        let id = sqlx::query_scalar!(
            "SELECT id AS \"id!\" FROM notification_destinations WHERE name=?1",
            name_str,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        Self::cancel_deliveries(&mut tx, &id, Cancel::All, now_ms()).await?;
        sqlx::query!("DELETE FROM notification_destinations WHERE id=?1", id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        let message = format!("Notification destination {name} was deleted");
        Self::security_event_on(
            &mut tx,
            SecurityEvent::DestinationChanged,
            &message,
            actor.attribution(),
        )
        .await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// The destination `name`, to send a test notification to, without a
    /// category message.
    ///
    /// # Errors
    /// Returns absence or database errors.
    pub(crate) async fn notification_destination(
        &self,
        name: &DestinationName,
    ) -> Result<DestinationRow, StoreError> {
        let name = name.as_str();
        let row = sqlx::query!(
            "SELECT id AS \"id!\",kind,nonce,ciphertext FROM notification_destinations WHERE name=?1",
            name,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        DestinationRow::new(row.id, &row.kind, None, row.nonce, row.ciphertext)
    }

    /// Decrypts `destination`'s URL.
    ///
    /// # Errors
    /// Returns secret storage errors, such as a lost master key.
    pub(crate) fn destination_url(
        &self,
        destination: &DestinationRow,
    ) -> Result<Zeroizing<String>, StoreError> {
        let url = SecretCipher::load(&self.secret_key_path, true)
            .and_then(|cipher| {
                cipher.decrypt(
                    SecretOwner::NotificationDestination(&destination.id),
                    "url",
                    1,
                    &destination.envelope,
                )
            })
            .map_err(StoreError::SecretSource)?;
        String::from_utf8(url.to_vec())
            .map(Zeroizing::new)
            .map_err(StoreError::corrupt)
    }

    /// Encrypts `url` for the destination `id`.
    fn encrypt_url(
        cipher: &SecretCipher,
        id: &str,
        url: &WebhookUrl,
    ) -> Result<Envelope, StoreError> {
        cipher
            .encrypt(
                SecretOwner::NotificationDestination(id),
                "url",
                1,
                url.0.as_bytes(),
            )
            .map_err(StoreError::SecretSource)
    }

    /// Cancels the destination `id`'s pending deliveries that `cancel` selects.
    async fn cancel_deliveries(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        cancel: Cancel<'_>,
        now: i64,
    ) -> Result<(), StoreError> {
        let (category, outside) = match cancel {
            Cancel::All => (None, false),
            Cancel::Category(category) => (Some(category), false),
            Cancel::OutsideAudience => (None, true),
        };
        sqlx::query!(
            "UPDATE notification_deliveries
            SET state='cancelled',last_error=?5,updated_at_ms=?1
            WHERE destination_id=?2 AND state='pending' AND (?3 IS NULL OR category=?3)
            AND (NOT ?4 OR (category!='recovery' AND NOT EXISTS (SELECT 1 FROM notification_audience au
                WHERE au.destination_id=?2 AND au.event_id=notification_deliveries.event_id)))",
            now,
            id,
            category,
            outside,
            super::DESTINATION_CHANGED,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_require_tls_except_on_loopback() {
        for (url, accepted) in [
            ("https://hooks.example.com/secret", true),
            ("http://localhost:8080/secret", true),
            ("http://127.0.0.1:8080/secret", true),
            ("http://127.0.0.2/secret", true),
            ("http://[::1]:8080/secret", true),
            ("http://hooks.example.com/secret", false),
            ("http://192.168.1.2/secret", false),
            ("http://[2001:db8::1]/secret", false),
            ("http://localhost.example.com/secret", false),
            ("https://user:password@hooks.example.com/secret", false),
            ("https://hooks.example.com/secret#fragment", false),
            ("file:///secret", false),
        ] {
            assert_eq!(WebhookUrl::parse(url.into()).is_ok(), accepted, "{url}");
        }
    }
}
