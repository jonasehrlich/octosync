use crate::{
    InstallationClientArgs, PurgeArgs, SyncArgs, groups, public_keys, store, user_manager,
};
use anyhow::Context as _;
use futures::{StreamExt as _, stream};
use std::{collections, path, time};
use tokio::fs;

/// Bounds concurrency of GitHub requests
const MAX_CONCURRENT_USER_SYNCS: usize = 8;
pub(crate) const GITHUB_MAX_PER_PAGE: u8 = 100;
const GITHUB_CONNECT_TIMEOUT: time::Duration = time::Duration::from_secs(10);
const SOCKET_RW_TIMEOUT: time::Duration = time::Duration::from_secs(30);

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
        .set_connect_timeout(Some(GITHUB_CONNECT_TIMEOUT))
        .set_read_timeout(Some(SOCKET_RW_TIMEOUT))
        .set_write_timeout(Some(SOCKET_RW_TIMEOUT))
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
    /// Use the mock backend and disable store writes.
    dry_run: bool,
    user_manager: user_manager::UserManager,
}

impl Octosync {
    pub fn new(dry_run: bool, data_dir: &path::Path) -> Self {
        let user_manager = user_manager::UserManager::builder()
            .dry_run(dry_run)
            .build();
        Self {
            data_dir: data_dir.to_path_buf(),
            dry_run,
            user_manager,
        }
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
            Some(user) => {
                tracing::debug!("User exists in store");
                self.user_manager.update_user(gh_user, user).await?
            }
            None => self.create_user(gh_user, store).await?,
        };

        self.user_manager
            .sync_supplementary_groups(&new_user, groups)
            .await
            .context("Failed to sync supplementary groups")?;

        // Keep existing keys when GitHub cannot refresh them.
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

    /// Create the platform user, re-using the tombstoned IDs when the member rejoined.
    ///
    /// A rejoin whose stored ID is taken fails and is retried on the next sync.
    ///
    /// Falling back to fresh IDs would create an ownership drift that should be prevented by the
    /// tombstones. A brand-new member must not be allocated an ID that a tombstone (departed or
    /// purged) reserves either, so the reserved IDs are propagated to the creation.
    async fn create_user(
        &self,
        gh_user: &octocrab::models::Author,
        store: &store::UserStore,
    ) -> anyhow::Result<store::User> {
        // Refuse a recycled login for a different GitHub account, rather than expose the departed
        // member's account. This is something that needs to be solved by an operator.
        if let Some(tombstone) = store
            .departed()
            .values()
            .find(|tombstone| tombstone.name() == gh_user.login)
            && tombstone.id() != gh_user.id
        {
            anyhow::bail!(
                "Login '{}' belonged to the departed member with GitHub ID {}, but the joining \
                 member has GitHub ID {}: the login was recycled by a different person, refusing \
                 to create the user",
                gh_user.login,
                tombstone.id(),
                gh_user.id
            );
        }

        // Departed members reuse their account, purged members reuse only its IDs.
        let stored_ids = store
            .departed()
            .get(&gh_user.id)
            .map(|tombstone| (tombstone.uid(), tombstone.gid()))
            .or_else(|| {
                store
                    .purged()
                    .get(&gh_user.id)
                    .map(|tombstone| (tombstone.uid(), tombstone.gid()))
            });
        let ids = match stored_ids {
            Some((uid, gid)) => {
                tracing::info!(
                    uid = uid.as_raw(),
                    "Member rejoined, re-creating the account with its stored IDs"
                );
                user_manager::AccountIds::Stored { uid, gid }
            }
            None => user_manager::AccountIds::Fresh {
                reserved_uids: store
                    .departed()
                    .values()
                    .map(store::DepartedUser::uid)
                    .chain(store.purged().values().map(store::PurgedUser::uid))
                    .collect(),
                reserved_gids: store
                    .departed()
                    .values()
                    .filter_map(store::DepartedUser::gid)
                    .chain(store.purged().values().filter_map(store::PurgedUser::gid))
                    .collect(),
            },
        };
        self.user_manager.create_user(gh_user, ids).await
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
            store::UserStore::from_dir(&self.data_dir, self.dry_run),
            groups::GroupAssignments::resolve(&octocrab, &args.octocrab.org, &args.group)
        )?;
        tracing::info!("Successfully retrieved {} members", org_members.len());

