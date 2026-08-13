use crate::{
    GlobalArgs, InstallationClientArgs, SyncArgs, store,
    user_manager::{
        self, CreateUser as _, DeleteUser as _, ManageAuthorizedKeys as _,
        ManageSupplementaryGroups as _, UpdateUser as _,
    },
};
use anyhow::Context as _;
use futures::{StreamExt as _, stream};
use std::{collections, path, sync};
use tokio::fs;

/// Maximum number of user deletions that run concurrently. Each deletion archives the
/// user's home directory, which is disk- and CPU-heavy, so it is not unbounded.
const MAX_CONCURRENT_DELETES: usize = 4;

/// Maximum number of users that are processed concurrently. Processing a user talks
/// to GitHub and spawns system commands, so it is not unbounded.
const MAX_CONCURRENT_USER_SYNCS: usize = 32;

async fn org_client(args: &InstallationClientArgs) -> anyhow::Result<octocrab::Octocrab> {
    let private_key = fs::read(args.private_key.as_path())
        .await
        .with_context(|| {
            format!(
                "Failed to read private key from file '{}'",
                args.private_key.display()
            )
        })?;
    let jwt = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_slice())?;

    let app_client = octocrab::Octocrab::builder()
        .app(args.app_id, jwt)
        .build()
        .with_context(|| {
            format!(
                "Failed to build App GitHub client with App ID {} and {}",
                args.app_id,
                args.private_key.display()
            )
        })?;

    let installation = app_client
        .apps()
        .get_org_installation(&args.org)
        .await
        .with_context(|| format!("Failed to get installation for org '{}'", args.org))?;
    let install_crab = app_client.installation(installation.id)?;
    tracing::debug!(
        "Successfully authenticated to GitHub API, starting member sync for org '{}'",
        args.org
    );
    Ok(install_crab)
}

pub struct Octosync {
    data_dir: path::PathBuf,
    user_manager: user_manager::PlatformUserManager,
}

