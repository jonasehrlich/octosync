use crate::{
    GlobalArgs, InstallationClientArgs, SyncArgs, groups, public_keys, store, user_manager,
};
use anyhow::Context as _;
use futures::{StreamExt as _, stream};
use std::{collections, path, sync, time};
use tokio::fs;

/// Maximum number of user deletions that run concurrently. Each deletion archives the
/// user's home directory outside the user manager actor, which is disk- and CPU-heavy,
/// so it is not unbounded. Account mutations serialize on the actor.
const MAX_CONCURRENT_DELETES: usize = 4;

/// Maximum number of users processed concurrently during a sync. Bounds the GitHub
/// requests in flight at the same time. The platform operations of each user are
/// serialized by the user manager actor.
const MAX_CONCURRENT_USER_SYNCS: usize = 8;

/// Maximum number of items per page supported by the GitHub API
pub(crate) const GITHUB_MAX_PER_PAGE: u8 = 100;

/// Timeout for establishing a connection to the GitHub API
const CONNECT_TIMEOUT: time::Duration = time::Duration::from_secs(10);
/// Timeout for individual socket reads/writes, so a hung request cannot stall the sync forever
const READ_WRITE_TIMEOUT: time::Duration = time::Duration::from_secs(30);

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
        .set_connect_timeout(Some(CONNECT_TIMEOUT))
        .set_read_timeout(Some(READ_WRITE_TIMEOUT))
        .set_write_timeout(Some(READ_WRITE_TIMEOUT))
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
    user_manager: user_manager::UserManager,
}

impl Octosync {
    pub async fn new(
        global_config: sync::Arc<GlobalArgs>,
        data_dir: &path::Path,
    ) -> anyhow::Result<Self> {
        let user_manager = user_manager::UserManager::builder()
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
        skip_all,
        fields(user = %gh_user.login, id = gh_user.id.into_inner(), )
    )]
    async fn process_user(
        &self,
        octocrab: &octocrab::Octocrab,
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

        // A failed fetch must not fail processing the user: the authorized_keys file keeps its
        // current keys and is refreshed on the next sync
        match public_keys::PublicKeys::fetch(octocrab, new_user.name()).await {
            Ok(keys) => {
                if keys.is_empty() {
                    tracing::warn!("User has no public keys on GitHub");
                }
                self.user_manager
                    .update_authorized_keys(&new_user, &keys)
                    .await
                    .context("Failed to sync SSH keys")?;
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to fetch public keys, not updating authorized_keys: {:#}",
                    e
                );
            }
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
        let (org_members, old_store, assignments) = tokio::try_join!(
            get_all_org_members(&octocrab, &args.octocrab.org),
            store::UserStore::from_dir(&self.data_dir),
            groups::GroupAssignments::resolve(&octocrab, &args.octocrab.org, &args.group)
        )?;
        tracing::info!("Successfully retrieved {} members", org_members.len());

        if org_members.is_empty() && !old_store.data().is_empty() {
            anyhow::bail!(
                "Refusing to sync: org '{}' returned no members while {} users are stored. \
                 Run the 'delete' command to remove all users intentionally.",
                args.octocrab.org,
                old_store.data().len()
            );
        }

        let org_member_map: collections::HashMap<octocrab::models::UserId, String> =
            collections::HashMap::from_iter(
                org_members.iter().map(|user| (user.id, user.login.clone())),
            );

        // Create the resolved groups before the users are processed, so the per-user
        // group sync can rely on every managed group existing
        self.user_manager
            .ensure_groups_exist(&assignments.all_groups())
            .await?;

        let mut new_store = self
            .process_members(&octocrab, &org_members, &old_store, &assignments)
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

        if would_delete_all_users(old_store.data(), &users_to_delete) {
            for user in users_to_delete {
                new_store.data_mut().insert(user.id(), user.clone());
            }
            new_store.save().await?;
            anyhow::bail!(
                "Refusing to delete all {} stored users in a single sync. None of them is in \
                 the fetched member list of org '{}' ({} members). All users are kept in \
                 the store; run the 'delete' command to remove them intentionally.",
                old_store.data().len(),
                args.octocrab.org,
                org_members.len()
            );
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
        octocrab: &octocrab::Octocrab,
        org_members: &[octocrab::models::Author],
        store: &store::UserStore,
        assignments: &groups::GroupAssignments,
    ) -> anyhow::Result<store::UserStore> {
        let mut new_store = store::UserStore::new(&self.data_dir).await?;
        *new_store.data_mut() = stream::iter(org_members)
            .map(|gh_user| {
                let groups = assignments.user_groups(gh_user.id);
                async move {
                    self.process_user(octocrab, gh_user, store, &groups)
                        .await
                        .inspect_err(|e| {
                            tracing::error!("Failed to process user '{}': {:?}", gh_user.login, e);
                        })
                        .ok()
                }
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

/// Circuit breaker for [`Octosync::sync`]. True when the pending deletions would empty
/// a non-empty store.
///
/// A member list that no longer contains a single stored user is far more likely a
/// mis-scoped installation, an org rename or an empty-but-successful API response than
/// every member leaving at once, so such a sync must refuse to delete anyone.
fn would_delete_all_users(
    old_users: &collections::HashMap<octocrab::models::UserId, store::User>,
    users_to_delete: &[&store::User],
) -> bool {
    !users_to_delete.is_empty() && users_to_delete.len() == old_users.len()
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

        /// The successful-but-empty member list: `sync()` bails on this before
        /// processing, but the circuit breaker must still trip as defense in depth.
        #[test]
        fn empty_member_list_puts_all_users_in_delete_bucket_and_trips_guard() {
            let users = [user(1, "a"), user(2, "b")];
            let old = user_map(&users);
            let new = user_map(&[]);
            let members = member_map(&[]);

            let (retry, delete) = partition_stale_users(&old, &new, &members);

            assert!(retry.is_empty());
            assert_eq!(delete.len(), 2);
            assert!(would_delete_all_users(&old, &delete));
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

    mod deletes_entire_store {
        use super::*;

        #[test]
        fn deleting_every_stored_user_trips_the_guard() {
            let users = [user(1, "a"), user(2, "b")];
            let old = user_map(&users);
            let delete: Vec<&store::User> = old.values().collect();

            assert!(would_delete_all_users(&old, &delete));
        }

        #[test]
        fn deleting_a_subset_is_allowed() {
            let users = [user(1, "a"), user(2, "b")];
            let old = user_map(&users);
            let delete = vec![&old[&octocrab::models::UserId(1)]];

            assert!(!would_delete_all_users(&old, &delete));
        }

        #[test]
        fn nothing_to_delete_is_allowed() {
            let old = user_map(&[user(1, "a")]);

            assert!(!would_delete_all_users(&old, &[]));
        }

        /// A single-user store where that user really left still trips the guard;
        /// the explicit delete command is the intentional path for this case.
        #[test]
        fn deleting_the_only_stored_user_trips_the_guard() {
            let only = user(1, "a");
            let old = user_map(std::slice::from_ref(&only));
            let delete: Vec<&store::User> = old.values().collect();

            assert!(would_delete_all_users(&old, &delete));
        }
    }
}
