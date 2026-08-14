//! Platform user management behind a message-passing actor.
//!
//! The [`messages`] are the platform-neutral contract: every backend in [`backends`]
//! is an actor handling the full message set, and [`UserManager`] is a
//! type-erased handle to one spawned backend. The actor processes one message at a
//! time, which serializes all account mutations: shadow-utils commands (useradd,
//! usermod, userdel, groupadd) fail with "cannot lock /etc/passwd" instead of waiting
//! when another invocation holds the lock, so the concurrent per-user syncs must never
//! run them in parallel.
//!
//! Deletion is split so the disk-heavy work stays concurrent: the actor prepares the
//! deletion and removes the account, while the home directory archiving between the
//! two runs outside the actor in the caller's task.

pub mod backends;
mod messages;

pub use messages::{
    CreateUser, DeletionPreparation, EnsureGroupsExist, PrepareUserDeletion, RemoveAccount,
    SyncSupplementaryGroups, UpdateAuthorizedKeys, UpdateUser,
};

use crate::{public_keys, store};
use anyhow::Context as _;
use std::{collections, path};

/// Handle to a spawned platform user manager actor.
///
/// Cloning the handle clones the callers; all clones address the same actor, so the
/// one-message-at-a-time serialization holds across every user of the handle.
#[derive(Clone)]
pub struct UserManager {
    /// Directory where home directories of deleted users are archived
    home_archive_dir: path::PathBuf,
    create_user: hannibal::Caller<CreateUser>,
    update_user: hannibal::Caller<UpdateUser>,
    prepare_user_deletion: hannibal::Caller<PrepareUserDeletion>,
    remove_account: hannibal::Caller<RemoveAccount>,
    sync_supplementary_groups: hannibal::Caller<SyncSupplementaryGroups>,
    ensure_groups_exist: hannibal::Caller<EnsureGroupsExist>,
    update_authorized_keys: hannibal::Caller<UpdateAuthorizedKeys>,
}

const ACTOR_ERROR: &str = "User manager actor not available or failed to process the request";

#[bon::bon]
impl UserManager {
    #[builder]
    pub fn new(
        /// Directory where home directories of deleted users are archived
        home_archive_dir: path::PathBuf,
        /// Preview actions without changing users, groups or files on the system
        dry_run: bool,
    ) -> Self {
        if dry_run {
            return Self::from_actor(backends::mock::MockUserManager::new(1000), home_archive_dir);
        }

        #[cfg(target_os = "linux")]
        {
            Self::from_actor(backends::linux::LinuxUserManager::new(), home_archive_dir)
        }

        #[cfg(not(target_os = "linux"))]
        {
            Self::from_actor(backends::mock::MockUserManager::new(1000), home_archive_dir)
        }
    }
}

impl UserManager {
    /// Spawn the actor and keep one caller per message type, erasing the concrete
    /// actor type from the handle.
    fn from_actor<A>(actor: A, home_archive_dir: path::PathBuf) -> Self
    where
        A: hannibal::Handler<CreateUser>
            + hannibal::Handler<UpdateUser>
            + hannibal::Handler<PrepareUserDeletion>
            + hannibal::Handler<RemoveAccount>
            + hannibal::Handler<SyncSupplementaryGroups>
            + hannibal::Handler<EnsureGroupsExist>
            + hannibal::Handler<UpdateAuthorizedKeys>,
    {
        use hannibal::spawnable::Spawnable as _;
        let addr = actor.spawn();
        Self {
            home_archive_dir,
            create_user: addr.caller(),
            update_user: addr.caller(),
            prepare_user_deletion: addr.caller(),
            remove_account: addr.caller(),
            sync_supplementary_groups: addr.caller(),
            ensure_groups_exist: addr.caller(),
            update_authorized_keys: addr.caller(),
        }
    }

    /// Sends [`CreateUser`] to the actor and awaits the created user. `uid` and `gid`
    /// are the stored IDs of a rejoining member, see [`CreateUser`].
    pub async fn create_user(
        &self,
        gh_user: &octocrab::models::Author,
        uid: Option<nix::unistd::Uid>,
        gid: Option<nix::unistd::Gid>,
    ) -> anyhow::Result<store::User> {
        self.create_user
            .call(CreateUser {
                gh_user: gh_user.clone(),
                uid,
                gid,
            })
            .await
            .context(ACTOR_ERROR)?
    }

    /// Sends [`UpdateUser`] to the actor and awaits the updated user.
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

