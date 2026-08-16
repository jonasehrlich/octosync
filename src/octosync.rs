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

    /// Create the platform user, reusing retained IDs when the member rejoins.
    ///
    /// A rejoin whose stored ID is taken fails and is retried on the next sync.
    ///
    /// Falling back to fresh IDs would break ownership of the member's existing files.
    /// A brand-new member must not receive an ID retained for any departed member,
    /// whether their account is disabled or already deleted.
    async fn create_user(
        &self,
        gh_user: &octocrab::models::Author,
        store: &store::UserStore,
    ) -> anyhow::Result<store::User> {
        // Refuse a recycled login for a different GitHub account, rather than expose the departed
        // member's account. This is something that needs to be solved by an operator.
        if let Some(departure) = store
            .departed()
            .values()
            .find(|departure| departure.name() == gh_user.login)
            && departure.id() != gh_user.id
        {
            anyhow::bail!(
                "Login '{}' belonged to the departed member with GitHub ID {}, but the joining \
                 member has GitHub ID {}: the login was recycled by a different person, refusing \
                 to create the user",
                gh_user.login,
                departure.id(),
                gh_user.id
            );
        }

        // Departed members reuse their account. Members whose account was deleted reuse only IDs.
        let stored_ids = store
            .departed()
            .get(&gh_user.id)
            .map(|departure| (departure.uid(), departure.gid()))
            .or_else(|| {
                store
                    .deleted()
                    .get(&gh_user.id)
                    .map(|deleted| (deleted.uid(), deleted.gid()))
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
                    .chain(store.deleted().values().map(store::DeletedUser::uid))
                    .collect(),
                reserved_gids: store
                    .departed()
                    .values()
                    .filter_map(store::DepartedUser::gid)
                    .chain(store.deleted().values().filter_map(store::DeletedUser::gid))
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

        // Validate the fetched membership before changing the system. The later
        // mass-departure safeguard only prevents account disablement. By then a bad
        // member list could already have renamed accounts, rewritten keys or changed
        // supplementary groups.
        if membership_has_no_stored_users(old_store.data(), &org_member_map) {
            anyhow::bail!(
                "Refusing to sync: none of the {} stored users is in the fetched member list \
                 of org '{}' ({} members). Nothing was changed on the system. Run the 'delete' \
                 command to disable all users intentionally.",
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

        if would_disable_all_users(old_store.data(), &leavers) {
            for user in leavers {
                new_store.data_mut().insert(user.id(), user.clone());
            }
            new_store.save().await?;
            anyhow::bail!(
                "Refusing to disable all {} stored users in a single sync. None of them is in \
                 the fetched member list of org '{}' ({} members). All users are kept in \
                 the store. Run the 'delete' command to disable them intentionally.",
                old_store.data().len(),
                args.octocrab.org,
                org_members.len()
            );
        }

        let leavers: Vec<store::User> = leavers.into_iter().cloned().collect();
        self.depart_and_disable(&mut new_store, leavers, &org_member_map)
            .await?;

        self.purge_disabled_accounts(
            &mut new_store,
            &args.octocrab.org,
            &org_member_map,
            args.purge_after_days,
        )
        .await
    }

    /// Permanently delete departed accounts once both the recorded departure and the
    /// account's shadow expiry exceed the retention period. Current members are never deleted.
    async fn purge_disabled_accounts(
        &self,
        store: &mut store::UserStore,
        org: &str,
        org_member_map: &collections::HashMap<octocrab::models::UserId, String>,
        purge_after_days: u32,
    ) -> anyhow::Result<()> {
        // Every departure record appears eligible when the fetched membership is empty.
        // An organization where the app is installed always has at least one member, so
        // an empty result indicates a scoping, permission or API failure. This also
        // protects a store with no active users left to exercise the sync-time guard.
        if org_member_map.is_empty() {
            anyhow::bail!(
                "Refusing to purge: GitHub org '{org}' returned no members, which would make \
                 every departure record eligible. No accounts were deleted.",
            );
        }

        let now = chrono::Utc::now();
        let cutoff = now - chrono::Duration::days(purge_after_days.into());

        let candidates: Vec<store::User> = store
            .departed()
            .values()
            .filter(|departure| {
                departure.departed_at() <= cutoff && !org_member_map.contains_key(&departure.id())
            })
            .map(store::User::from)
            .collect();

        for user in candidates {
            // A marker from an earlier run proves that octosync started deleting this
            // account and stopped before it could update the departure record.
            let purge_was_interrupted = store
                .departed()
                .get(&user.id())
                .is_some_and(|departure| departure.deletion_started_at().is_some());

            // Persist the intent before `userdel` can make the account disappear, so an
            // interruption between the two is not mistaken for an operator's removal.
            if !purge_was_interrupted {
                store.mark_deletion_started(&user.id(), now);
                store.save().await?;
            }

            match self.user_manager.purge_user(&user, cutoff).await {
                Ok(user_manager::PurgeOutcome::Deleted) => {
                    tracing::info!(
                        user = %user.name(),
                        uid = user.uid().as_raw(),
                        "Permanently deleted the disabled account after the retention period"
                    );
                    store.mark_deleted(&user.id(), now);
                }
                // A missing account is only considered deleted by octosync when the
                // persisted marker proves that an earlier purge started it. Otherwise an
                // operator may have deleted the account independently.
                Ok(user_manager::PurgeOutcome::NoAccount) if purge_was_interrupted => {
                    tracing::info!(
                        user = %user.name(),
                        "Account was deleted during an interrupted purge. Completing the store update"
                    );
                    store.mark_deleted(&user.id(), now);
                }
                // Keep the departure record until octosync confirms account deletion.
                Ok(user_manager::PurgeOutcome::NoAccount) => {
                    tracing::warn!(
                        "No account for departed user '{}'. Keeping the departure record",
                        user.name()
                    );
                    store.clear_deletion_started(&user.id());
                }
                Ok(user_manager::PurgeOutcome::NotDisabledLongEnough) => {
                    tracing::warn!(
                        "Account of '{}' was not disabled before the retention cutoff. \
                         Keeping the account and departure record",
                        user.name()
                    );
                    store.clear_deletion_started(&user.id());
                }
                // Keep the marker because `userdel` can remove the account before
                // reporting an error or timing out. The next run checks the account and
                // can complete the store update if it is already gone.
                Err(e) => {
                    tracing::error!("Failed to purge the account of '{}': {:?}", user.name(), e);
                }
            }
            // Persist this result before attempting another account.
            store.save().await?;
        }
        store.save().await
    }

    /// Persist new departures, then retry every unfinished account disablement.
    ///
    /// A departure record receives its completion timestamp only after
    /// [`user_manager::DisableAccount`] disables the account and removes its access. A
    /// failed or interrupted operation is therefore retried by the next sync.
    async fn depart_and_disable(
        &self,
        store: &mut store::UserStore,
        leavers: Vec<store::User>,
        org_member_map: &collections::HashMap<octocrab::models::UserId, String>,
    ) -> anyhow::Result<()> {
        let departed_at = chrono::Utc::now();
        for user in leavers {
            tracing::info!(user = %user.name(), "Recording departure and disabling the account");
            store.depart_user(user, departed_at);
        }
        store.save().await?;

        let pending: Vec<store::User> = store
            .departed()
            .values()
            .filter(|departed| {
                // A rejoining member whose account was already restored but whose group
                // or key sync then failed is missing from the new store, so their
                // unfinished departure survives this sync. Disabling the account again
                // here would undo the reactivation, kill the member's
                // processes and strip their groups over a transient failure.
                departed.disabled_at().is_none() && !org_member_map.contains_key(&departed.id())
            })
            .map(store::User::from)
            .collect();

        // A failed disablement keeps its empty completion timestamp for the next sync.
        for user in pending {
            match self.user_manager.disable_user(&user).await {
                Ok(()) => store.mark_disabled(&user.id(), chrono::Utc::now()),
                Err(e) => {
                    tracing::error!(
                        "Failed to disable the account of '{}': {:?}",
                        user.name(),
                        e
                    )
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
        *new_store.deleted_mut() = store.deleted().clone();
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
        // Successful processing drops retained departure records for a rejoining member.
        new_store.prune_rejoined();
        Ok(new_store)
    }

    #[tracing::instrument(name = "Octosync::delete", skip(self))]
    pub async fn delete(&self) -> anyhow::Result<()> {
        let mut store = store::UserStore::from_dir(&self.data_dir, self.dry_run).await?;
        let users: Vec<store::User> = store.data().values().cloned().collect();
        let count = users.len();
        // Despite the command name, this disables accounts rather than deleting them.
        // The store keeps each departure and its IDs so a later rejoin can reactivate
        // the same account. An empty membership is intentional here, so every active
        // user is processed as departed.
        self.depart_and_disable(&mut store, users, &collections::HashMap::new())
            .await?;
        tracing::info!(
            "Recorded {count} departures and disabled their accounts. The accounts and homes remain"
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
        self.purge_disabled_accounts(
            &mut store,
            &args.octocrab.org,
            &org_member_map,
            args.purge_after_days,
        )
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
///
/// This is the same failure [`would_disable_all_users`] catches, seen before any platform
/// operation runs: a mis-scoped installation or a partially failed member list makes
/// every managed account look departed. An empty store needs no guard.
fn membership_has_no_stored_users(
    stored: &collections::HashMap<octocrab::models::UserId, store::User>,
    org_member_map: &collections::HashMap<octocrab::models::UserId, String>,
) -> bool {
    !stored.is_empty() && !stored.keys().any(|id| org_member_map.contains_key(id))
}

/// Refuse a sync that would disable every previously stored account.
fn would_disable_all_users(
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

        /// A dry run must not write the users database. New members would otherwise be
        /// persisted with invented mock IDs, and a later real run would act on previewed
        /// departure changes.
        #[tokio::test]
        async fn dry_run_does_not_write_the_store() {
            let mut actor = TestingUserManager::default();
            actor.disable_account.push_back(Ok(()));
            let data_dir = tempfile::tempdir().unwrap();
            let octosync = Octosync {
                data_dir: data_dir.path().to_path_buf(),
                dry_run: true,
                user_manager: UserManager::testing(actor),
            };
            let mut store = store::UserStore::new(data_dir.path(), true).await.unwrap();

            octosync
                .depart_and_disable(&mut store, vec![user(1, "alice")], &member_map(&[]))
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

        /// A member whose processing fails must be left out of the new store.
        /// [`partition_stale_users`] then keeps the existing active record for a retry
        /// instead of treating the member as departed.
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

        /// A rejoining member has a departure record but no active record. The account
        /// is restored with its retained UID and GID, then the departure record is dropped.
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

        /// A brand-new member's creation carries IDs retained for departed members, so
        /// the backend never assigns one of those UIDs or GIDs to the new account.
        #[tokio::test]
        async fn process_members_reserves_departed_ids_for_a_new_member() {
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
        /// disabled account. No user manager response is scripted, which also proves
        /// the guard refuses before any platform operation runs.
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

        /// Departure records survive member processing. A failed account restoration
        /// leaves the record in place for a retry on the next sync.
        #[tokio::test]
        async fn process_members_keeps_the_departure_when_restoration_fails() {
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

        /// A departure is saved before the account is disabled. A failure therefore
        /// leaves a durable record for the next sync instead of relying on in-memory
        /// retry state.
        #[tokio::test]
        async fn depart_and_disable_keeps_the_record_when_disabling_fails() {
            let mut actor = TestingUserManager::default();
            actor
                .disable_account
                .push_back(Err(anyhow::anyhow!("boom")));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();

            octosync
                .depart_and_disable(&mut store, vec![user(1, "alice")], &member_map(&[]))
                .await
                .unwrap();

            let saved = store::UserStore::from_dir(data_dir.path(), false)
                .await
                .unwrap();
            let departure = &saved.departed()[&octocrab::models::UserId(1)];
            assert_eq!(departure.name(), "alice");
            // No completion timestamp means the next sync retries the disablement.
            assert!(departure.disabled_at().is_none());
            assert!(saved.data().is_empty());
        }

        /// A departure whose disablement never completed is retried by a later sync.
        /// This lets a failed or interrupted operation converge without in-memory state.
        #[tokio::test]
        async fn depart_and_disable_retries_an_unfinished_departure() {
            let mut actor = TestingUserManager::default();
            actor.disable_account.push_back(Ok(()));
            let disabled_users = actor.disabled_users.clone();
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), chrono::Utc::now());

            // No members left in this sync. The unfinished departure alone drives disablement.
            octosync
                .depart_and_disable(&mut store, vec![], &member_map(&[]))
                .await
                .unwrap();

            assert_eq!(*disabled_users.lock().unwrap(), ["alice"]);
            assert!(
                store.departed()[&octocrab::models::UserId(1)]
                    .disabled_at()
                    .is_some()
            );
        }

        /// A rejoining member whose account was restored but whose group or key sync
        /// then failed keeps their unfinished departure for this sync. Repeating account
        /// disablement would undo the account restoration.
        #[tokio::test]
        async fn depart_and_disable_skips_unfinished_departure_for_current_member() {
            // No scripted response is provided. The assertion below proves the message
            // is never sent.
            let actor = TestingUserManager::default();
            let disabled_users = actor.disabled_users.clone();
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            let alice = user(1, "alice");
            store.depart_user(alice.clone(), chrono::Utc::now());

            octosync
                .depart_and_disable(&mut store, vec![], &member_map(&[alice]))
                .await
                .unwrap();

            assert!(disabled_users.lock().unwrap().is_empty());
            // The departure remains unfinished for a later successful sync to remove.
            assert!(
                store.departed()[&octocrab::models::UserId(1)]
                    .disabled_at()
                    .is_none()
            );
        }

        /// A completed disablement is not repeated during the retention period.
        #[tokio::test]
        async fn depart_and_disable_skips_a_finished_departure() {
            // No scripted response is provided. The assertion below proves the message
            // is never sent.
            let actor = TestingUserManager::default();
            let disabled_users = actor.disabled_users.clone();
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            let now = chrono::Utc::now();
            store.depart_user(user(1, "alice"), now);
            store.mark_disabled(&octocrab::models::UserId(1), now);

            octosync
                .depart_and_disable(&mut store, vec![], &member_map(&[]))
                .await
                .unwrap();

            assert!(disabled_users.lock().unwrap().is_empty());
        }

        /// The `delete` command disables every account and retains each departure record,
        /// so UIDs remain available for later account restoration.
        #[tokio::test]
        async fn delete_command_keeps_departure_records() {
            let mut actor = TestingUserManager::default();
            actor.disable_account.push_back(Ok(()));
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
        const ORG: &str = "acme";

        /// A departure timestamp safely past the retention period
        fn old_departure() -> chrono::DateTime<chrono::Utc> {
            chrono::Utc::now() - chrono::Duration::days(RETENTION_DAYS as i64 + 30)
        }

        /// A current member unrelated to the departure records under test. The purge
        /// refuses an empty member list, so a different member represents a nonmember.
        fn other_member() -> store::User {
            user(99, "zoe")
        }

        #[tokio::test]
        async fn deletes_an_account_past_the_retention_period() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Ok(PurgeOutcome::Deleted));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());

            octosync
                .purge_disabled_accounts(
                    &mut store,
                    ORG,
                    &member_map(&[other_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            assert!(store.departed().is_empty());
            // The retained record keeps the UID available for a later rejoin.
            let saved = store::UserStore::from_dir(data_dir.path(), false)
                .await
                .unwrap();
            let deleted_account = &saved.deleted()[&octocrab::models::UserId(1)];
            assert_eq!(deleted_account.name(), "alice");
        }

        /// A recent departure is not eligible for account deletion. The scripted
        /// response would move its record if the purge message were sent.
        #[tokio::test]
        async fn recent_departure_is_not_deleted() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Ok(PurgeOutcome::Deleted));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), chrono::Utc::now());

            octosync
                .purge_disabled_accounts(
                    &mut store,
                    ORG,
                    &member_map(&[other_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            assert!(store.departed().contains_key(&octocrab::models::UserId(1)));
            assert!(store.deleted().is_empty());
        }

        /// A current member's account is never deleted, regardless of the retained
        /// departure date.
        #[tokio::test]
        async fn current_org_member_is_not_deleted() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Ok(PurgeOutcome::Deleted));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            let alice = user(1, "alice");
            store.depart_user(alice.clone(), old_departure());

            octosync
                .purge_disabled_accounts(&mut store, ORG, &member_map(&[alice]), RETENTION_DAYS)
                .await
                .unwrap();

            assert!(store.departed().contains_key(&octocrab::models::UserId(1)));
            assert!(store.deleted().is_empty());
        }

        /// The departure date and shadow expiry disagree. The backend reports that the
        /// account was not disabled before the cutoff, so the departure record remains.
        #[tokio::test]
        async fn account_side_disagreement_keeps_the_departure() {
            let mut actor = TestingUserManager::default();
            actor
                .purge_account
                .push_back(Ok(PurgeOutcome::NotDisabledLongEnough));
            actor.purge_account.push_back(Ok(PurgeOutcome::NoAccount));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());
            store.depart_user(user(2, "bob"), old_departure());

            octosync
                .purge_disabled_accounts(
                    &mut store,
                    ORG,
                    &member_map(&[other_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            assert_eq!(store.departed().len(), 2);
            assert!(store.deleted().is_empty());
        }

        /// A failed purge keeps the departure record for a later retry.
        #[tokio::test]
        async fn failed_purge_keeps_the_departure() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Err(anyhow::anyhow!("boom")));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());

            octosync
                .purge_disabled_accounts(
                    &mut store,
                    ORG,
                    &member_map(&[other_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            assert!(store.departed().contains_key(&octocrab::models::UserId(1)));
            assert!(store.deleted().is_empty());
        }

        /// An empty member list makes every departure record look eligible, so the purge
        /// refuses it. No user manager response is provided, which also proves no account
        /// was deleted before the guard ran.
        #[tokio::test]
        async fn empty_member_list_refuses_the_purge() {
            let (octosync, data_dir) = octosync_with(TestingUserManager::default());
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());

            let err = octosync
                .purge_disabled_accounts(&mut store, ORG, &member_map(&[]), RETENTION_DAYS)
                .await
                .unwrap_err();

            assert!(err.to_string().contains("returned no members"));
            assert!(store.departed().contains_key(&octocrab::models::UserId(1)));
        }

        /// A store containing only departure records has no active user left to detect a
        /// broken member list, so the guard must not depend on active users.
        #[tokio::test]
        async fn empty_member_list_is_refused_with_departures_only() {
            let (octosync, data_dir) = octosync_with(TestingUserManager::default());
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());
            store.depart_user(user(2, "bob"), old_departure());

            assert!(store.data().is_empty());
            assert!(
                octosync
                    .purge_disabled_accounts(&mut store, ORG, &member_map(&[]), RETENTION_DAYS)
                    .await
                    .is_err()
            );
            assert_eq!(store.departed().len(), 2);
        }

        /// A failure may happen after `userdel` removed the account, so the persisted
        /// marker is retained for the next run to resolve.
        #[tokio::test]
        async fn failed_purge_keeps_the_pending_marker() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Err(anyhow::anyhow!("boom")));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());

            octosync
                .purge_disabled_accounts(
                    &mut store,
                    ORG,
                    &member_map(&[other_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            let saved = store::UserStore::from_dir(data_dir.path(), false)
                .await
                .unwrap();
            assert!(
                saved.departed()[&octocrab::models::UserId(1)]
                    .deletion_started_at()
                    .is_some()
            );
        }

        /// The account is gone and the departure record shows an interrupted purge.
        /// The next run completes octosync's unfinished store update.
        #[tokio::test]
        async fn interrupted_purge_is_completed_on_the_next_run() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Ok(PurgeOutcome::NoAccount));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());
            store.mark_deletion_started(&octocrab::models::UserId(1), old_departure());

            octosync
                .purge_disabled_accounts(
                    &mut store,
                    ORG,
                    &member_map(&[other_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            assert!(store.departed().is_empty());
            assert_eq!(
                store.deleted()[&octocrab::models::UserId(1)].name(),
                "alice"
            );
        }

        /// Without a marker, a missing account may have been deleted independently.
        /// Its departure record must remain.
        #[tokio::test]
        async fn account_deleted_by_an_operator_keeps_its_departure() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Ok(PurgeOutcome::NoAccount));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());

            octosync
                .purge_disabled_accounts(
                    &mut store,
                    ORG,
                    &member_map(&[other_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            assert!(store.departed().contains_key(&octocrab::models::UserId(1)));
            assert!(store.deleted().is_empty());
        }

        /// Each purge result is persisted before the next `userdel` starts.
        #[tokio::test]
        async fn each_purge_is_saved_before_the_next_one_starts() {
            let mut actor = TestingUserManager::default();
            actor.purge_account.push_back(Ok(PurgeOutcome::Deleted));
            actor.purge_account.push_back(Err(anyhow::anyhow!("boom")));
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());
            store.depart_user(user(2, "bob"), old_departure());

            octosync
                .purge_disabled_accounts(
                    &mut store,
                    ORG,
                    &member_map(&[other_member()]),
                    RETENTION_DAYS,
                )
                .await
                .unwrap();

            // One account was deleted and one purge failed. Both outcomes are saved.
            // The processing order is not fixed, so only the counts are asserted.
            let saved = store::UserStore::from_dir(data_dir.path(), false)
                .await
                .unwrap();
            assert_eq!(saved.deleted().len(), 1);
            assert_eq!(saved.departed().len(), 1);
        }

        /// A record for a permanently deleted account is not processed by account
        /// disablement again.
        #[tokio::test]
        async fn deleted_record_is_not_disabled_again() {
            let actor = TestingUserManager::default();
            let disabled_users = actor.disabled_users.clone();
            let (octosync, data_dir) = octosync_with(actor);
            let mut store = store::UserStore::new(data_dir.path(), false).await.unwrap();
            store.depart_user(user(1, "alice"), old_departure());
            store.mark_deleted(&octocrab::models::UserId(1), chrono::Utc::now());

            octosync
                .depart_and_disable(&mut store, vec![], &member_map(&[]))
                .await
                .unwrap();

            assert!(disabled_users.lock().unwrap().is_empty());
        }
    }

    mod partition_stale_users {
        use super::*;

        /// Regression test for the 2026-08-12 incident. Every member is still in the
        /// organization, but all processing failed and the new store is empty. No account
        /// may be disabled. Every member must be retried.
        #[test]
        fn all_members_failed_processing_disables_nobody() {
            let users = [user(1, "a"), user(2, "b"), user(3, "c")];
            let old = user_map(&users);
            let new = user_map(&[]);
            let members = member_map(&users);

            let (retry, leavers) = partition_stale_users(&old, &new, &members);

            assert_eq!(retry.len(), 3);
            assert!(leavers.is_empty());
        }

        #[test]
        fn user_absent_from_member_list_is_a_leaver() {
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
        fn successfully_processed_user_is_neither_retried_nor_departed() {
            let processed = user(1, "a");
            let old = user_map(std::slice::from_ref(&processed));
            let new = user_map(std::slice::from_ref(&processed));
            let members = member_map(&[processed]);

            let (retry, leavers) = partition_stale_users(&old, &new, &members);

            assert!(retry.is_empty());
            assert!(leavers.is_empty());
        }

        #[test]
        fn failed_member_is_retried_while_absent_member_is_a_leaver() {
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

        /// `sync()` rejects an empty member list before processing. The later
        /// mass-disablement safeguard remains as a second check.
        #[test]
        fn empty_member_list_puts_all_users_in_leaver_bucket_and_trips_guard() {
            let users = [user(1, "a"), user(2, "b")];
            let old = user_map(&users);
            let new = user_map(&[]);
            let members = member_map(&[]);

            let (retry, leavers) = partition_stale_users(&old, &new, &members);

            assert!(retry.is_empty());
            assert_eq!(leavers.len(), 2);
            assert!(would_disable_all_users(&old, &leavers));
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

        /// The 2026-08-12 failure seen one step earlier. An empty member list must stop
        /// the sync before any group is created or user is processed.
        #[test]
        fn empty_member_list_contains_no_stored_users() {
            let stored = user_map(&[user(1, "a"), user(2, "b")]);

            assert!(membership_has_no_stored_users(&stored, &member_map(&[])));
        }

        /// A mis-scoped installation returns members, just not the managed ones.
        #[test]
        fn membership_with_only_unrelated_users_contains_no_stored_users() {
            let stored = user_map(&[user(1, "a")]);

            assert!(membership_has_no_stored_users(
                &stored,
                &member_map(&[user(2, "b")])
            ));
        }

        /// One surviving member is enough to trust the list. The users missing from it
        /// are real leavers, and `partition_stale_users` still separates them from
        /// members whose processing merely failed.
        #[test]
        fn membership_with_a_stored_user_is_accepted() {
            let kept = user(1, "a");
            let stored = user_map(&[kept.clone(), user(2, "b")]);

            assert!(!membership_has_no_stored_users(
                &stored,
                &member_map(&[kept])
            ));
        }

        /// A first run has no stored users to protect.
        #[test]
        fn empty_store_does_not_trigger_guard() {
            assert!(!membership_has_no_stored_users(
                &user_map(&[]),
                &member_map(&[])
            ));
        }
    }

    mod disables_entire_store {
        use super::*;

        #[test]
        fn disabling_every_stored_user_trips_the_guard() {
            let users = [user(1, "a"), user(2, "b")];
            let old = user_map(&users);
            let leavers: Vec<&store::User> = old.values().collect();

            assert!(would_disable_all_users(&old, &leavers));
        }

        #[test]
        fn disabling_a_subset_is_allowed() {
            let users = [user(1, "a"), user(2, "b")];
            let old = user_map(&users);
            let leavers = vec![&old[&octocrab::models::UserId(1)]];

            assert!(!would_disable_all_users(&old, &leavers));
        }

        #[test]
        fn nothing_to_disable_is_allowed() {
            let old = user_map(&[user(1, "a")]);

            assert!(!would_disable_all_users(&old, &[]));
        }

        /// A single-user store where that user really left still triggers the safeguard.
        /// The explicit `delete` command is the intentional path for this case.
        #[test]
        fn disabling_the_only_stored_user_trips_the_guard() {
            let only = user(1, "a");
            let old = user_map(std::slice::from_ref(&only));
            let leavers: Vec<&store::User> = old.values().collect();

            assert!(would_disable_all_users(&old, &leavers));
        }
    }
}
