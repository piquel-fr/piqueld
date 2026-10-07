use super::*;
use crate::api::http::Authenticator as _;
use crate::store::{Invitation, Lockout, NewPasskey, PasskeyOwner, StoreError};
use piqueld_core::access::{Denied, GlobalPermission, Grants, Permission, Preset, Scope};
use piqueld_core::auth::{Manage, User};

struct Fixture {
    auth: Auth,
    dir: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("state.db"))
            .await
            .unwrap();
        Self {
            auth: Auth::new(&store, "http://localhost:7845").unwrap(),
            dir,
        }
    }
    /// Seeds an account holding `grants`, returning its ID and a credential.
    async fn account(
        &self,
        name: &str,
        kind: CredentialKind,
        expires: Option<i64>,
        grants: &Grants,
    ) -> (String, String) {
        let user = User {
            id: Auth::id(),
            username: name.into(),
            display_name: String::new(),
        };
        self.auth.0.store.seed_auth_user(&user, grants).await;
        let (token, credential) = Auth::new_credential(kind, "Test", expires, None).unwrap();
        self.auth
            .0
            .store
            .insert_credential(&user.id, &credential)
            .await
            .unwrap();
        (user.id, token)
    }

    /// Seeds an account holding `grants` behind a non-expiring CLI session
    /// with the account's full access, and returns its identity.
    async fn identity(&self, name: &str, grants: &Grants) -> Identity {
        let (_, token) = self.account(name, CredentialKind::Cli, None, grants).await;
        self.auth.authenticate(&token).await.unwrap()
    }

    /// Reads an account's current profile and grants.
    async fn reload(&self, identity: &Identity) -> piqueld_core::auth::Account {
        self.auth
            .directory(identity)
            .await
            .unwrap()
            .users
            .into_iter()
            .find(|account| account.user.id == identity.user.id)
            .unwrap()
    }

    // Management tests only need a stored key; no WebAuthn ceremony reads it.
    async fn passkey(&self, owner: &Identity) -> String {
        let id = Auth::id();
        let passkey = NewPasskey {
            id: &id,
            name: "Test key",
            credential: "{}",
        };
        let store = &self.auth.0.store;
        assert!(
            store
                .add_passkey(
                    PasskeyOwner::Existing(owner.caller(), &owner.user.id),
                    passkey,
                )
                .await
                .unwrap()
        );
        id
    }
}

/// Every grant on every application, except managing accounts.
fn developer() -> Grants {
    Preset::Developer.grants(&Scope::All)
}

/// Developer access plus managing accounts it covers.
fn team_lead() -> Grants {
    let mut grants = developer();
    grants
        .grant(
            Permission::Global(GlobalPermission::AccountsManage),
            &Scope::All,
        )
        .unwrap();
    grants
}

fn denied(result: Result<piqueld_core::auth::Managed>) -> Denied {
    match result {
        Err(AuthError::Denied(denied) | AuthError::Store(StoreError::Denied(denied))) => denied,
        other => panic!("expected a refusal: {other:?}"),
    }
}

#[tokio::test]
async fn sessions_expire_revoke_and_survive_restart_without_storing_secrets() {
    let f = Fixture::new().await;
    let (id, token) = f
        .account(
            "alice",
            CredentialKind::Browser,
            Some(now_secs() + 7 * DAY),
            &Grants::admin(),
        )
        .await;
    let identity = f.auth.authenticate(&token).await.unwrap();
    assert_eq!(identity.user.id, id);
    assert_eq!(identity.grants, Grants::admin());
    // Only the hash is stored, so the secret itself finds nothing.
    assert!(
        f.auth
            .0
            .store
            .credential_owner(&token)
            .await
            .unwrap()
            .is_none()
    );
    let store = crate::store::Store::open(f.dir.path().join("state.db"))
        .await
        .unwrap();
    let restarted = Auth::new(&store, "http://localhost:7845").unwrap();
    restarted.authenticate(&token).await.unwrap();
    f.auth.0.store.age_auth_credentials(DAY).await;
    assert!(matches!(
        f.auth.authenticate(&token).await,
        Err(AuthError::Unauthorized)
    ));
    let (_, cli) = f
        .account(
            "bob",
            CredentialKind::Cli,
            Some(now_secs() - 1),
            &Grants::admin(),
        )
        .await;
    assert!(matches!(
        f.auth.authenticate(&cli).await,
        Err(AuthError::Unauthorized)
    ));
    let (other, api) = f
        .account("carol", CredentialKind::Token, None, &developer())
        .await;
    let admin = f.identity("dave", &Grants::admin()).await;
    f.auth
        .manage(&admin, Manage::RevokeAll { user_id: other })
        .await
        .unwrap();
    assert!(matches!(
        f.auth.authenticate(&api).await,
        Err(AuthError::Unauthorized)
    ));
}