        let org_member_map: collections::HashMap<octocrab::models::UserId, String> =
            collections::HashMap::from_iter(
                org_members.iter().map(|user| (user.id, user.login.clone())),
            );

        if membership_has_no_stored_users(old_store.data(), &org_member_map) {
            anyhow::bail!(
                "Refusing to sync: none of the {} stored users is in the fetched member list \
                 of org '{}' ({} members). Nothing was changed on the system. Run the 'delete' \
                 command to expire all users intentionally.",
                old_store.data().len(),
                args.octocrab.org,
                org_members.len()
            );
        }

        // Create the resolved groups before the users are processed, so the per-user
        // group sync can rely on every managed group existing
        self.user_manager
            .ensure_groups_exist(&assignments.all_groups())
            .await?;

        let mut new_store = self
            .process_members(&octocrab, &org_members, &old_store, &assignments)
            .await?;

        let (users_to_retry, leavers) =
            partition_stale_users(old_store.data(), new_store.data(), &org_member_map);

        for user in users_to_retry {
            tracing::warn!(
                "Keeping user '{}' in store after failed processing, retrying on next sync",
                user.name()
            );
            new_store.data_mut().insert(user.id(), user.clone());
        }

        if would_expire_all_users(old_store.data(), &leavers) {
            for user in leavers {
                new_store.data_mut().insert(user.id(), user.clone());
            }
            new_store.save().await?;
            anyhow::bail!(
                "Refusing to expire all {} stored users in a single sync. None of them is in \
                 the fetched member list of org '{}' ({} members). All users are kept in \
                 the store; run the 'delete' command to expire them intentionally.",
                old_store.data().len(),
                args.octocrab.org,
                org_members.len()
            );
        }

        let leavers: Vec<store::User> = leavers.into_iter().cloned().collect();
        self.depart_and_expire(&mut new_store, leavers, &org_member_map)
            .await?;