    /// Deletes the platform user of `user`, returning the path of the created home
    /// directory archive, or `None` when there was no account or nothing to archive.
    ///
    /// The actor prepares the deletion ([`PrepareUserDeletion`]) and removes the
    /// account ([`RemoveAccount`]); the disk- and CPU-heavy home directory archiving
    /// between the two runs here, outside the actor, so concurrent deletions only
    /// serialize on the account mutations themselves. A failed archive aborts the
    /// deletion, the caller keeps the user's tombstone and retries on the next sync.
    #[tracing::instrument(
        name = "UserManager::delete_user",
        skip_all,
        fields(user = %user.name(), uid = user.uid().as_raw())
    )]
    pub async fn delete_user(&self, user: &store::User) -> anyhow::Result<Option<path::PathBuf>> {
        let preparation = self
            .prepare_user_deletion
            .call(PrepareUserDeletion { user: user.clone() })
            .await
            .context(ACTOR_ERROR)??;

        let DeletionPreparation::Prepared { home_dir } = preparation else {
            return Ok(None);
        };

        let receipt = crate::archiver::archive_home_dir(&self.home_archive_dir, user, &home_dir)
            .await
            .context("Home directory was not archived, not deleting user")?;
        let archive_path = receipt.archive_path().map(path::Path::to_path_buf);
        if let Some(archive_path) = &archive_path {
            tracing::info!(
                home_dir = %home_dir.display(),
                archive_path = %archive_path.display(),
                "Archived home directory",
            );
        }

        self.remove_account
            .call(RemoveAccount {
                user: user.clone(),
                receipt,
            })
            .await
            .context(ACTOR_ERROR)??;
        Ok(archive_path)
    }

    /// Sends [`SyncSupplementaryGroups`] to the actor and awaits the update.
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

    /// Sends [`EnsureGroupsExist`] to the actor and awaits the group creation.
    pub async fn ensure_groups_exist(&self, groups: &[String]) -> anyhow::Result<()> {
        self.ensure_groups_exist
            .call(EnsureGroupsExist {
                groups: groups.to_vec(),
            })
            .await
            .context(ACTOR_ERROR)?
    }

    /// Sends [`UpdateAuthorizedKeys`] to the actor and awaits the key update.
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
    pub(crate) fn testing(
        actor: backends::testing::TestingUserManager,
        home_archive_dir: path::PathBuf,
    ) -> Self {
        Self::from_actor(actor, home_archive_dir)
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
            actor
                .prepare_user_deletion
                .push_back(Ok(DeletionPreparation::NothingToDo));
            actor
                .prepare_user_deletion
                .push_back(Err(anyhow::anyhow!("scripted failure")));
            let manager = UserManager::testing(actor, path::PathBuf::new());

            let user = test_user(1, "alice");
            manager.delete_user(&user).await.unwrap();
            let err = manager.delete_user(&user).await.unwrap_err();
            assert!(err.to_string().contains("scripted failure"));
        }

        #[tokio::test]
        async fn exhausted_script_fails_with_the_message_name() {
            let manager = UserManager::testing(Default::default(), path::PathBuf::new());

            let err = manager
                .ensure_groups_exist(&["developers".to_string()])
                .await
                .unwrap_err();
            assert!(err.to_string().contains("EnsureGroupsExist"));
        }
    }

    mod deletion_flow {
        use super::*;

        #[tokio::test]
        async fn archives_the_home_directory_before_removing_the_account() {
            let home_dir = tempfile::tempdir().unwrap();
            std::fs::write(home_dir.path().join("notes.txt"), "important").unwrap();
            let archive_dir = tempfile::tempdir().unwrap();

            let mut actor = backends::testing::TestingUserManager::default();
            actor
                .prepare_user_deletion
                .push_back(Ok(DeletionPreparation::Prepared {
                    home_dir: home_dir.path().to_path_buf(),
                }));
            actor.remove_account.push_back(Ok(()));
            let manager = UserManager::testing(actor, archive_dir.path().to_path_buf());

            manager.delete_user(&test_user(1, "alice")).await.unwrap();

            // RemoveAccount was consumed from the script, and the archive exists
            let archives = std::fs::read_dir(archive_dir.path()).unwrap().count();
            assert!(archives > 0, "expected an archive to be created");
        }

        #[tokio::test]
        async fn failed_archive_aborts_the_deletion() {
            // A home directory that is a plain file makes the archiver refuse
            let bogus_home = tempfile::NamedTempFile::new().unwrap();
            let archive_dir = tempfile::tempdir().unwrap();

            let mut actor = backends::testing::TestingUserManager::default();
            actor
                .prepare_user_deletion
                .push_back(Ok(DeletionPreparation::Prepared {
                    home_dir: bogus_home.path().to_path_buf(),
                }));
            // No RemoveAccount response is scripted: reaching it would fail with
            // "No scripted response left", so the assertion below proves it is never sent
            let manager = UserManager::testing(actor, archive_dir.path().to_path_buf());

            let err = manager
                .delete_user(&test_user(1, "alice"))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("not deleting user"));
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

        impl hannibal::Handler<PrepareUserDeletion> for OverlapProbe {
            async fn handle(
                &mut self,
                _ctx: &mut hannibal::Context<Self>,
                _msg: PrepareUserDeletion,
            ) -> anyhow::Result<DeletionPreparation> {
                self.probe().await;
                Ok(DeletionPreparation::NothingToDo)
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

        // The remaining messages are not exercised; the impls only complete the
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

        impl hannibal::Handler<RemoveAccount> for OverlapProbe {
            async fn handle(
                &mut self,
                _ctx: &mut hannibal::Context<Self>,
                _msg: RemoveAccount,
            ) -> anyhow::Result<()> {
                Ok(())
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
            let manager = UserManager::from_actor(
                OverlapProbe {
                    active: active.clone(),
                    max_active: max_active.clone(),
                },
                path::PathBuf::new(),
            );

            let user = test_user(1, "alice");
            let calls = (0..8).map(|i| {
                let manager = &manager;
                let user = &user;
                async move {
                    if i % 2 == 0 {
                        manager.delete_user(user).await.map(|_| ())
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