impl Octosync {
    pub async fn new(
        global_config: sync::Arc<GlobalArgs>,
        data_dir: &path::Path,
    ) -> anyhow::Result<Self> {
        let user_manager = user_manager::PlatformUserManager::builder()
            .dry_run(global_config.dry_run)
            .home_archive_dir(data_dir.join("home-archive"))
            .build();
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            user_manager,
        })
    }

    #[tracing::instrument(
        name = "Octosync::process_user",
        skip(self, gh_user, store),
        fields(user = %gh_user.login, id = gh_user.id.into_inner(), )
    )]
    async fn process_user(
        &self,
        gh_user: &octocrab::models::Author,
        store: &store::UserStore,
        groups: &[String],
    ) -> anyhow::Result<store::User> {
        let new_user = match store.data().get(&gh_user.id) {
            Some(user) => self.manage_existing_user(gh_user, user).await?,
            None => self.create_user(gh_user).await?,
        };

        self.user_manager
            .sync_supplementary_groups(&new_user, groups)
            .await
            .context("Failed to sync supplementary groups")?;

        // A failed keys fetch must not fail user processing: the existing
        // authorized_keys file stays in place and the fetch is retried on the next sync.
        if let Err(e) = self.user_manager.update_authorized_keys(&new_user).await {
            tracing::error!(
                "Failed to sync SSH keys, keeping existing authorized_keys: {:?}",
                e
            );
        }
        Ok(new_user)
    }

    async fn create_user(&self, gh_user: &octocrab::models::Author) -> anyhow::Result<store::User> {
        self.user_manager.create_user(gh_user).await
    }

    async fn manage_existing_user(
        &self,
        gh_user: &octocrab::models::Author,
        user: &store::User,
    ) -> anyhow::Result<store::User> {
        tracing::debug!("User exists in store");

        // TODO: re-create the user if it no longer exists on the platform

        self.user_manager.update_user(gh_user, user).await
    }

    #[tracing::instrument(
        name = "Octosync::sync",
        skip(self, args),
        fields(org = %args.octocrab.org)
    )]
    pub async fn sync(self, args: &SyncArgs) -> anyhow::Result<()> {
        let octocrab = org_client(&args.octocrab).await?;
        let (org_members, old_store) = tokio::try_join!(
            get_all_org_members(&octocrab, &args.octocrab.org),
            store::UserStore::from_dir(&self.data_dir)
        )?;
        tracing::info!("Successfully retrieved {} members", org_members.len());
        let org_member_map: collections::HashMap<octocrab::models::UserId, String> =
            collections::HashMap::from_iter(
                org_members.iter().map(|user| (user.id, user.login.clone())),
            );

        let groups: Vec<String> = args
            .group
            .iter()
            .filter_map(|mapping| match mapping {
                crate::GroupMapping::AddGroup(group) => Some(group.clone()),
                crate::GroupMapping::MapGitHubTeam { .. } => None, // Not implemented yet
            })
            .collect();
        // Don't create the groups as part of the try_join above, because at some point we also need
        // to support mapping GitHub teams to Linux groups, which requires the user -> team -> group mapping
        // to be available created before processing the users
        self.user_manager.ensure_groups_exists(&groups).await?;

        let mut new_store = self
            .process_members(&org_members, &old_store, &groups)
            .await?;

        let (users_to_retry, users_to_delete) =
            partition_stale_users(old_store.data(), new_store.data(), &org_member_map);

        for user in users_to_retry {
            tracing::warn!(
                "Keeping user '{}' in store after failed processing, retrying on next sync",
                user.name()
            );
            new_store.data_mut().insert(user.id(), user.clone());
        }

        for user in self.delete_users(users_to_delete).await {
            tracing::warn!(
                "Re-adding user '{}' to store after failed deletion, retrying on next sync",
                user.name()
            );
            new_store.data_mut().insert(user.id(), user.clone());
        }

        new_store.save().await?;
        Ok(())
    }

    /// Process all org members concurrently and collect the successfully processed
    /// users into a new store.
    ///
    /// A processing failure is logged and leaves the user out of the returned store.
    /// It never signals that the user left the org; [`partition_stale_users`] alone
    /// decides which users are deleted.
    async fn process_members(
        &self,
        org_members: &[octocrab::models::Author],
        store: &store::UserStore,
        groups: &[String],
    ) -> anyhow::Result<store::UserStore> {
        let mut new_store = store::UserStore::new(&self.data_dir).await?;
        *new_store.data_mut() = stream::iter(org_members)
            .map(|gh_user| async move {
                self.process_user(gh_user, store, groups)
                    .await
                    .inspect_err(|e| {
                        tracing::error!("Failed to process user '{}': {:?}", gh_user.login, e);
                    })
                    .ok()
            })
            .buffer_unordered(MAX_CONCURRENT_USER_SYNCS)
            .filter_map(|res| async move { res })
            .map(|user| (user.id(), user))
            .collect()
            .await;
        Ok(new_store)
    }

    /// Delete the given users concurrently and return the users whose deletion failed.
    ///
    /// Failures are logged; callers must keep the returned users in the store so the
    /// deletion is retried on the next sync.
    async fn delete_users<'a>(&self, users: Vec<&'a store::User>) -> Vec<&'a store::User> {
        stream::iter(users)
            .map(|user| async move {
                self.user_manager
                    .delete_user(user)
                    .await
                    .map_err(|e| {
                        tracing::error!("Failed to delete user '{}': {:?}", user.name(), e);
                        user
                    })
                    .err()
            })
            // Deletion archives the user's home directory, which can take a while for large
            // home directories, run a few deletions concurrently
            .buffer_unordered(MAX_CONCURRENT_DELETES)
            .filter_map(|res| async move { res })
            .collect()
            .await
    }

    #[tracing::instrument(name = "Octosync::delete", skip(self))]
    pub async fn delete(&self) -> anyhow::Result<()> {
        let mut store = store::UserStore::from_dir(&self.data_dir).await?;
        let failed_ids: collections::HashSet<octocrab::models::UserId> = self
            .delete_users(store.data().values().collect())
            .await
            .into_iter()
            .map(store::User::id)
            .collect();
        store.data_mut().retain(|id, _| failed_ids.contains(id));

        if store.data().is_empty() {
            tracing::info!("All users deleted successfully, removing store data file");

            store.delete().await?;
        } else {
            tracing::warn!(
                "Some users could not be deleted. Remaining users in store: {}",
                store.data().len()
            );
            store
                .save()
                .await
                .context("Failed to save store data after deletion")?;
        }
        Ok(())
    }
}