        self.purge_expired(&mut new_store, &org_member_map, args.purge_after_days)
            .await
    }

    /// Permanently remove departed accounts once both the tombstone and account expiry
    /// exceed the retention period. Current members are never purged.
    async fn purge_expired(
        &self,
        store: &mut store::UserStore,
        org_member_map: &collections::HashMap<octocrab::models::UserId, String>,
        purge_after_days: u32,
    ) -> anyhow::Result<()> {
        if org_member_map.is_empty() {
            anyhow::bail!(
                "Refusing to purge with an empty organization member list; nothing was removed"
            );
        }

        let now = chrono::Utc::now();
        let cutoff = now - chrono::Duration::days(purge_after_days.into());

        let candidates: Vec<store::User> = store
            .departed()
            .values()
            .filter(|tombstone| {
                tombstone.departed_at() <= cutoff && !org_member_map.contains_key(&tombstone.id())
            })
            .map(store::User::from)
            .collect();

        for user in candidates {
            match self.user_manager.purge_user(&user, cutoff).await {
                Ok(user_manager::PurgeOutcome::Purged) => {
                    tracing::info!(
                        user = %user.name(),
                        uid = user.uid().as_raw(),
                        "Purged the expired account after the retention period"
                    );
                    store.mark_purged(&user.id(), now);
                }
                // Keep the tombstone until a purge succeeds.
                Ok(user_manager::PurgeOutcome::NoAccount) => tracing::warn!(
                    "No account for departed user '{}', not marking the tombstone purged",
                    user.name()
                ),
                Ok(user_manager::PurgeOutcome::NotExpired) => tracing::warn!(
                    "Account of '{}' has no shadow expiry older than the retention period, \
                     not purging despite the tombstone age",
                    user.name()
                ),
                // The tombstone stays departed, so the purge is retried on later syncs
                Err(e) => {
                    tracing::error!("Failed to purge the account of '{}': {:?}", user.name(), e);
                }
            }
        }
        store.save().await
    }

    /// Persist new departures, then retry every unfinished account teardown. A tombstone only
    /// carries the completion timestamp once [`user_manager::ExpireAccount`] succeeded, so a
    /// teardown that failed or was interrupted is retried by the next sync without
    /// in-memory retry state
    async fn depart_and_expire(
        &self,
        store: &mut store::UserStore,
        leavers: Vec<store::User>,
        org_member_map: &collections::HashMap<octocrab::models::UserId, String>,
    ) -> anyhow::Result<()> {
        let departed_at = chrono::Utc::now();
        for user in leavers {
            tracing::info!(user = %user.name(), "Recording departure, expiring the account");
            store.depart_user(user, departed_at);
        }
        store.save().await?;

        let pending: Vec<store::User> = store
            .departed()
            .values()
            .filter(|departed| {
                departed.expired_at().is_none() && !org_member_map.contains_key(&departed.id())
            })
            .map(store::User::from)
            .collect();

        // A failed teardown keeps its empty completion timestamp for the next sync.
        for user in pending {
            match self.user_manager.expire_user(&user).await {
                Ok(()) => store.mark_expired(&user.id(), chrono::Utc::now()),
                Err(e) => {
                    tracing::error!("Failed to expire the account of '{}': {:?}", user.name(), e)
                }
            }
        }
        store.save().await
    }

    /// Process members concurrently and collect successful results into a new store.
    async fn process_members(
        &self,
        octocrab: &octocrab::Octocrab,
        org_members: &[octocrab::models::Author],
        store: &store::UserStore,
        assignments: &groups::GroupAssignments,
    ) -> anyhow::Result<store::UserStore> {
        let mut new_store = store::UserStore::new(&self.data_dir, self.dry_run).await?;
        *new_store.departed_mut() = store.departed().clone();
        *new_store.purged_mut() = store.purged().clone();
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
        // Successful processing spends any tombstone for a rejoining member.
        new_store.prune_rejoined();
        Ok(new_store)
    }

    #[tracing::instrument(name = "Octosync::delete", skip(self))]
    pub async fn delete(&self) -> anyhow::Result<()> {
        let mut store = store::UserStore::from_dir(&self.data_dir, self.dry_run).await?;
        let users: Vec<store::User> = store.data().values().cloned().collect();
        let count = users.len();
        // The store file is kept: the tombstones preserve every UID so members
        // re-created by a later sync get their old UID back
        self.depart_and_expire(&mut store, users, &collections::HashMap::new())
            .await?;
        tracing::info!(
            "Recorded {count} departures and expired their accounts, keeping the tombstones \
             in the store"
        );
        Ok(())
    }

    /// Explicitly purge eligible departed users, excluding current org members.
    #[tracing::instrument(
        name = "Octosync::purge",
        skip(self, args),
        fields(org = %args.octocrab.org)
    )]
    pub async fn purge(&self, args: &PurgeArgs) -> anyhow::Result<()> {
        let octocrab = org_client(&args.octocrab).await?;
        let (org_members, mut store) = tokio::try_join!(
            get_all_org_members(&octocrab, &args.octocrab.org),
            store::UserStore::from_dir(&self.data_dir, self.dry_run),
        )?;
        let org_member_map: collections::HashMap<octocrab::models::UserId, String> =
            collections::HashMap::from_iter(
                org_members.iter().map(|user| (user.id, user.login.clone())),
            );
        self.purge_expired(&mut store, &org_member_map, args.purge_after_days)
            .await
    }
}

/// Separate failed current members, which remain for retry, from actual leavers based on the
/// fetched org member list.
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

