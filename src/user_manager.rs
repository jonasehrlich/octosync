//! Serializes platform user management through a message-passing actor. This keeps
//! concurrent syncs from racing shadow-utils' passwd and group file locks.
//!
//! The [`messages`] are the platform-neutral contract. Every backend in [`backends`]
//! is an actor handling the full message set, and [`UserManager`] is a
//! type-erased handle to one spawned backend.

pub mod backends;
mod messages;

pub use messages::{
    AccountIds, CreateUser, DisableAccount, EnsureGroupsExist, PurgeAccount, PurgeOutcome,
    SyncSupplementaryGroups, UpdateAuthorizedKeys, UpdateUser,
};

use crate::{public_keys, store};
use anyhow::Context as _;
use std::collections;

/// Type-erased handle to a platform user manager actor.
#[derive(Clone)]
pub struct UserManager {
    create_user: hannibal::Caller<CreateUser>,
    update_user: hannibal::Caller<UpdateUser>,
    disable_account: hannibal::Caller<DisableAccount>,
    purge_account: hannibal::Caller<PurgeAccount>,
    sync_supplementary_groups: hannibal::Caller<SyncSupplementaryGroups>,
    ensure_groups_exist: hannibal::Caller<EnsureGroupsExist>,
    update_authorized_keys: hannibal::Caller<UpdateAuthorizedKeys>,
}

const ACTOR_ERROR: &str = "User manager actor not available or failed to process the request";

#[bon::bon]
impl UserManager {
    #[builder]
    pub fn new(
        /// Preview actions without changing users, groups or files on the system
        dry_run: bool,
    ) -> Self {
        if dry_run {
            return Self::from_actor(backends::mock::MockUserManager::new(1000));
        }

        #[cfg(target_os = "linux")]
        {
            Self::from_actor(backends::linux::LinuxUserManager)
        }

        #[cfg(not(target_os = "linux"))]
        {
            Self::from_actor(backends::mock::MockUserManager::new(1000))
        }
    }
}

impl UserManager {
    fn from_actor<A>(actor: A) -> Self
    where
        A: hannibal::Handler<CreateUser>
            + hannibal::Handler<UpdateUser>
            + hannibal::Handler<DisableAccount>
            + hannibal::Handler<PurgeAccount>
            + hannibal::Handler<SyncSupplementaryGroups>
            + hannibal::Handler<EnsureGroupsExist>
            + hannibal::Handler<UpdateAuthorizedKeys>,
    {
        use hannibal::spawnable::Spawnable as _;
        let addr = actor.spawn();
        Self {
            create_user: addr.caller(),
            update_user: addr.caller(),
            disable_account: addr.caller(),
            purge_account: addr.caller(),
            sync_supplementary_groups: addr.caller(),
            ensure_groups_exist: addr.caller(),
            update_authorized_keys: addr.caller(),
        }
    }

    pub async fn create_user(
        &self,
        gh_user: &octocrab::models::Author,
        ids: AccountIds,
    ) -> anyhow::Result<store::User> {
        self.create_user
            .call(CreateUser {
                gh_user: gh_user.clone(),
                ids,
            })
            .await
            .context(ACTOR_ERROR)?
    }

    pub async fn update_user(
        &self,
        gh_user: &octocrab::models::Author,
        available_user: &store::User,
    ) -> anyhow::Result<store::User> {
        self.update_user
            .call(UpdateUser {
                gh_user: gh_user.clone(),
                available_user: available_user.clone(),
            })
            .await
            .context(ACTOR_ERROR)?
    }

    pub async fn disable_user(&self, user: &store::User) -> anyhow::Result<()> {
        self.disable_account
            .call(DisableAccount { user: user.clone() })
            .await
            .context(ACTOR_ERROR)?
    }