#[tokio::test]
async fn accounts_manage_themselves_and_last_admin_deletion_is_atomic() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let bob = f.identity("bob", &Grants::admin()).await;
    f.auth
        .manage(
            &bob,
            Manage::UpdateUser {
                user_id: bob.user.id.clone(),
                username: "robert".into(),
                display_name: "Bob".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(f.reload(&bob).await.user.username, "robert");
    // Passkeys are only added to one's own account, even by administrators.
    let input = |user_id: &str| piqueld_core::auth::RegistrationStart {
        invitation: None,
        user_id: Some(user_id.into()),
        username: String::new(),
        display_name: String::new(),
        passkey_name: "Authenticator".into(),
    };
    assert!(
        f.auth
            .registration_start(input(&bob.user.id), "binding", Some(&alice))
            .await
            .is_err()
    );
    assert!(
        f.auth
            .registration_start(input(&alice.user.id), "binding", Some(&alice))
            .await
            .is_ok()
    );
    let made = f
        .auth
        .manage(
            &bob,
            Manage::CreateToken {
                grants: Grants::admin(),
                name: "automation".into(),
                days: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        f.auth
            .authenticate(made.token.as_ref().unwrap())
            .await
            .unwrap()
            .user
            .id,
        bob.user.id
    );
    f.passkey(&alice).await;
    f.passkey(&bob).await;
    let (first, second) = tokio::join!(
        f.auth.manage(
            &alice,
            Manage::DeleteUser {
                user_id: bob.user.id.clone()
            }
        ),
        f.auth.manage(
            &alice,
            Manage::DeleteUser {
                user_id: alice.user.id.clone()
            }
        )
    );
    assert_ne!(first.is_ok(), second.is_ok());
    assert_eq!(f.auth.directory(&alice).await.unwrap().users.len(), 1);
    assert!(f.auth.status().await.unwrap().initialized);
}

/// Changing another account needs `accounts:manage` and every grant it holds,
/// and grants can only be handed out by someone holding them.
#[tokio::test]
async fn managers_change_only_accounts_and_grants_they_cover() {
    let f = Fixture::new().await;
    let admin = f.identity("admin", &Grants::admin()).await;
    f.passkey(&admin).await;
    let lead = f.identity("lead", &team_lead()).await;
    let dev = f.identity("dev", &developer()).await;
    let viewer = f
        .identity("viewer", &Preset::ReadOnly.grants(&Scope::All))
        .await;
    let rename = |user: &Identity| Manage::UpdateUser {
        user_id: user.user.id.clone(),
        username: format!("{}-renamed", user.user.username),
        display_name: String::new(),
    };
    assert_eq!(
        denied(f.auth.manage(&dev, rename(&viewer)).await),
        Denied::Missing(Permission::Global(GlobalPermission::AccountsManage))
    );
    assert_eq!(
        denied(f.auth.manage(&lead, rename(&admin)).await),
        Denied::Exceeds
    );
    f.auth.manage(&lead, rename(&viewer)).await.unwrap();
    f.auth
        .manage(
            &lead,
            Manage::SetGrants {
                user_id: viewer.user.id.clone(),
                grants: developer(),
            },
        )
        .await
        .unwrap();
    for grants in [Grants::admin(), team_lead()] {
        let mut more = grants;
        more.grant(
            Permission::Global(GlobalPermission::SystemOperate),
            &Scope::All,
        )
        .unwrap();
        assert_eq!(
            denied(
                f.auth
                    .manage(
                        &lead,
                        Manage::SetGrants {
                            user_id: dev.user.id.clone(),
                            grants: more.clone(),
                        },
                    )
                    .await
            ),
            Denied::Exceeds
        );
        assert_eq!(
            denied(
                f.auth
                    .manage(&lead, Manage::CreateInvitation { grants: more })
                    .await
            ),
            Denied::Exceeds
        );
    }
    // Without `accounts:manage`, the directory shows only the caller.
    let directory = f.auth.directory(&dev).await.unwrap();
    assert_eq!(directory.users.len(), 1);
    assert_eq!(directory.users[0].user.id, dev.user.id);
    assert_eq!(f.auth.directory(&lead).await.unwrap().users.len(), 4);
}

#[tokio::test]
async fn invitations_carry_grants_and_enrollment_targets_an_account() {
    let f = Fixture::new().await;
    let admin = f.identity("admin", &Grants::admin()).await;
    let bob = f.identity("bob", &developer()).await;
    let store = &f.auth.0.store;
    let link = f
        .auth
        .manage(
            &admin,
            Manage::CreateInvitation {
                grants: developer(),
            },
        )
        .await
        .unwrap()
        .invitation_url
        .unwrap();
    let secret = link.split_once("#invite=").unwrap().1;
    let carol = User {
        id: Auth::id(),
        username: "carol".into(),
        display_name: String::new(),
    };
    let (_, session) = Auth::browser_session().unwrap();
    let passkey = NewPasskey {
        id: "carol-key",
        name: "Key",
        credential: "{}",
    };
    let redeem = PasskeyOwner::Redeem {
        user: &carol,
        invitation_hash: &Auth::hash(secret),
        session,
    };
    assert!(store.add_passkey(redeem, passkey).await.unwrap());
    let directory = f.auth.directory(&admin).await.unwrap();
    let account = directory
        .users
        .iter()
        .find(|account| account.user.id == carol.id)
        .unwrap();
    assert_eq!(account.grants, developer());

    let link = f
        .auth
        .manage(
            &admin,
            Manage::CreateEnrollment {
                user_id: bob.user.id.clone(),
            },
        )
        .await
        .unwrap()
        .invitation_url
        .unwrap();
    let secret = link.split_once("#enroll=").unwrap().1;
    assert!(matches!(
        f.auth.invitation(secret).await.unwrap(),
        Some(Invitation::Enrollment(user)) if user.id == bob.user.id
    ));
    // Enrollment links add a passkey only to their own account.
    let (_, session) = Auth::browser_session().unwrap();
    let wrong = PasskeyOwner::Redeem {
        user: &carol,
        invitation_hash: &Auth::hash(secret),
        session,
    };
    let passkey = NewPasskey {
        id: "stray-key",
        name: "Key",
        credential: "{}",
    };
    assert!(!store.add_passkey(wrong, passkey).await.unwrap());
    // Only managers covering the account may create its enrollment link.
    assert_eq!(
        denied(
            f.auth
                .manage(
                    &bob,
                    Manage::CreateEnrollment {
                        user_id: admin.user.id.clone(),
                    },
                )
                .await
        ),
        Denied::Missing(Permission::Global(GlobalPermission::AccountsManage))
    );
}

/// A caller demoted or revoked after authenticating cannot act on its old
/// grants: every change re-reads the caller inside its transaction.
#[tokio::test]
async fn changes_use_the_callers_current_grants() {
    let f = Fixture::new().await;
    let root = f.identity("root", &Grants::admin()).await;
    f.passkey(&root).await;
    // Alice authenticated as an administrator, then was demoted.
    let alice = f.identity("alice", &Grants::admin()).await;
    let set = |grants: Grants| Manage::SetGrants {
        user_id: alice.user.id.clone(),
        grants,
    };
    f.auth.manage(&root, set(developer())).await.unwrap();
    assert_eq!(
        denied(f.auth.manage(&alice, set(Grants::admin())).await),
        Denied::Exceeds
    );
    assert_eq!(
        denied(
            f.auth
                .manage(
                    &alice,
                    Manage::CreateInvitation {
                        grants: developer()
                    }
                )
                .await
        ),
        Denied::Missing(Permission::Global(GlobalPermission::AccountsManage))
    );
    f.auth
        .manage(
            &root,
            Manage::RevokeAll {
                user_id: alice.user.id.clone(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        f.auth
            .manage(
                &alice,
                Manage::UpdateUser {
                    user_id: alice.user.id.clone(),
                    username: "alice".into(),
                    display_name: "Revoked".into(),
                },
            )
            .await,
        Err(AuthError::Store(StoreError::CredentialRevoked))
    ));
    // Nor can it add a passkey to regain access.
    let passkey = NewPasskey {
        id: "revoked-key",
        name: "Key",
        credential: "{}",
    };
    let owner = PasskeyOwner::Existing(alice.caller(), &alice.user.id);
    assert!(matches!(
        f.auth.0.store.add_passkey(owner, passkey).await,
        Err(StoreError::CredentialRevoked)
    ));
    // Browser sessions idle past their limit since authenticating are refused too.
    let (_, session) = f
        .account(
            "bob",
            CredentialKind::Browser,
            Some(now_secs() + DAY),
            &developer(),
        )
        .await;
    let bob = f.auth.authenticate(&session).await.unwrap();
    f.auth.0.store.age_auth_credentials(DAY).await;
    assert!(matches!(
        f.auth
            .manage(
                &bob,
                Manage::CreateToken {
                    grants: developer(),
                    name: "late".into(),
                    days: None,
                },
            )
            .await,
        Err(AuthError::Store(StoreError::CredentialRevoked))
    ));
}

/// Links act with their issuer's authority, so they are refused once either
/// side's grants put the target beyond the issuer, and only issuers able to
/// create a link may revoke it.
#[tokio::test]
async fn links_are_revalidated_when_redeemed_and_revoked() {
    let f = Fixture::new().await;
    let admin = f.identity("admin", &Grants::admin()).await;
    f.passkey(&admin).await;
    let lead = f.identity("lead", &team_lead()).await;
    let dev = f.identity("dev", &developer()).await;
    let secret = |link: Option<String>| link.unwrap().split_once('=').unwrap().1.to_owned();
    let enroll = secret(
        f.auth
            .manage(
                &lead,
                Manage::CreateEnrollment {
                    user_id: dev.user.id.clone(),
                },
            )
            .await
            .unwrap()
            .invitation_url,
    );
    let invite = secret(
        f.auth
            .manage(
                &lead,
                Manage::CreateInvitation {
                    grants: developer(),
                },
            )
            .await
            .unwrap()
            .invitation_url,
    );
    f.auth
        .manage(
            &admin,
            Manage::CreateEnrollment {
                user_id: admin.user.id.clone(),
            },
        )
        .await
        .unwrap();
    let directory = f.auth.directory(&admin).await.unwrap();
    let admin_link = directory
        .invitations
        .iter()
        .find(|link| link.user_id.as_deref() == Some(admin.user.id.as_str()))
        .unwrap()
        .id
        .clone();
    assert_eq!(
        denied(
            f.auth
                .manage(&lead, Manage::RevokeInvitation { id: admin_link })
                .await
        ),
        Denied::Exceeds
    );
    // Dev is promoted beyond the lead, then the lead loses accounts:manage.
    let set = |user: &Identity, grants: Grants| Manage::SetGrants {
        user_id: user.user.id.clone(),
        grants,
    };
    f.auth
        .manage(&admin, set(&dev, Grants::admin()))
        .await
        .unwrap();
    f.auth
        .manage(&admin, set(&lead, developer()))
        .await
        .unwrap();
    let carol = User {
        id: Auth::id(),
        username: "carol".into(),
        display_name: String::new(),
    };
    for (user, secret) in [(&dev.user, &enroll), (&carol, &invite)] {
        let (_, session) = Auth::browser_session().unwrap();
        let redeem = PasskeyOwner::Redeem {
            user,
            invitation_hash: &Auth::hash(secret),
            session,
        };
        let passkey = NewPasskey {
            id: &Auth::id(),
            name: "Key",
            credential: "{}",
        };
        assert!(!f.auth.0.store.add_passkey(redeem, passkey).await.unwrap());
    }
}

/// Tokens act with their grants within the owner's current access, carry the
/// `pqd_` prefix, and cannot create credentials of any kind or change their
/// own account.
#[tokio::test]
async fn tokens_act_within_their_grants_and_cannot_create_credentials() {
    let f = Fixture::new().await;
    let root = f.identity("root", &Grants::admin()).await;
    f.passkey(&root).await;
    let alice = f.identity("alice", &developer()).await;
    let token = |grants: Grants| Manage::CreateToken {
        grants,
        name: "ci".into(),
        days: Some(30),
    };
    let deploy = Preset::Deploy.grants(&Scope::All);
    assert_eq!(
        denied(f.auth.manage(&alice, token(Grants::admin())).await),
        Denied::Exceeds
    );
    let secret = f
        .auth
        .manage(&alice, token(deploy.clone()))
        .await
        .unwrap()
        .token
        .unwrap();
    assert!(secret.starts_with("pqd_"));
    let ci = f.auth.authenticate(&secret).await.unwrap();
    assert!(ci.scoped);
    assert_eq!(ci.grants, deploy);
    // The owner's current access limits the token.
    let read_only = Preset::ReadOnly.grants(&Scope::All);
    f.auth
        .manage(
            &root,
            Manage::SetGrants {
                user_id: alice.user.id.clone(),
                grants: read_only.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        f.auth.authenticate(&secret).await.unwrap().grants,
        read_only.intersection(&deploy)
    );
    // No credential can be created through a token, even within its grants.
    for command in [
        token(Grants::default()),
        Manage::CreateEnrollment {
            user_id: alice.user.id.clone(),
        },
        Manage::CreateInvitation {
            grants: Grants::default(),
        },
    ] {
        assert_eq!(denied(f.auth.manage(&ci, command).await), Denied::Scoped);
    }
    let passkey = NewPasskey {
        id: "token-key",
        name: "Key",
        credential: "{}",
    };
    let owner = PasskeyOwner::Existing(ci.caller(), &alice.user.id);
    assert!(matches!(
        f.auth.0.store.add_passkey(owner, passkey).await,
        Err(StoreError::Denied(Denied::Scoped))
    ));
    let start = f.auth.device_start(None, None).await.unwrap();
    assert!(matches!(
        f.auth.device_approve(&start.user_code, &ci).await,
        Err(AuthError::Denied(Denied::Scoped))
    ));
    // Nor can it change its own account, which needs no permission, but it
    // can revoke itself.
    let own = || alice.user.id.clone();
    for command in [
        Manage::UpdateUser {
            user_id: own(),
            username: "mallory".into(),
            display_name: String::new(),
        },
        Manage::SetGrants {
            user_id: own(),
            grants: Grants::default(),
        },
        Manage::RevokeAll { user_id: own() },
        Manage::DeleteUser { user_id: own() },
    ] {
        assert_eq!(denied(f.auth.manage(&ci, command).await), Denied::Scoped);
    }
    f.auth.logout(&ci).await.unwrap();
    assert!(f.auth.authenticate(&secret).await.is_err());
}

/// A token keeps no access its owner lost after it authenticated: writes
/// re-read the owner's grants, and a scoped credential without grants can do
/// nothing.
#[tokio::test]
async fn scoped_credentials_follow_their_owner_and_never_widen() {
    let f = Fixture::new().await;
    let root = f.identity("root", &Grants::admin()).await;
    f.passkey(&root).await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let bob = f.identity("bob", &developer()).await;
    let secret = f
        .auth
        .manage(
            &alice,
            Manage::CreateToken {
                grants: Grants::admin(),
                name: "ops".into(),
                days: None,
            },
        )
        .await
        .unwrap()
        .token
        .unwrap();
    // Authenticated as an administrator's token, then the owner is demoted.
    let ops = f.auth.authenticate(&secret).await.unwrap();
    f.auth
        .manage(
            &root,
            Manage::SetGrants {
                user_id: alice.user.id.clone(),
                grants: developer(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        denied(
            f.auth
                .manage(
                    &ops,
                    Manage::RevokeAll {
                        user_id: bob.user.id.clone(),
                    },
                )
                .await
        ),
        Denied::Missing(Permission::Global(GlobalPermission::AccountsManage))
    );
    // Neither a token nor a limited CLI login hands out lasting access, even
    // within its grants.
    let read_only = Preset::ReadOnly.grants(&Scope::All);
    let admin = Grants::admin();
    let (secret, credential) =
        Auth::new_credential(CredentialKind::Cli, "limited", None, Some(&admin)).unwrap();
    f.auth
        .0
        .store
        .insert_credential(&root.user.id, &credential)
        .await
        .unwrap();
    let limited = f.auth.authenticate(&secret).await.unwrap();
    let root_secret = f
        .auth
        .manage(
            &root,
            Manage::CreateToken {
                grants: Grants::admin(),
                name: "ops".into(),
                days: None,
            },
        )
        .await
        .unwrap()
        .token
        .unwrap();
    let token = f.auth.authenticate(&root_secret).await.unwrap();
    for scoped in [&limited, &token] {
        let grant = Manage::SetGrants {
            user_id: bob.user.id.clone(),
            grants: read_only.clone(),
        };
        assert_eq!(denied(f.auth.manage(scoped, grant).await), Denied::Scoped);
    }
    let empty = Grants::default();
    let (secret, credential) =
        Auth::new_credential(CredentialKind::Token, "empty", None, Some(&empty)).unwrap();
    f.auth
        .0
        .store
        .insert_credential(&alice.user.id, &credential)
        .await
        .unwrap();
    let nothing = f.auth.authenticate(&secret).await.unwrap();
    assert!(nothing.scoped);
    assert!(nothing.grants.is_empty());
}

/// `auth.max_token_days` bounds new tokens, and secrets issued before the
/// `pqd_` prefix still authenticate, including ones that happen to start
/// with it.
#[tokio::test]
async fn token_lifetimes_are_bounded_and_legacy_secrets_authenticate() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let auth = Auth::configured(&f.auth.0.store, "http://localhost:7845", Some(30)).unwrap();
    let token = |days| Manage::CreateToken {
        grants: developer(),
        name: "ci".into(),
        days,
    };
    for days in [None, Some(31)] {
        assert!(matches!(
            auth.manage(&alice, token(days)).await,
            Err(AuthError::Invalid(_))
        ));
    }
    auth.manage(&alice, token(Some(30))).await.unwrap();
    for legacy in ["l".repeat(43), format!("pqd_{}", "A".repeat(39))] {
        let credential = crate::store::NewCredential {
            id: Auth::id(),
            secret_hash: Auth::hash(&legacy),
            kind: CredentialKind::Token,
            name: "legacy",
            expires_at: None,
            grants: Some(&Grants::admin()),
        };
        f.auth
            .0
            .store
            .insert_credential(&alice.user.id, &credential)
            .await
            .unwrap();
        assert_eq!(
            f.auth.authenticate(&legacy).await.unwrap().grants,
            Grants::admin()
        );
    }
}

/// A CLI login can ask for less than the approver's access; the approver must
/// hold what it asks for, and the issued session is limited to it.
#[tokio::test]
async fn device_logins_can_request_limited_sessions() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &developer()).await;
    let start = f
        .auth
        .device_start(None, Some(Grants::admin()))
        .await
        .unwrap();
    assert!(matches!(
        f.auth.device_approve(&start.user_code, &alice).await,
        Err(AuthError::Denied(Denied::Exceeds))
    ));
    let read_only = Preset::ReadOnly.grants(&Scope::All);
    let start = f
        .auth
        .device_start(None, Some(read_only.clone()))
        .await
        .unwrap();
    let request = f.auth.device_inspect(&start.user_code).await.unwrap();
    assert_eq!(request.grants.as_ref(), Some(&read_only));
    f.auth
        .device_approve(&start.user_code, &alice)
        .await
        .unwrap();
    let result = f.auth.device_poll(&start.device_code).await.unwrap();
    let session = f
        .auth
        .authenticate(result.token.as_ref().unwrap())
        .await
        .unwrap();
    assert!(session.scoped);
    assert_eq!(session.grants, read_only);
    // Issuance re-checks the approver, who may have lost the access since.
    let root = f.identity("root", &Grants::admin()).await;
    f.passkey(&root).await;
    let start = f
        .auth
        .device_start(None, Some(read_only.clone()))
        .await
        .unwrap();
    f.auth
        .device_approve(&start.user_code, &alice)
        .await
        .unwrap();
    f.auth
        .manage(
            &root,
            Manage::SetGrants {
                user_id: alice.user.id.clone(),
                grants: Preset::Deploy.grants(&Scope::All),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        f.auth.device_poll(&start.device_code).await,
        Err(AuthError::Store(StoreError::Denied(Denied::Exceeds)))
    ));
}

#[tokio::test]
async fn deleting_an_issuer_invalidates_its_invitations_and_credentials() {
    let f = Fixture::new().await;
    let (_, token) = f
        .account("alice", CredentialKind::Token, None, &Grants::admin())
        .await;
    let alice = f.auth.authenticate(&token).await.unwrap();
    let bob = f.identity("bob", &Grants::admin()).await;
    f.passkey(&bob).await;
    let result = f
        .auth
        .manage(
            &alice,
            Manage::CreateInvitation {
                grants: developer(),
            },
        )
        .await
        .unwrap();
    let link = result.invitation_url.unwrap();
    let secret = link.split_once("#invite=").unwrap().1;
    assert!(f.auth.invitation(secret).await.unwrap().is_some());
    f.auth
        .manage(
            &bob,
            Manage::DeleteUser {
                user_id: alice.user.id.clone(),
            },
        )
        .await
        .unwrap();
    assert!(f.auth.invitation(secret).await.unwrap().is_none());
    assert!(f.auth.authenticate(&token).await.is_err());
}

#[tokio::test]
async fn changes_keep_an_admin_with_a_passkey() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let bob = f.identity("bob", &Grants::admin()).await;
    let key = f.passkey(&alice).await;
    for command in [
        Manage::RemovePasskey { id: key.clone() },
        Manage::DeleteUser {
            user_id: alice.user.id.clone(),
        },
        Manage::SetGrants {
            user_id: alice.user.id.clone(),
            grants: developer(),
        },
    ] {
        assert!(matches!(
            f.auth.manage(&bob, command).await,
            Err(AuthError::Store(StoreError::Lockout(Lockout::LastAdmin)))
        ));
        let directory = f.auth.directory(&alice).await.unwrap();
        assert_eq!(directory.users.len(), 2);
        assert_eq!(directory.passkeys.len(), 1);
        assert_eq!(directory.passkeys[0].id, key);
        assert_eq!(f.reload(&alice).await.grants, Grants::admin());
    }
    // An administrator may have no passkey, provided another one keeps theirs.
    f.auth
        .manage(
            &alice,
            Manage::DeleteUser {
                user_id: bob.user.id.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(f.auth.directory(&alice).await.unwrap().users.len(), 1);
}

#[tokio::test]
async fn concurrent_passkey_and_account_deletions_keep_one_admin() {
    for delete_account in [false, true] {
        let f = Fixture::new().await;
        let alice = f.identity("alice", &Grants::admin()).await;
        let bob = f.identity("bob", &Grants::admin()).await;
        let alice_key = f.passkey(&alice).await;
        let bob_key = f.passkey(&bob).await;
        let second = if delete_account {
            Manage::DeleteUser {
                user_id: bob.user.id.clone(),
            }
        } else {
            Manage::RemovePasskey { id: bob_key }
        };
        let (first, second) = tokio::join!(
            f.auth
                .manage(&alice, Manage::RemovePasskey { id: alice_key }),
            f.auth.manage(&alice, second),
        );
        let error = match (first, second) {
            (Ok(_), Err(error)) | (Err(error), Ok(_)) => error,
            results => panic!("exactly one deletion must succeed: {results:?}"),
        };
        assert!(matches!(
            error,
            AuthError::Store(StoreError::Lockout(Lockout::LastAdmin))
        ));
        assert_eq!(f.auth.directory(&alice).await.unwrap().passkeys.len(), 1);
    }
}

#[tokio::test]
async fn device_approval_is_explicit_single_use_and_bound_to_a_live_session() {
    let f = Fixture::new().await;
    let (_, token) = f
        .account(
            "alice",
            CredentialKind::Browser,
            Some(now_secs() + DAY),
            &Grants::admin(),
        )
        .await;
    let identity = f.auth.authenticate(&token).await.unwrap();
    let start = f.auth.device_start(None, None).await.unwrap();
    assert_eq!(
        f.auth.device_poll(&start.device_code).await.unwrap().status,
        "authorization_pending"
    );
    assert_eq!(
        f.auth.device_poll(&start.device_code).await.unwrap().status,
        "slow_down"
    );
    f.auth
        .device_approve(&start.user_code, &identity)
        .await
        .unwrap();
    assert!(
        f.auth
            .device_approve(&start.user_code, &identity)
            .await
            .is_err()
    );
    f.auth
        .0
        .devices
        .lock()
        .await
        .get_mut(&Auth::hash(&start.device_code))
        .unwrap()
        .next_poll = 0;
    let result = f.auth.device_poll(&start.device_code).await.unwrap();
    f.auth
        .authenticate(result.token.as_ref().unwrap())
        .await
        .unwrap();
    assert!(f.auth.device_poll(&start.device_code).await.is_err());
    let start = f.auth.device_start(None, None).await.unwrap();
    f.auth
        .device_approve(&start.user_code, &identity)
        .await
        .unwrap();
    f.auth.logout(&identity).await.unwrap();
    assert!(f.auth.device_poll(&start.device_code).await.is_err());
}

#[tokio::test]
async fn setup_link_is_private_stable_and_never_reopens() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new().await;
    let path = f.dir.path().join("setup-link");
    f.auth.prepare_setup(&path).await.unwrap();
    let link = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    f.auth.prepare_setup(&path).await.unwrap();
    assert_eq!(link, std::fs::read_to_string(&path).unwrap());
    assert_eq!(f.auth.setup_link().await.unwrap().url, link.trim());
    // A link from a previous public_url keeps its secret under the current one.
    let (_, secret) = link.split_once("#invite=").unwrap();
    std::fs::write(
        &path,
        format!("https://old.example/dashboard/auth#invite={secret}"),
    )
    .unwrap();
    f.auth.prepare_setup(&path).await.unwrap();
    assert_eq!(link, std::fs::read_to_string(&path).unwrap());
    f.account("alice", CredentialKind::Token, None, &Grants::admin())
        .await;
    assert!(matches!(
        f.auth.setup_link().await,
        Err(AuthError::SetupCompleted)
    ));
    f.auth.prepare_setup(&path).await.unwrap();
    assert!(!path.exists());
    f.auth.0.store.clear_auth_users().await;
    f.auth.prepare_setup(&path).await.unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn challenges_require_the_original_browser_and_expire() {
    let f = Fixture::new().await;
    let ceremony = f.auth.login_start("browser-one").await.unwrap();
    let input = piqueld_core::auth::CeremonyFinish {
        id: ceremony.id.clone(),
        credential: serde_json::json!({}),
    };
    assert!(matches!(
        f.auth.login_finish(input.clone(), "browser-two").await,
        Err(AuthError::Unauthorized)
    ));
    assert!(f.auth.0.ceremonies.lock().await.contains_key(&ceremony.id));
    assert!(
        f.auth
            .login_finish(input.clone(), "browser-one")
            .await
            .is_err()
    );
    assert!(!f.auth.0.ceremonies.lock().await.contains_key(&ceremony.id));
    assert!(matches!(
        f.auth.login_finish(input, "browser-one").await,
        Err(AuthError::Unauthorized)
    ));
    let ceremony = f.auth.login_start("browser").await.unwrap();
    f.auth
        .0
        .ceremonies
        .lock()
        .await
        .get_mut(&ceremony.id)
        .unwrap()
        .expires = 0;
    assert!(matches!(
        f.auth
            .login_finish(
                piqueld_core::auth::CeremonyFinish {
                    id: ceremony.id,
                    credential: serde_json::json!({})
                },
                "browser"
            )
            .await,
        Err(AuthError::Unauthorized)
    ));
}

#[tokio::test]
async fn middleware_authenticates_api_and_enforces_cookie_csrf() {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let (_, token) = f
        .account("alice", CredentialKind::Token, None, &Grants::admin())
        .await;
    let router = f.auth.clone().guard(
        Router::new()
            .route("/api/v1/private", get(signed_in).post(signed_in))
            .route("/health", get(|| async { "ok" })),
    );
    for (path, method, credential, origin, expected) in [
        ("/health", "GET", None, None, StatusCode::OK),
        (
            "/api/v1/private",
            "GET",
            None,
            None,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "/api/v1/private",
            "GET",
            Some(("authorization", format!("Bearer {token}"))),
            None,
            StatusCode::OK,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("cookie", format!("piqueld_session_7845={token}"))),
            None,
            StatusCode::FORBIDDEN,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("cookie", format!("piqueld_session_7845={token}"))),
            Some("https://evil.example"),
            StatusCode::FORBIDDEN,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("cookie", format!("piqueld_session_7845={token}"))),
            Some("http://localhost:7845"),
            StatusCode::OK,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("authorization", format!("Bearer {token}"))),
            None,
            StatusCode::OK,
        ),
    ] {
        let mut request = Request::builder().method(method).uri(path);
        if let Some((name, value)) = credential {
            request = request.header(name, value);
        }
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        // Rejections are marked too, so no API response is ever cached.
        let cache_control = response
            .headers()
            .get("cache-control")
            .map(|value| value.to_str().unwrap());
        let expected_cache_control = path.starts_with("/api/").then_some("no-store");
        assert_eq!(cache_control, expected_cache_control, "{method} {path}");
    }
    // Browsers send cookies on cross-site WebSocket handshakes, which use GET.
    for (origin, expected) in [
        ("https://evil.example", StatusCode::FORBIDDEN),
        ("http://localhost:7845", StatusCode::OK),
    ] {
        let request = Request::get("/api/v1/private")
            .header("cookie", format!("piqueld_session_7845={token}"))
            .header("origin", origin)
            .header("upgrade", "websocket")
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected, "{origin}");
    }
}