/// Split the users of the previous store that are missing from the new store into
/// users to keep for a retry and users to delete.
///
/// Deletion is decided solely by absence from the fetched org member list. A user that
/// is still an org member but missing from the new store failed processing; deleting
/// them would turn any correlated per-user failure (e.g. rate-limited key fetches)
/// into a mass deletion, so they are kept unchanged and retried on the next sync.
fn partition_stale_users<'a>(
    old_users: &'a collections::HashMap<octocrab::models::UserId, store::User>,
    new_users: &collections::HashMap<octocrab::models::UserId, store::User>,
    org_member_map: &collections::HashMap<octocrab::models::UserId, String>,
) -> (Vec<&'a store::User>, Vec<&'a store::User>) {
    old_users
        .values()
        .filter(|user| !new_users.contains_key(&user.id()))
        .partition(|user| org_member_map.contains_key(&user.id()))
}

async fn get_all_org_members(
    octocrab: &octocrab::Octocrab,
    org: &str,
) -> anyhow::Result<Vec<octocrab::models::Author>> {
    use futures::TryStreamExt as _;
    let stream = octocrab
        .orgs(org)
        .list_members()
        .per_page(100)
        .send()
        .await
        .with_context(|| format!("Failed to list members for org '{}'", org))?
        .into_stream(octocrab);

    Ok(stream.try_collect().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: u64, name: &str) -> store::User {
        store::User::builder()
            .id(octocrab::models::UserId(id))
            .name(name.to_string())
            .uid(nix::unistd::Uid::from_raw(1000 + id as u32))
            .build()
    }

    fn user_map(
        users: &[store::User],
    ) -> collections::HashMap<octocrab::models::UserId, store::User> {
        users.iter().map(|u| (u.id(), u.clone())).collect()
    }

    fn member_map(users: &[store::User]) -> collections::HashMap<octocrab::models::UserId, String> {
        users
            .iter()
            .map(|u| (u.id(), u.name().to_string()))
            .collect()
    }

    mod partition_stale_users {
        use super::*;

        /// Regression test for the 2026-08-12 incident: every member is still in the
        /// org, but all of them failed processing, so the new store is empty.
        /// No user may be deleted; all must be retried.
        #[test]
        fn all_members_failed_processing_deletes_nobody() {
            let users = [user(1, "a"), user(2, "b"), user(3, "c")];
            let old = user_map(&users);
            let new = user_map(&[]);
            let members = member_map(&users);

            let (retry, delete) = partition_stale_users(&old, &new, &members);

            assert_eq!(retry.len(), 3);
            assert!(delete.is_empty());
        }

        #[test]
        fn user_absent_from_member_list_is_deleted() {
            let remaining = user(1, "a");
            let left = user(2, "b");
            let old = user_map(&[remaining.clone(), left.clone()]);
            let new = user_map(&[remaining]);
            let members = member_map(std::slice::from_ref(&old[&octocrab::models::UserId(1)]));

            let (retry, delete) = partition_stale_users(&old, &new, &members);

            assert!(retry.is_empty());
            assert_eq!(delete, vec![&left]);
        }

        #[test]
        fn successfully_processed_user_is_neither_retried_nor_deleted() {
            let processed = user(1, "a");
            let old = user_map(std::slice::from_ref(&processed));
            let new = user_map(std::slice::from_ref(&processed));
            let members = member_map(&[processed]);

            let (retry, delete) = partition_stale_users(&old, &new, &members);

            assert!(retry.is_empty());
            assert!(delete.is_empty());
        }

        #[test]
        fn failed_member_is_retried_while_removed_member_is_deleted() {
            let processed = user(1, "a");
            let failed = user(2, "b");
            let left = user(3, "c");
            let old = user_map(&[processed.clone(), failed.clone(), left.clone()]);
            let new = user_map(std::slice::from_ref(&processed));
            let members = member_map(&[processed, failed.clone()]);

            let (retry, delete) = partition_stale_users(&old, &new, &members);

            assert_eq!(retry, vec![&failed]);
            assert_eq!(delete, vec![&left]);
        }

        #[test]
        fn new_member_in_new_store_only_is_untouched() {
            let joined = user(1, "a");
            let old = user_map(&[]);
            let new = user_map(std::slice::from_ref(&joined));
            let members = member_map(&[joined]);

            let (retry, delete) = partition_stale_users(&old, &new, &members);

            assert!(retry.is_empty());
            assert!(delete.is_empty());
        }
    }
}