    pub async fn purge_user(
        &self,
        user: &store::User,
        disabled_before: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<PurgeOutcome> {
        self.purge_account
            .call(PurgeAccount {
                user: user.clone(),
                disabled_before,
            })
            .await
            .context(ACTOR_ERROR)?
    }

    pub async fn sync_supplementary_groups(
        &self,
        user: &store::User,
        groups: &[String],
    ) -> anyhow::Result<()> {
        self.sync_supplementary_groups
            .call(SyncSupplementaryGroups {
                user: user.clone(),
                groups: groups.to_vec(),
            })
            .await
            .context(ACTOR_ERROR)?
    }

    pub async fn ensure_groups_exist(&self, groups: &[String]) -> anyhow::Result<()> {
        self.ensure_groups_exist
            .call(EnsureGroupsExist {
                groups: groups.to_vec(),
            })
            .await
            .context(ACTOR_ERROR)?
    }

    pub async fn update_authorized_keys(
        &self,
        user: &store::User,
        keys: &public_keys::PublicKeys,
    ) -> anyhow::Result<()> {
        self.update_authorized_keys
            .call(UpdateAuthorizedKeys {
                user: user.clone(),
                keys: keys.clone(),
            })
            .await
            .context(ACTOR_ERROR)?
    }
}

#[cfg(test)]
impl UserManager {
    /// Build a manager backed by a [`backends::testing::TestingUserManager`], so
    /// tests can mock the response of every operation.
    pub(crate) fn testing(actor: backends::testing::TestingUserManager) -> Self {
        Self::from_actor(actor)
    }
}

/// Compute the supplementary groups to set for a user, or `None` when the current
/// memberships already match and no update is needed.
///
/// octosync owns the supplementary groups of synced users: the result is exactly
/// `groups` minus the user's primary group, regardless of the current memberships.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn supplementary_groups_update(
    groups: &[String],
    primary_group: &str,
    current: &collections::BTreeSet<String>,
) -> Option<Vec<String>> {
    let desired: collections::BTreeSet<String> = groups
        .iter()
        .filter(|group| group.as_str() != primary_group)
        .cloned()
        .collect();
    if desired == *current {
        return None;
    }
    Some(desired.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_user(id: u64, name: &str) -> store::User {
        store::User::builder()
            .id(octocrab::models::UserId(id))
            .name(name.to_string())
            .uid(nix::unistd::Uid::from_raw(1000 + id as u32))
            .build()
    }

    mod scripted_manager {
        use super::*;

        #[tokio::test]
        async fn responses_are_returned_in_order() {
            let mut actor = backends::testing::TestingUserManager::default();
            actor.disable_account.push_back(Ok(()));
            actor
                .disable_account
                .push_back(Err(anyhow::anyhow!("scripted failure")));
            let manager = UserManager::testing(actor);

            let user = test_user(1, "alice");
            manager.disable_user(&user).await.unwrap();
            let err = manager.disable_user(&user).await.unwrap_err();
            assert!(err.to_string().contains("scripted failure"));
        }

        #[tokio::test]
        async fn exhausted_script_fails_with_the_message_name() {
            let manager = UserManager::testing(Default::default());

            let err = manager
                .ensure_groups_exist(&["developers".to_string()])
                .await
                .unwrap_err();
            assert!(err.to_string().contains("EnsureGroupsExist"));
        }
    }

    mod serialization {
        use super::*;
        use std::sync::{self, atomic};

        /// Records how many of its handlers run at the same time.
        struct OverlapProbe {
            active: sync::Arc<atomic::AtomicUsize>,
            max_active: sync::Arc<atomic::AtomicUsize>,
        }

        impl OverlapProbe {
            async fn probe(&self) {
                let active = self.active.fetch_add(1, atomic::Ordering::SeqCst) + 1;
                self.max_active.fetch_max(active, atomic::Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                self.active.fetch_sub(1, atomic::Ordering::SeqCst);
            }
        }

        impl hannibal::Actor for OverlapProbe {}

        impl hannibal::Handler<DisableAccount> for OverlapProbe {
            async fn handle(
                &mut self,
                _ctx: &mut hannibal::Context<Self>,
                _msg: DisableAccount,
            ) -> anyhow::Result<()> {
                self.probe().await;
                Ok(())
            }
        }

        impl hannibal::Handler<SyncSupplementaryGroups> for OverlapProbe {
            async fn handle(
                &mut self,
                _ctx: &mut hannibal::Context<Self>,
                _msg: SyncSupplementaryGroups,
            ) -> anyhow::Result<()> {
                self.probe().await;
                Ok(())
            }
        }

        // The remaining messages are not exercised. The impls only complete the
        // handler set `UserManager::from_actor` requires.
        impl hannibal::Handler<CreateUser> for OverlapProbe {
            async fn handle(
                &mut self,
                _ctx: &mut hannibal::Context<Self>,
                _msg: CreateUser,
            ) -> anyhow::Result<store::User> {
                Err(anyhow::anyhow!("not exercised"))
            }
        }

        impl hannibal::Handler<UpdateUser> for OverlapProbe {
            async fn handle(
                &mut self,
                _ctx: &mut hannibal::Context<Self>,
                _msg: UpdateUser,
            ) -> anyhow::Result<store::User> {
                Err(anyhow::anyhow!("not exercised"))
            }
        }

        impl hannibal::Handler<PurgeAccount> for OverlapProbe {
            async fn handle(
                &mut self,
                _ctx: &mut hannibal::Context<Self>,
                _msg: PurgeAccount,
            ) -> anyhow::Result<PurgeOutcome> {
                Ok(PurgeOutcome::NoAccount)
            }
        }

        impl hannibal::Handler<EnsureGroupsExist> for OverlapProbe {
            async fn handle(
                &mut self,
                _ctx: &mut hannibal::Context<Self>,
                _msg: EnsureGroupsExist,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        impl hannibal::Handler<UpdateAuthorizedKeys> for OverlapProbe {
            async fn handle(
                &mut self,
                _ctx: &mut hannibal::Context<Self>,
                _msg: UpdateAuthorizedKeys,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        /// Regression test for the flaky "cannot lock /etc/passwd" failures: however
        /// many callers use the handle concurrently, the actor must run the platform
        /// operations one at a time.
        #[tokio::test]
        async fn concurrent_operations_never_overlap() {
            let active = sync::Arc::new(atomic::AtomicUsize::new(0));
            let max_active = sync::Arc::new(atomic::AtomicUsize::new(0));
            let manager = UserManager::from_actor(OverlapProbe {
                active: active.clone(),
                max_active: max_active.clone(),
            });

            let user = test_user(1, "alice");
            let calls = (0..8).map(|i| {
                let manager = &manager;
                let user = &user;
                async move {
                    if i % 2 == 0 {
                        manager.disable_user(user).await
                    } else {
                        manager.sync_supplementary_groups(user, &[]).await
                    }
                }
            });

            for result in futures::future::join_all(calls).await {
                result.unwrap();
            }
            assert_eq!(max_active.load(atomic::Ordering::SeqCst), 1);
        }
    }

    mod supplementary_groups_update {
        use super::*;

        fn set(groups: &[&str]) -> collections::BTreeSet<String> {
            groups.iter().map(|group| group.to_string()).collect()
        }

        fn groups(groups: &[&str]) -> Vec<String> {
            groups.iter().map(|group| group.to_string()).collect()
        }

        #[test]
        fn replaces_out_of_band_memberships() {
            let update =
                supplementary_groups_update(&groups(&["developers"]), "alice", &set(&["docker"]));
            assert_eq!(update, Some(groups(&["developers"])));
        }

        #[test]
        fn primary_group_is_excluded() {
            let update =
                supplementary_groups_update(&groups(&["alice", "developers"]), "alice", &set(&[]));
            assert_eq!(update, Some(groups(&["developers"])));
        }

        #[test]
        fn no_update_when_groups_match() {
            let update = supplementary_groups_update(
                &groups(&["developers", "ops"]),
                "alice",
                &set(&["developers", "ops"]),
            );
            assert_eq!(update, None);
        }

        #[test]
        fn empty_desired_set_clears_groups() {
            let update = supplementary_groups_update(&groups(&[]), "alice", &set(&["docker"]));
            assert_eq!(update, Some(groups(&[])));
        }

        #[test]
        fn duplicate_groups_are_deduplicated_and_sorted() {
            let update = supplementary_groups_update(
                &groups(&["ops", "developers", "ops"]),
                "alice",
                &set(&[]),
            );
            assert_eq!(update, Some(groups(&["developers", "ops"])));
        }
    }
}