/// Whether no stored user appears in the fetched membership.
fn membership_has_no_stored_users(
    stored: &collections::HashMap<octocrab::models::UserId, store::User>,
    org_member_map: &collections::HashMap<octocrab::models::UserId, String>,
) -> bool {
    !stored.is_empty() && !stored.keys().any(|id| org_member_map.contains_key(id))
}

/// Refuse a sync that would expire every previously stored user.
fn would_expire_all_users(
    old_users: &collections::HashMap<octocrab::models::UserId, store::User>,
    leavers: &[&store::User],
) -> bool {
    !leavers.is_empty() && leavers.len() == old_users.len()
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
            .gid(nix::unistd::Gid::from_raw(2000 + id as u32))
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
        use crate::user_manager::{UserManager, backends::testing::TestingUserManager};

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
        pub(super) fn octosync_with(actor: TestingUserManager) -> (Octosync, tempfile::TempDir) {
            let data_dir = tempfile::tempdir().unwrap();
            let octosync = Octosync {
                data_dir: data_dir.path().to_path_buf(),
                dry_run: false,
                user_manager: UserManager::testing(actor),
            };
            (octosync, data_dir)
        }

        /// A dry run must not write the users database: new members would be
        /// persisted with invented mock IDs and tombstone changes would be acted on
        /// by a later real run.
        #[tokio::test]
        async fn dry_run_does_not_write_the_store() {
            let mut actor = TestingUserManager::default();
            actor.expire_account.push_back(Ok(()));
            let data_dir = tempfile::tempdir().unwrap();
            let octosync = Octosync {
                data_dir: data_dir.path().to_path_buf(),
                dry_run: true,
                user_manager: UserManager::testing(actor),
            };
            let mut store = store::UserStore::new(data_dir.path(), true).await.unwrap();

            octosync
                .depart_and_expire(&mut store, vec![user(1, "alice")], &member_map(&[]))
                .await
                .unwrap();

            // The departure exists in memory but nothing was persisted
            assert!(store.departed().contains_key(&octocrab::models::UserId(1)));
            let on_disk = store::UserStore::from_dir(data_dir.path(), false)
                .await
                .unwrap();
            assert!(on_disk.departed().is_empty());
        }

        #[tokio::test]
        async fn process_user_creates_missing_user_and_tolerates_failed_key_fetch() {
            let mut actor = TestingUserManager::default();
            actor.create_user.push_back(Ok(user(1, "alice")));
            actor.sync_supplementary_groups.push_back(Ok(()));
            // No UpdateAuthorizedKeys response is scripted: the key fetch against the
            // unreachable client fails, so the keys must be skipped, not synced
            let (octosync, _data_dir) = octosync_with(actor);
            let empty_store = store::UserStore::new(_data_dir.path(), false)
                .await
                .unwrap();

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
            let mut old_store = store::UserStore::new(_data_dir.path(), false)
                .await
                .unwrap();
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
            let empty_store = store::UserStore::new(_data_dir.path(), false)
                .await
                .unwrap();

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
            let empty_store = store::UserStore::new(_data_dir.path(), false)
                .await
                .unwrap();

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
            let empty_store = store::UserStore::new(_data_dir.path(), false)
                .await
                .unwrap();

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
        /// users) is created with the stored UID and GID and their tombstone is spent.
        #[tokio::test]
        async fn process_members_recreates_a_rejoined_member_with_the_stored_ids() {
            let mut actor = TestingUserManager::default();
            actor.create_user.push_back(Ok(user(1, "alice")));
            actor.sync_supplementary_groups.push_back(Ok(()));
            let received_ids = actor.create_user_ids.clone();
            let (octosync, _data_dir) = octosync_with(actor);
            let mut old_store = store::UserStore::new(_data_dir.path(), false)
                .await
                .unwrap();
            old_store.depart_user(user(1, "alice"), chrono::Utc::now());

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
                *received_ids.lock().unwrap(),
                [crate::user_manager::AccountIds::Stored {
                    uid: nix::unistd::Uid::from_raw(1001),
                    gid: Some(nix::unistd::Gid::from_raw(2001)),
                }]
            );
            assert!(new_store.data().contains_key(&octocrab::models::UserId(1)));
            assert!(new_store.departed().is_empty());
        }

        /// A brand-new member's creation carries the IDs reserved by tombstones, so
        /// the backend never allocates a departed user's UID or GID to them.
        #[tokio::test]
        async fn process_members_reserves_archived_ids_for_a_new_member() {
            let mut actor = TestingUserManager::default();
            actor.create_user.push_back(Ok(user(2, "bob")));
            actor.sync_supplementary_groups.push_back(Ok(()));
            let received_ids = actor.create_user_ids.clone();
            let (octosync, _data_dir) = octosync_with(actor);
            let mut old_store = store::UserStore::new(_data_dir.path(), false)
                .await
                .unwrap();
            old_store.depart_user(user(1, "alice"), chrono::Utc::now());

            octosync
                .process_members(
                    &unreachable_octocrab(),
                    &[author(2, "bob")],
                    &old_store,
                    &groups::GroupAssignments::default(),
                )
                .await
                .unwrap();

            assert_eq!(
                *received_ids.lock().unwrap(),
                [crate::user_manager::AccountIds::Fresh {
                    reserved_uids: [nix::unistd::Uid::from_raw(1001)].into(),
                    reserved_gids: [nix::unistd::Gid::from_raw(2001)].into(),
                }]
            );
        }

        /// A departed member's login claimed by a different person must not adopt the
        /// expired account: no user manager response is scripted, so the test also
        /// proves the guard refuses before any platform operation runs.
        #[tokio::test]
        async fn recycled_login_of_a_departed_member_is_refused() {
            let (octosync, _data_dir) = octosync_with(TestingUserManager::default());
            let mut old_store = store::UserStore::new(_data_dir.path(), false)
                .await
                .unwrap();
            old_store.depart_user(user(1, "alice"), chrono::Utc::now());

            let err = octosync
                .create_user(&author(2, "alice"), &old_store)
                .await
                .unwrap_err();

            assert!(err.to_string().contains("recycled"));
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
            let mut old_store = store::UserStore::new(_data_dir.path(), false)
                .await
                .unwrap();
            old_store.depart_user(user(1, "alice"), chrono::Utc::now());

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
                    .departed()
                    .contains_key(&octocrab::models::UserId(1))
            );
        }

        /// A leaver is tombstoned and the store saved before the account is expired,
        /// so an expiry failure leaves a durable tombstone instead of in-memory
        /// retry state.
        #[tokio::test]
        async fn depart_and_expire_keeps_the_tombstone_when_expiry_fails() {
            let mut actor = TestingUserManager::default();
            actor.expire_account.push_back(Err(anyhow::anyhow!("boom")));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();

            octosync
                .depart_and_expire(&mut store, vec![user(1, "alice")], &member_map(&[]))
                .await
                .unwrap();

            let saved = store::UserStore::from_dir(data_dir.path(), false)
                .await
                .unwrap();
            let tombstone = &saved.departed()[&octocrab::models::UserId(1)];
            assert_eq!(tombstone.name(), "alice");
            // No completion timestamp, so the next sync retries the teardown
            assert!(tombstone.expired_at().is_none());
            assert!(saved.data().is_empty());
        }

        /// A tombstone whose teardown never completed is retried by a later sync, so a
        /// failed or interrupted expiry converges without in-memory retry state.
        #[tokio::test]
        async fn depart_and_expire_retries_an_unfinished_tombstone() {
            let mut actor = TestingUserManager::default();
            actor.expire_account.push_back(Ok(()));
            let expired_users = actor.expired_users.clone();
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), chrono::Utc::now());

            // No leavers this sync; the unfinished departure alone drives the account removal.
            octosync
                .depart_and_expire(&mut store, vec![], &member_map(&[]))
                .await
                .unwrap();

            assert_eq!(*expired_users.lock().unwrap(), ["alice"]);
            assert!(
                store.departed()[&octocrab::models::UserId(1)]
                    .expired_at()
                    .is_some()
            );
        }

        #[tokio::test]
        async fn depart_and_expire_skips_unfinished_departure_for_current_member() {
            let actor = TestingUserManager::default();
            let expired_users = actor.expired_users.clone();
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            let alice = user(1, "alice");
            store.depart_user(alice.clone(), chrono::Utc::now());

            octosync
                .depart_and_expire(&mut store, vec![], &member_map(&[alice]))
                .await
                .unwrap();

            assert!(expired_users.lock().unwrap().is_empty());
            assert!(store.departed().contains_key(&octocrab::models::UserId(1)));
        }

        /// A tombstone whose teardown completed is left alone for the rest of the
        /// retention period, instead of paying the full teardown on every sync.
        #[tokio::test]
        async fn depart_and_expire_skips_a_finished_tombstone() {
            // No scripted response: reaching the actor would fail with "No scripted
            // response left", which the loop only logs, so the assertion below is what
            // proves the message is never sent
            let actor = TestingUserManager::default();
            let expired_users = actor.expired_users.clone();
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            let now = chrono::Utc::now();
            store.depart_user(user(1, "alice"), now);
            store.mark_expired(&octocrab::models::UserId(1), now);

            octosync
                .depart_and_expire(&mut store, vec![], &member_map(&[]))
                .await
                .unwrap();

            assert!(expired_users.lock().unwrap().is_empty());
        }

        /// The `delete` command tombstones every user instead of removing the store
        /// file, so the UID memory survives a full wipe.
        #[tokio::test]
        async fn delete_command_keeps_the_store_file_with_tombstones() {
            let mut actor = TestingUserManager::default();
            actor.expire_account.push_back(Ok(()));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store
                .data_mut()
                .insert(octocrab::models::UserId(1), user(1, "alice"));
            store.save().await.unwrap();

            octosync.delete().await.unwrap();

            let saved = store::UserStore::from_dir(data_dir.path(), false)
                .await
                .unwrap();
            assert!(saved.data().is_empty());
            assert_eq!(
                saved.departed()[&octocrab::models::UserId(1)].name(),
                "alice"
            );
        }
    }

    mod purge {
        use super::orchestration::octosync_with;
        use super::*;
        use crate::user_manager::{PurgeOutcome, backends::testing::TestingUserManager};

        const RETENTION_DAYS: u32 = 180;

        /// A departure timestamp safely past the retention period
        fn old_departure() -> chrono::DateTime<chrono::Utc> {
            chrono::Utc::now() - chrono::Duration::days(RETENTION_DAYS as i64 + 30)
        }

        fn unrelated_member() -> store::User {
            user(99, "zoe")
        }

        #[tokio::test]
        async fn purges_a_tombstone_older_than_the_retention_period() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Ok(PurgeOutcome::Purged));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());

            octosync
                .purge_expired(
                    &mut store,
                    &member_map(&[unrelated_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            assert!(store.departed().is_empty());
            // The tombstone survives the purge and is saved, so the UID stays
            // reserved for a rejoin even after the purge
            let saved = store::UserStore::from_dir(data_dir.path(), false)
                .await
                .unwrap();
            let purged = &saved.purged()[&octocrab::models::UserId(1)];
            assert_eq!(purged.name(), "alice");
        }

        /// A tombstone younger than the retention period is not purged: the scripted
        /// Purged response would move it if the message were sent.
        #[tokio::test]
        async fn young_tombstone_is_not_purged() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Ok(PurgeOutcome::Purged));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), chrono::Utc::now());

            octosync
                .purge_expired(
                    &mut store,
                    &member_map(&[unrelated_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            assert!(store.departed().contains_key(&octocrab::models::UserId(1)));
            assert!(store.purged().is_empty());
        }

        /// A member present in the fetched member list is never purged, however old
        /// their tombstone is.
        #[tokio::test]
        async fn current_org_member_is_not_purged() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Ok(PurgeOutcome::Purged));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            let alice = user(1, "alice");
            store.depart_user(alice.clone(), old_departure());

            octosync
                .purge_expired(&mut store, &member_map(&[alice]), RETENTION_DAYS)
                .await
                .unwrap();

            assert!(store.departed().contains_key(&octocrab::models::UserId(1)));
            assert!(store.purged().is_empty());
        }

        /// The account-side clock disagrees with the tombstone: the backend reports
        /// the account as not expired long enough, the tombstone stays departed.
        #[tokio::test]
        async fn account_side_disagreement_keeps_the_tombstone_departed() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Ok(PurgeOutcome::NotExpired));
            actor.purge_account.push_back(Ok(PurgeOutcome::NoAccount));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());
            store.depart_user(user(2, "bob"), old_departure());

            octosync
                .purge_expired(
                    &mut store,
                    &member_map(&[unrelated_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            assert_eq!(store.departed().len(), 2);
            assert!(store.purged().is_empty());
        }

        /// A failed purge keeps the tombstone departed, so it is retried on the next
        /// sync.
        #[tokio::test]
        async fn failed_purge_keeps_the_tombstone_departed() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Err(anyhow::anyhow!("boom")));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());

            octosync
                .purge_expired(
                    &mut store,
                    &member_map(&[unrelated_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            assert!(store.departed().contains_key(&octocrab::models::UserId(1)));
            assert!(store.purged().is_empty());
        }

        #[tokio::test]
        async fn empty_member_list_refuses_the_purge() {
            let (octosync, data_dir) = octosync_with(TestingUserManager::default());
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());

            let result = octosync
                .purge_expired(&mut store, &member_map(&[]), RETENTION_DAYS)
                .await;

            assert!(result.is_err());
            assert!(store.departed().contains_key(&octocrab::models::UserId(1)));
        }

        /// A purged tombstone is left alone by the expiry reconciliation: the two
        /// rules must not fight over an entry.
        #[tokio::test]
        async fn purged_tombstone_is_not_re_expired() {
            let actor = TestingUserManager::default();
            let expired_users = actor.expired_users.clone();
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());
            store.mark_purged(&octocrab::models::UserId(1), chrono::Utc::now());

            octosync
                .depart_and_expire(&mut store, vec![], &member_map(&[]))
                .await
                .unwrap();

            assert!(expired_users.lock().unwrap().is_empty());
        }
    }

    mod partition_stale_users {
        use super::*;

        /// Regression test for the 2026-08-12 incident: every member is still in the
        /// org, but all of them failed processing, so the new store is empty.
        /// No user may be expired; all must be retried.
        #[test]
        fn all_members_failed_processing_expires_nobody() {
            let users = [user(1, "a"), user(2, "b"), user(3, "c")];
            let old = user_map(&users);
            let new = user_map(&[]);
            let members = member_map(&users);

            let (retry, leavers) = partition_stale_users(&old, &new, &members);

            assert_eq!(retry.len(), 3);
            assert!(leavers.is_empty());
        }

        #[test]
        fn user_absent_from_member_list_is_expired() {
            let remaining = user(1, "a");
            let left = user(2, "b");
            let old = user_map(&[remaining.clone(), left.clone()]);
            let new = user_map(&[remaining]);
            let members = member_map(std::slice::from_ref(&old[&octocrab::models::UserId(1)]));

            let (retry, leavers) = partition_stale_users(&old, &new, &members);

            assert!(retry.is_empty());
            assert_eq!(leavers, vec![&left]);
        }

        #[test]
        fn successfully_processed_user_is_neither_retried_nor_expired() {
            let processed = user(1, "a");
            let old = user_map(std::slice::from_ref(&processed));
            let new = user_map(std::slice::from_ref(&processed));
            let members = member_map(&[processed]);

            let (retry, leavers) = partition_stale_users(&old, &new, &members);

            assert!(retry.is_empty());
            assert!(leavers.is_empty());
        }

        #[test]
        fn failed_member_is_retried_while_removed_member_is_expired() {
            let processed = user(1, "a");
            let failed = user(2, "b");
            let left = user(3, "c");
            let old = user_map(&[processed.clone(), failed.clone(), left.clone()]);
            let new = user_map(std::slice::from_ref(&processed));
            let members = member_map(&[processed, failed.clone()]);

            let (retry, leavers) = partition_stale_users(&old, &new, &members);

            assert_eq!(retry, vec![&failed]);
            assert_eq!(leavers, vec![&left]);
        }

        /// The successful-but-empty member list: `sync()` bails on this before
        /// processing, but the circuit breaker must still trip as defense in depth.
        #[test]
        fn empty_member_list_puts_all_users_in_leaver_bucket_and_trips_guard() {
            let users = [user(1, "a"), user(2, "b")];
            let old = user_map(&users);
            let new = user_map(&[]);
            let members = member_map(&[]);

            let (retry, leavers) = partition_stale_users(&old, &new, &members);

            assert!(retry.is_empty());
            assert_eq!(leavers.len(), 2);
            assert!(would_expire_all_users(&old, &leavers));
        }

        #[test]
        fn new_member_in_new_store_only_is_untouched() {
            let joined = user(1, "a");
            let old = user_map(&[]);
            let new = user_map(std::slice::from_ref(&joined));
            let members = member_map(&[joined]);

            let (retry, leavers) = partition_stale_users(&old, &new, &members);

            assert!(retry.is_empty());
            assert!(leavers.is_empty());
        }
    }

    mod membership_has_no_stored_users {
        use super::*;

        #[test]
        fn empty_member_list_contains_no_stored_users() {
            let stored = user_map(&[user(1, "a"), user(2, "b")]);

            assert!(membership_has_no_stored_users(&stored, &member_map(&[])));
        }

        #[test]
        fn membership_with_only_unrelated_users_contains_no_stored_users() {
            let stored = user_map(&[user(1, "a")]);

            assert!(membership_has_no_stored_users(
                &stored,
                &member_map(&[user(2, "b")])
            ));
        }

        #[test]
        fn membership_with_a_stored_user_is_accepted() {
            let kept = user(1, "a");
            let stored = user_map(&[kept.clone(), user(2, "b")]);

            assert!(!membership_has_no_stored_users(
                &stored,
                &member_map(&[kept])
            ));
        }

        #[test]
        fn empty_store_does_not_trigger_guard() {
            assert!(!membership_has_no_stored_users(
                &user_map(&[]),
                &member_map(&[])
            ));
        }
    }

    mod expires_entire_store {
        use super::*;

        #[test]
        fn expiring_every_stored_user_trips_the_guard() {
            let users = [user(1, "a"), user(2, "b")];
            let old = user_map(&users);
            let leavers: Vec<&store::User> = old.values().collect();

            assert!(would_expire_all_users(&old, &leavers));
        }

        #[test]
        fn expiring_a_subset_is_allowed() {
            let users = [user(1, "a"), user(2, "b")];
            let old = user_map(&users);
            let leavers = vec![&old[&octocrab::models::UserId(1)]];

            assert!(!would_expire_all_users(&old, &leavers));
        }

        #[test]
        fn nothing_to_expire_is_allowed() {
            let old = user_map(&[user(1, "a")]);

            assert!(!would_expire_all_users(&old, &[]));
        }

        /// A single-user store where that user really left still trips the guard;
        /// the explicit delete command is the intentional path for this case.
        #[test]
        fn expiring_the_only_stored_user_trips_the_guard() {
            let only = user(1, "a");
            let old = user_map(std::slice::from_ref(&only));
            let leavers: Vec<&store::User> = old.values().collect();

            assert!(would_expire_all_users(&old, &leavers));
        }
    }
}
