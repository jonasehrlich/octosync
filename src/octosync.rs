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
            None => {
                self.create_user(gh_user, store.archived().get(&gh_user.id))
                    .await?
            }
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

    /// Create the platform user, re-using the archived UID when the member rejoined.
    ///
    /// A taken UID fails the creation loudly and the tombstone is retried on the next
    /// sync; falling back to a fresh UID would recreate exactly the ownership drift
    /// the tombstones exist to prevent.
    async fn create_user(
        &self,
        gh_user: &octocrab::models::Author,
        archived: Option<&store::ArchivedUser>,
    ) -> anyhow::Result<store::User> {
        if let Some(archived) = archived {
            tracing::info!(
                uid = archived.uid().as_raw(),
                "Member rejoined, re-creating the account with its stored UID"
            );
        }
        self.user_manager
            .create_user(gh_user, archived.map(store::ArchivedUser::uid))
            .await
    }

    async fn manage_existing_user(
        &self,
        gh_user: &octocrab::models::Author,
        user: &store::User,
    ) -> anyhow::Result<store::User> {
        tracing::debug!("User exists in store");
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

        let users_to_delete: Vec<store::User> = users_to_delete.into_iter().cloned().collect();
        self.archive_and_delete(&mut new_store, users_to_delete)
            .await
    }

    /// Archive the given users as tombstones and converge every archived account
    /// toward deletion.
    ///
    /// The tombstones are saved before any account is removed, so the UID mapping
    /// survives a crash anywhere in the deletion window. Every archived user whose
    /// account still exists is re-enqueued here on each sync: a deletion interrupted
    /// between the store save and `userdel` is finished by a later sync instead of
    /// orphaning a live account, and a failed deletion needs no in-memory retry state.
    async fn archive_and_delete(
        &self,
        store: &mut store::UserStore,
        leavers: Vec<store::User>,
    ) -> anyhow::Result<()> {
        let deleted_at = chrono::Utc::now();
        for user in leavers {
            tracing::info!("Archiving user '{}' for deletion", user.name());
            store.archive_user(user, deleted_at);
        }
        store.save().await?;

        for (id, archive_path) in self.delete_archived_accounts(store.archived()).await {
            // An already-gone account reports no archive; keep the path recorded by
            // the sync that created it
            if let Some(archive_path) = archive_path {
                store.record_home_archive(&id, archive_path);
            }
        }
        store.save().await
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
        // Tombstones are carried over wholesale before any member is processed, so
        // their survival never depends on per-user processing succeeding
        *new_store.archived_mut() = store.archived().clone();
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
        // A tombstoned member that was processed has rejoined and their account is
        // re-created, so the tombstone is spent
        new_store.prune_rejoined();
        Ok(new_store)
    }

    /// Delete the platform accounts of the archived users concurrently, returning the
    /// ID and home archive path of every successful deletion.
    ///
    /// Failures are only logged: the tombstone stays in the store, so the deletion is
    /// retried on every sync until the account is gone.
    async fn delete_archived_accounts(
        &self,
        archived: &store::ArchivedMap,
    ) -> Vec<(octocrab::models::UserId, Option<path::PathBuf>)> {
        stream::iter(archived.values())
            .map(|archived_user| async move {
                self.user_manager
                    .delete_user(&store::User::from(archived_user))
                    .await
                    .map(|archive_path| (archived_user.id(), archive_path))
                    .inspect_err(|e| {
                        tracing::error!(
                            "Failed to delete user '{}': {:?}",
                            archived_user.name(),
                            e
                        );
                    })
                    .ok()
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
        let users: Vec<store::User> = store.data().values().cloned().collect();
        let count = users.len();
        // The store file is kept: the tombstones preserve every UID so members
        // re-created by a later sync get their old UID back
        self.archive_and_delete(&mut store, users).await?;
        tracing::info!(
            "Archived {count} users for deletion, keeping their tombstones in the store"
        );
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

    mod orchestration {
        use super::*;
        use crate::user_manager::{
            DeletionPreparation, UserManager, backends::testing::TestingUserManager,
        };

        /// A GitHub user as returned by the API. `Author` is non-exhaustive, so it can
        /// only be built through deserialization.
        fn author(id: u64, login: &str) -> octocrab::models::Author {
            let url = "https://api.github.com/";
            serde_json::from_value(serde_json::json!({
                "login": login,
                "id": id,
                "node_id": "node",
                "avatar_url": url,
                "gravatar_id": "",
                "url": url,
                "html_url": url,
                "followers_url": url,
                "following_url": url,
                "gists_url": url,
                "starred_url": url,
                "subscriptions_url": url,
                "organizations_url": url,
                "repos_url": url,
                "events_url": url,
                "received_events_url": url,
                "type": "User",
                "site_admin": false,
            }))
            .unwrap()
        }

        /// A client whose every request fails: it points at a local port that was
        /// bound and released, so connections are refused
        fn unreachable_octocrab() -> octocrab::Octocrab {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            octocrab::Octocrab::builder()
                .base_uri(format!("http://127.0.0.1:{port}"))
                .unwrap()
                .build()
                .unwrap()
        }

        /// An [`Octosync`] backed by the scripted user manager. The [`tempfile::TempDir`]
        /// is its data directory and must outlive it.
        fn octosync_with(actor: TestingUserManager) -> (Octosync, tempfile::TempDir) {
            let data_dir = tempfile::tempdir().unwrap();
            let octosync = Octosync {
                data_dir: data_dir.path().to_path_buf(),
                user_manager: UserManager::testing(actor, data_dir.path().join("home-archive")),
            };
            (octosync, data_dir)
        }

        #[tokio::test]
        async fn process_user_creates_missing_user_and_tolerates_failed_key_fetch() {
            let mut actor = TestingUserManager::default();
            actor.create_user.push_back(Ok(user(1, "alice")));
            actor.sync_supplementary_groups.push_back(Ok(()));
            // No UpdateAuthorizedKeys response is scripted: the key fetch against the
            // unreachable client fails, so the keys must be skipped, not synced
            let (octosync, _data_dir) = octosync_with(actor);
            let empty_store = store::UserStore::new(_data_dir.path()).await.unwrap();

            let processed = octosync
                .process_user(
                    &unreachable_octocrab(),
                    &author(1, "alice"),
                    &empty_store,
                    &[],
                )
                .await
                .unwrap();

            assert_eq!(processed.name(), "alice");
        }

        #[tokio::test]
        async fn process_user_updates_a_stored_user() {
            let mut actor = TestingUserManager::default();
            actor.update_user.push_back(Ok(user(1, "alice-renamed")));
            actor.sync_supplementary_groups.push_back(Ok(()));
            let (octosync, _data_dir) = octosync_with(actor);
            let mut old_store = store::UserStore::new(_data_dir.path()).await.unwrap();
            old_store
                .data_mut()
                .insert(octocrab::models::UserId(1), user(1, "alice"));

            let processed = octosync
                .process_user(
                    &unreachable_octocrab(),
                    &author(1, "alice-renamed"),
                    &old_store,
                    &[],
                )
                .await
                .unwrap();

            assert_eq!(processed.name(), "alice-renamed");
        }

        #[tokio::test]
        async fn process_user_fails_when_the_group_sync_fails() {
            let mut actor = TestingUserManager::default();
            actor.create_user.push_back(Ok(user(1, "alice")));
            actor
                .sync_supplementary_groups
                .push_back(Err(anyhow::anyhow!("boom")));
            let (octosync, _data_dir) = octosync_with(actor);
            let empty_store = store::UserStore::new(_data_dir.path()).await.unwrap();

            let err = octosync
                .process_user(
                    &unreachable_octocrab(),
                    &author(1, "alice"),
                    &empty_store,
                    &[],
                )
                .await
                .unwrap_err();

            assert!(
                err.to_string()
                    .contains("Failed to sync supplementary groups")
            );
        }

        /// A member whose processing fails must be left out of the new store, so
        /// [`partition_stale_users`] keeps them for a retry instead of deleting them.
        #[tokio::test]
        async fn process_members_leaves_a_failed_member_out_of_the_new_store() {
            let mut actor = TestingUserManager::default();
            actor.create_user.push_back(Err(anyhow::anyhow!("boom")));
            let (octosync, _data_dir) = octosync_with(actor);
            let empty_store = store::UserStore::new(_data_dir.path()).await.unwrap();

            let new_store = octosync
                .process_members(
                    &unreachable_octocrab(),
                    &[author(1, "alice")],
                    &empty_store,
                    &groups::GroupAssignments::default(),
                )
                .await
                .unwrap();

            assert!(new_store.data().is_empty());
        }

        #[tokio::test]
        async fn process_members_collects_a_processed_member_into_the_new_store() {
            let mut actor = TestingUserManager::default();
            actor.create_user.push_back(Ok(user(1, "alice")));
            actor.sync_supplementary_groups.push_back(Ok(()));
            let (octosync, _data_dir) = octosync_with(actor);
            let empty_store = store::UserStore::new(_data_dir.path()).await.unwrap();

            let new_store = octosync
                .process_members(
                    &unreachable_octocrab(),
                    &[author(1, "alice")],
                    &empty_store,
                    &groups::GroupAssignments::default(),
                )
                .await
                .unwrap();

            let stored = new_store.data().get(&octocrab::models::UserId(1)).unwrap();
            assert_eq!(stored.name(), "alice");
        }

        /// A rejoining member (present in the archived map, absent from the active
        /// users) is created with the stored UID and their tombstone is spent.
        #[tokio::test]
        async fn process_members_recreates_a_rejoined_member_with_the_stored_uid() {
            let mut actor = TestingUserManager::default();
            actor.create_user.push_back(Ok(user(1, "alice")));
            actor.sync_supplementary_groups.push_back(Ok(()));
            let received_uids = actor.create_user_uids.clone();
            let (octosync, _data_dir) = octosync_with(actor);
            let mut old_store = store::UserStore::new(_data_dir.path()).await.unwrap();
            old_store.archive_user(user(1, "alice"), chrono::Utc::now());

            let new_store = octosync
                .process_members(
                    &unreachable_octocrab(),
                    &[author(1, "alice")],
                    &old_store,
                    &groups::GroupAssignments::default(),
                )
                .await
                .unwrap();

            assert_eq!(
                *received_uids.lock().unwrap(),
                [Some(nix::unistd::Uid::from_raw(1001))]
            );
            assert!(new_store.data().contains_key(&octocrab::models::UserId(1)));
            assert!(new_store.archived().is_empty());
        }

        /// Tombstones survive member processing structurally: a failed re-creation
        /// leaves the tombstone in place for a retry on the next sync.
        #[tokio::test]
        async fn process_members_keeps_the_tombstone_when_recreation_fails() {
            let mut actor = TestingUserManager::default();
            actor
                .create_user
                .push_back(Err(anyhow::anyhow!("UID is taken")));
            let (octosync, _data_dir) = octosync_with(actor);
            let mut old_store = store::UserStore::new(_data_dir.path()).await.unwrap();
            old_store.archive_user(user(1, "alice"), chrono::Utc::now());

            let new_store = octosync
                .process_members(
                    &unreachable_octocrab(),
                    &[author(1, "alice")],
                    &old_store,
                    &groups::GroupAssignments::default(),
                )
                .await
                .unwrap();

            assert!(new_store.data().is_empty());
            assert!(
                new_store
                    .archived()
                    .contains_key(&octocrab::models::UserId(1))
            );
        }

        /// A leaver is tombstoned and the store saved before the account is removed,
        /// so a removal failure leaves a durable tombstone instead of in-memory
        /// retry state.
        #[tokio::test]
        async fn archive_and_delete_keeps_the_tombstone_when_removal_fails() {
            let mut actor = TestingUserManager::default();
            actor
                .prepare_user_deletion
                .push_back(Err(anyhow::anyhow!("boom")));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path()).await.unwrap();

            octosync
                .archive_and_delete(&mut store, vec![user(1, "alice")])
                .await
                .unwrap();

            let saved = store::UserStore::from_dir(data_dir.path()).await.unwrap();
            let tombstone = &saved.archived()[&octocrab::models::UserId(1)];
            assert_eq!(tombstone.name(), "alice");
            assert_eq!(tombstone.home_archive(), None);
            assert!(saved.data().is_empty());
        }

        /// A successful deletion records the home archive path on the tombstone.
        #[tokio::test]
        async fn archive_and_delete_records_the_home_archive_path() {
            let home_dir = tempfile::tempdir().unwrap();
            std::fs::write(home_dir.path().join("notes.txt"), "important").unwrap();

            let mut actor = TestingUserManager::default();
            actor
                .prepare_user_deletion
                .push_back(Ok(DeletionPreparation::Prepared {
                    home_dir: home_dir.path().to_path_buf(),
                }));
            actor.remove_account.push_back(Ok(()));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path()).await.unwrap();

            octosync
                .archive_and_delete(&mut store, vec![user(1, "alice")])
                .await
                .unwrap();

            let saved = store::UserStore::from_dir(data_dir.path()).await.unwrap();
            let tombstone = &saved.archived()[&octocrab::models::UserId(1)];
            let archive = tombstone.home_archive().expect("archive path recorded");
            assert!(archive.exists());
        }

        /// A tombstone whose account is already gone reports no archive; the path
        /// recorded by the sync that archived the home directory must survive.
        #[tokio::test]
        async fn archive_and_delete_keeps_a_recorded_archive_path() {
            let mut actor = TestingUserManager::default();
            actor
                .prepare_user_deletion
                .push_back(Ok(DeletionPreparation::NothingToDo));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path()).await.unwrap();
            store.archive_user(user(1, "alice"), chrono::Utc::now());
            let archive = path::PathBuf::from("/data/home-archive/alice.tar.gz");
            store.record_home_archive(&octocrab::models::UserId(1), archive.clone());

            octosync
                .archive_and_delete(&mut store, vec![])
                .await
                .unwrap();

            assert_eq!(
                store.archived()[&octocrab::models::UserId(1)].home_archive(),
                Some(archive.as_path())
            );
        }

        /// The `delete` command tombstones every user instead of removing the store
        /// file, so the UID memory survives a full wipe.
        #[tokio::test]
        async fn delete_command_keeps_the_store_file_with_tombstones() {
            let mut actor = TestingUserManager::default();
            actor
                .prepare_user_deletion
                .push_back(Ok(DeletionPreparation::NothingToDo));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path()).await.unwrap();
            store
                .data_mut()
                .insert(octocrab::models::UserId(1), user(1, "alice"));
            store.save().await.unwrap();

            octosync.delete().await.unwrap();

            let saved = store::UserStore::from_dir(data_dir.path()).await.unwrap();
            assert!(saved.data().is_empty());
            assert_eq!(
                saved.archived()[&octocrab::models::UserId(1)].name(),
                "alice"
            );
        }
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