/// Stands in for route authorization (`access::enforce`): signed-in callers only.
async fn signed_in(identity: Option<axum::Extension<Identity>>) -> axum::http::StatusCode {
    if identity.is_some() {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::UNAUTHORIZED
    }
}

#[test]
fn webauthn_origins_and_cookie_flags_are_explicit() {
    for origin in ["https://piqueld.example", "http://localhost:7845"] {
        assert!(Auth::validate_origin(origin).is_ok());
    }
    for origin in [
        "http://piqueld.example",
        "https://127.0.0.1",
        "https://user:secret@example.com",
        "https://example.com/path",
        "https://example.com?query",
    ] {
        assert!(Auth::validate_origin(origin).is_err());
    }
}

#[tokio::test]
async fn login_start_limits_share_listeners_ignore_forwarded_ips_and_leave_sessions_usable() {
    use axum::{
        Router,
        body::Body,
        extract::ConnectInfo,
        http::{Request, StatusCode},
        routing::{get, post},
    };
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let (_, token) = f
        .account("alice", CredentialKind::Token, None, &Grants::admin())
        .await;
    let router = f.auth.clone().guard(
        Router::new()
            .route("/api/v1/auth/login/start", post(|| async { "ok" }))
            .route("/api/v1/auth/device/start", post(|| async { "ok" }))
            .route("/api/v1/private", get(signed_in)),
    );
    for attempt in 0..31 {
        let request = Request::builder()
            .method("POST")
            .uri(if attempt % 2 == 0 {
                "/api/v1/auth/login/start"
            } else {
                "/api/v1/auth/device/start"
            })
            .header("origin", f.auth.origin())
            .header("x-forwarded-for", format!("192.0.2.{attempt}"))
            .extension(ConnectInfo(
                "192.0.2.1:1234".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            if attempt == 30 {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::OK
            }
        );
        if attempt == 30 {
            assert_eq!(response.headers()["retry-after"], "60");
        }
    }
    // Another router/listener must use the same budget.
    let other = f
        .auth
        .guard(Router::new().route("/api/v1/auth/device/start", post(|| async { "ok" })));
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/device/start")
        .extension(ConnectInfo(
            "192.0.2.1:5678".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        other.oneshot(request).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let request = Request::builder()
        .uri("/api/v1/private")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router.oneshot(request).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn https_cookies_use_host_prefix_and_ignore_unprefixed_names() {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let (_, token) = f
        .account(
            "alice",
            CredentialKind::Browser,
            Some(now_secs() + DAY),
            &Grants::admin(),
        )
        .await;
    let auth = Auth::new(&f.auth.0.store, "https://piqueld.example").unwrap();
    assert_eq!(
        auth.cookie("piqueld_session", "secret", 60),
        "__Host-piqueld_session=secret; Path=/; HttpOnly; SameSite=Strict; Max-Age=60; Secure"
    );
    assert_eq!(
        f.auth.cookie("piqueld_session", "secret", 60),
        "piqueld_session_7845=secret; Path=/; HttpOnly; SameSite=Strict; Max-Age=60"
    );
    assert_eq!(
        Auth::new(&f.auth.0.store, "https://piqueld.example:8443")
            .unwrap()
            .cookie_name("piqueld_session"),
        "__Host-piqueld_session_8443"
    );
    let router = auth.guard(Router::new().route("/api/v1/private", get(signed_in)));
    for (cookie, expected) in [
        (format!("piqueld_session={token}"), StatusCode::UNAUTHORIZED),
        (format!("__Host-piqueld_session={token}"), StatusCode::OK),
    ] {
        let request = Request::builder()
            .uri("/api/v1/private")
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
}

#[tokio::test]
async fn device_inspection_reports_the_requester_until_approval() {
    let f = Fixture::new().await;
    let (_, token) = f
        .account(
            "alice",
            CredentialKind::Browser,
            Some(now_secs() + DAY),
            &Grants::admin(),
        )
        .await;
    let identity = f.auth.authenticate(&token).await.unwrap();
    let peer = "192.0.2.7".parse().unwrap();
    let start = f.auth.device_start(Some(peer), None).await.unwrap();
    assert_eq!(start.requester.as_deref(), Some("192.0.2.7"));
    let request = f
        .auth
        .device_inspect(&format!(" {} ", start.user_code.to_lowercase()))
        .await
        .unwrap();
    assert_eq!(request.user_code, start.user_code);
    assert_eq!(request.requester.as_deref(), Some("192.0.2.7"));
    assert!(request.age <= 1 && (599..=600).contains(&request.expires_in));
    // Inspection does not approve anything.
    assert_eq!(
        f.auth.device_poll(&start.device_code).await.unwrap().status,
        "authorization_pending"
    );
    assert!(f.auth.device_inspect("AAAA-AAAA").await.is_err());
    f.auth
        .device_approve(&start.user_code, &identity)
        .await
        .unwrap();
    assert!(f.auth.device_inspect(&start.user_code).await.is_err());
    let local = f.auth.device_start(None, None).await.unwrap();
    assert!(local.requester.is_none());
    assert!(
        f.auth
            .device_inspect(&local.user_code)
            .await
            .unwrap()
            .requester
            .is_none()
    );
}
