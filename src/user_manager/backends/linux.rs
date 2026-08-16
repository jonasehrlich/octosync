//! Actor-serialized Linux user management through shadow-utils.

use crate::store;
use crate::user_manager::{
    AccountIds, CreateUser, EnsureGroupsExist, ExpireAccount, PurgeAccount, PurgeOutcome,
    SyncSupplementaryGroups, UpdateAuthorizedKeys, UpdateUser, supplementary_groups_update,
};
use anyhow::Context as _;
use std::{collections, time};
use tokio::process;

#[derive(Debug)]
pub struct LinuxUserManager;

impl hannibal::Actor for LinuxUserManager {}

impl hannibal::Handler<CreateUser> for LinuxUserManager {
    #[tracing::instrument(
        name = "UserManager::create_user",
        skip_all,
        fields(user = %msg.gh_user.login, id = msg.gh_user.id.into_inner())
    )]
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: CreateUser,
    ) -> anyhow::Result<store::User> {
        let user = &msg.gh_user;
        if let Ok(Some(existing_user)) = nix::unistd::User::from_name(&user.login) {
            verify_adoption(&existing_user, &msg.ids)?;
            tracing::info!(
                user = user.login,
                uid = existing_user.uid.as_raw(),
                "User already exists. Skipping creation."
            );

            // Reactivate an adopted account for a rejoining member.
            clear_departure_expiry(&existing_user.name).await?;

            return Ok(store::User::builder()
                .id(user.id)
                .uid(existing_user.uid)
                .gid(existing_user.gid)
                .name(existing_user.name.clone())
                .build());
        }

        let linux_user = match &msg.ids {
            AccountIds::Stored { uid, gid } => add_account(&user.login, Some(*uid), *gid).await?,
            AccountIds::Fresh {
                reserved_uids,
                reserved_gids,
            } => add_account_with_fresh_ids(&user.login, reserved_uids, reserved_gids).await?,
        };
        Ok(store::User::builder()
            .id(user.id)
            .uid(linux_user.uid)
            .gid(linux_user.gid)
            .name(linux_user.name.clone())
            .build())
    }
}

/// Check that a pre-existing account with the GitHub login may be adopted.
///
/// Adopting an account whose IDs differ from a rejoining member's stored ones, or
/// whose IDs another user's tombstone reserves, would create an ownership drift between the users.
fn verify_adoption(existing: &nix::unistd::User, ids: &AccountIds) -> anyhow::Result<()> {
    match ids {
        AccountIds::Stored { uid, gid } => {
            if existing.uid != *uid {
                anyhow::bail!(
                    "User '{}' already exists with UID {} but the store expects UID {}, \
                     refusing to adopt the account",
                    existing.name,
                    existing.uid,
                    uid
                );
            }
            if let Some(gid) = gid
                && existing.gid != *gid
            {
                anyhow::bail!(
                    "User '{}' already exists with primary GID {} but the store expects GID {}, \
                     refusing to adopt the account",
                    existing.name,
                    existing.gid,
                    gid
                );
            }
        }
        AccountIds::Fresh {
            reserved_uids,
            reserved_gids,
        } => {
            if reserved_uids.contains(&existing.uid) || reserved_gids.contains(&existing.gid) {
                anyhow::bail!(
                    "User '{}' already exists with UID {} and GID {}, which a departed user's \
                     tombstone reserves, refusing to adopt the account",
                    existing.name,
                    existing.uid,
                    existing.gid
                );
            }
        }
    }
    Ok(())
}

/// Number of candidates to probe before giving up the search for a free ID
const FREE_ID_ATTEMPTS: u32 = 10_000;

/// Create an account without re-using IDs reserved by tombstones.
///
/// If an account is created with a UID or GID that a departed user's tombstone reserves, the
/// new account may own files and directories that the departed user should still control.
/// Thus the account is deleted and the function searches for a free UID / GID pair.
async fn add_account_with_fresh_ids(
    login: &str,
    reserved_uids: &collections::HashSet<nix::unistd::Uid>,
    reserved_gids: &collections::HashSet<nix::unistd::Gid>,
) -> anyhow::Result<nix::unistd::User> {
    let linux_user = add_account(login, None, None).await?;
    if !reserved_uids.contains(&linux_user.uid) && !reserved_gids.contains(&linux_user.gid) {
        return Ok(linux_user);
    }

    tracing::info!(
        uid = linux_user.uid.as_raw(),
        gid = linux_user.gid.as_raw(),
        "Allocated IDs are reserved for a departed user, re-creating with free IDs"
    );
    // The new account has not escaped, so it is safe to recreate with explicit IDs.
    let o = process::Command::new("/usr/sbin/userdel")
        .arg("--remove")
        .arg(login)
        .output()
        .await
        .context("Failed to wait for userdel command to finish")?;
    if !o.status.success() {
        anyhow::bail!(
            "Failed to remove the account created with reserved IDs: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }

    let id = free_id_from(
        linux_user.uid.as_raw().max(linux_user.gid.as_raw()) + 1,
        reserved_uids,
        reserved_gids,
    )?;
    add_account(
        login,
        Some(nix::unistd::Uid::from_raw(id)),
        Some(nix::unistd::Gid::from_raw(id)),
    )
    .await
}

/// Find a free ID usable as both UID and private-group GID.
fn free_id_from(
    start: u32,
    reserved_uids: &collections::HashSet<nix::unistd::Uid>,
    reserved_gids: &collections::HashSet<nix::unistd::Gid>,
) -> anyhow::Result<u32> {
    let end = start.saturating_add(FREE_ID_ATTEMPTS);
    for id in start..end {
        let uid = nix::unistd::Uid::from_raw(id);
        let gid = nix::unistd::Gid::from_raw(id);
        if reserved_uids.contains(&uid) || reserved_gids.contains(&gid) {
            continue;
        }
        if nix::unistd::User::from_uid(uid)?.is_some()
            || nix::unistd::Group::from_gid(gid)?.is_some()
        {
            continue;
        }
        return Ok(id);
    }
    anyhow::bail!("No free UID/GID found in [{start}, {end})")
}

/// Create a platform account with `useradd` and return it.
async fn add_account(
    login: &str,
    uid: Option<nix::unistd::Uid>,
    gid: Option<nix::unistd::Gid>,
) -> anyhow::Result<nix::unistd::User> {
    let mut command = process::Command::new("/usr/sbin/useradd");
    command
        .arg("--create-home")
        .arg("--shell")
        .arg("/bin/bash")
        .arg("--password")
        .arg("!");
    if let Some(uid) = uid {
        command.arg("--uid").arg(uid.to_string());
    }
    if let Some(gid) = gid {
        // With --gid, useradd uses the given group as the primary group instead of
        // creating a private one, so the group must exist with the stored GID first
        ensure_private_group(login, gid).await?;
        command.arg("--gid").arg(gid.to_string());
    }
    command.arg(login);

    let o = command
        .output()
        .await
        .context("Failed to wait for useradd command to finish")?;

    if !o.status.success() {
        anyhow::bail!(
            "Failed to create user: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    tracing::info!("Created user");

    nix::unistd::User::from_name(login)
        .context("Failed to retrieve user info for newly created user")?
        .ok_or_else(|| {
            anyhow::anyhow!("User '{login}' was created but could not be found in the system")
        })
}

/// Recreate a missing stored account with its original identity.
async fn recreate_account(
    gh_user: &octocrab::models::Author,
    available_user: &store::User,
) -> anyhow::Result<nix::unistd::User> {
    // Refuse a manually recreated account with a different identity.
    let colliding = match nix::unistd::User::from_name(available_user.name())? {
        Some(existing) => Some(existing),
        None => nix::unistd::User::from_name(&gh_user.login)?,
    };
    if let Some(existing) = colliding {
        anyhow::bail!(
            "No account with UID {}, but '{}' exists with UID {}, refusing to re-create '{}'",
            available_user.uid(),
            existing.name,
            existing.uid,
            available_user.name()
        );
    }

    tracing::info!("User no longer exists in the system, re-creating with the stored UID");
    add_account(
        available_user.name(),
        Some(available_user.uid()),
        available_user.gid(),
    )
    .await
}

/// Recreate the private group, refusing conflicting names or GIDs.
async fn ensure_private_group(login: &str, gid: nix::unistd::Gid) -> anyhow::Result<()> {
    if let Some(group) = nix::unistd::Group::from_gid(gid)
        .with_context(|| format!("Failed to check whether GID {gid} is in use"))?
    {
        if group.name == login {
            // A previous attempt may have failed after groupadd.
            return Ok(());
        }
        anyhow::bail!(
            "GID {gid} already belongs to group '{}', refusing to re-create the private \
             group of '{login}' with it",
            group.name
        );
    }

    let o = process::Command::new("/usr/sbin/groupadd")
        .arg("--gid")
        .arg(gid.to_string())
        .arg(login)
        .output()
        .await
        .context("Failed to wait for groupadd command to finish")?;
    if !o.status.success() {
        anyhow::bail!(
            "Failed to re-create private group '{login}' with GID {gid}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    tracing::info!(gid = gid.as_raw(), "Re-created private group");
    Ok(())
}

impl hannibal::Handler<UpdateUser> for LinuxUserManager {
    #[tracing::instrument(
        name = "UserManager::update_user",
        skip_all,
        fields(from_uid = msg.available_user.uid().as_raw(), from = %msg.available_user.name(), to = %msg.gh_user.login)
    )]
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: UpdateUser,
    ) -> anyhow::Result<store::User> {
        let (gh_user, available_user) = (&msg.gh_user, &msg.available_user);
        // Accept the GitHub login after a rename whose store update was lost.
        let linux_user = match resolve_account(available_user, Some(&gh_user.login))? {
            AccountResolution::Matches(linux_user) => linux_user,
            // Recreate an account removed outside octosync.
            AccountResolution::Missing => recreate_account(gh_user, available_user).await?,
        };

        // Active org members must not retain an effective expiry.
        clear_departure_expiry(&linux_user.name).await?;

        if gh_user.login == linux_user.name {
            // Heal stale names and backfill GIDs missing from v1 stores.
            return Ok(store::User::builder()
                .id(available_user.id())
                .uid(linux_user.uid)
                .gid(linux_user.gid)
                .name(linux_user.name.clone())
                .build());
        }

        kill_processes_for_user(&linux_user).await?;
        let output = process::Command::new("/usr/sbin/usermod")
            .arg("--home")
            .arg(format!("/home/{}", gh_user.login))
            .arg("--move-home")
            .arg("--login")
            .arg(&gh_user.login)
            .arg(&linux_user.name)
            .output()
            .await
            .context("Failed to execute usermod command")?;

        if output.status.success() {
            tracing::info!(
                "Updated username from '{}' to '{}'",
                linux_user.name,
                gh_user.login
            );
            Ok(store::User::builder()
                .id(available_user.id())
                .uid(available_user.uid())
                .gid(linux_user.gid)
                .name(gh_user.login.clone())
                .build())
        } else {
            tracing::error!(
                error = %String::from_utf8_lossy(&output.stderr),
                "Failed to update username"
            );
            Err(anyhow::anyhow!(
                "Failed to update username for {}: {}",
                linux_user.name,
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }
}

impl hannibal::Handler<ExpireAccount> for LinuxUserManager {
    #[tracing::instrument(
        name = "UserManager::expire_account",
        skip_all,
        fields(user = %msg.user.name(), uid = msg.user.uid().as_raw())
    )]
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: ExpireAccount,
    ) -> anyhow::Result<()> {
        let AccountResolution::Matches(linux_user) = resolve_account(&msg.user, None)? else {
            tracing::warn!("User not found in system when attempting to expire, nothing to do");
            return Ok(());
        };

        // Block new sessions before tearing down existing work.
        expire_account(&linux_user.name).await?;

        // Run independent cleanup steps before returning the first error.

        // Remove schedulers before sweeping processes.
        let scheduled_jobs = remove_scheduled_jobs(&linux_user).await;

        // Sweep anything outside logind, or everything on systems without it.
        end_logind_sessions(&linux_user).await;
        let sweep = kill_processes_for_user(&linux_user).await;

        // Remove access granted through supplementary groups.
        let groups = strip_supplementary_groups(&linux_user).await;

        // Remove both fetched and manually configured SSH access.
        let keys = tokio::task::spawn_blocking(move || {
            crate::authorized_keys::remove_authorized_keys(&linux_user)
        })
        .await
        .context("Authorized keys removal task failed")?;

        scheduled_jobs.and(sweep).and(groups).and(keys)
    }
}

/// End the user's login sessions through logind, which logs them out cleanly. It
/// closes the PAM sessions and tears down the session scopes and the user manager
/// through the cgroup hierarchy instead of only signaling PIDs, so nothing escapes by
/// forking.
async fn end_logind_sessions(user: &nix::unistd::User) {
    let connection = match tokio::time::timeout(LOGIND_TIMEOUT, zbus::Connection::system()).await {
        Ok(Ok(connection)) => connection,
        // Containers and non-systemd hosts rely on the process sweep.
        Ok(Err(e)) => {
            tracing::debug!("No system bus, ending sessions with the process sweep alone: {e}");
            return;
        }
        Err(_) => {
            tracing::error!("System bus did not answer in time, leaving the sessions to the sweep");
            return;
        }
    };

    // Bound D-Bus calls so a stuck logind cannot stall the actor.
    let uid = user.uid.as_raw();
    match tokio::time::timeout(LOGIND_TIMEOUT, terminate_logind_user(&connection, uid)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!("logind is available but did not end the sessions: {e:#}"),
        Err(_) => {
            tracing::error!("logind did not answer in time, leaving the sessions to the sweep")
        }
    }
}

const LOGIND_TIMEOUT: time::Duration = time::Duration::from_secs(5);

/// Disable linger and ask logind to terminate the user's sessions.
async fn terminate_logind_user(connection: &zbus::Connection, uid: u32) -> anyhow::Result<()> {
    let manager = zbus::Proxy::new(
        connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .context("Failed to create the logind manager proxy")?;

    // Disabling linger keeps a `user@.service` from restarting user services after the
    // termination, but a surviving login session matters more, so a failure here must not skip the
    // termination below
    if let Err(e) = manager
        .call::<_, _, ()>("SetUserLinger", &(uid, false, false))
        .await
    {
        tracing::warn!("Failed to disable linger, terminating the sessions anyway: {e}");
    }

    // GetUser fails when there is no session or lingering user manager.
    if manager
        .call::<_, _, zbus::zvariant::OwnedObjectPath>("GetUser", &uid)
        .await
        .is_err()
    {
        return Ok(());
    }
    manager
        .call::<_, _, ()>("TerminateUser", &uid)
        .await
        .context("Failed to terminate the user's sessions")
}

/// Remove `cron` and `at` jobs that can outlive login sessions.
///
/// Expiring the account already blocks the PAM account check `cron` and `at` perform, so
/// this is the second half of that guarantee rather than the only one, and it also
/// covers a machine whose `cron` does not consult PAM.
async fn remove_scheduled_jobs(user: &nix::unistd::User) -> anyhow::Result<()> {
    remove_crontab(&user.name).await?;
    remove_at_jobs(&user.name).await
}

/// Remove the user's `crontab`
///
/// List the `crontab` first to avoid treating absence as an error.
async fn remove_crontab(name: &str) -> anyhow::Result<()> {
    let Some(list) = optional_command("/usr/bin/crontab", &["-l", "-u", name]).await? else {
        return Ok(());
    };
    if !list.status.success() {
        return Ok(());
    }

    let output = process::Command::new("/usr/bin/crontab")
        .args(["-r", "-u", name])
        .output()
        .await
        .context("Failed to wait for crontab command to finish")?;
    if !output.status.success() {
        anyhow::bail!(
            "Failed to remove the crontab of '{name}': {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    tracing::info!("Removed the crontab of the departed user");
    Ok(())
}

/// Remove the user's queued `at` jobs from root's global listing.
///
/// `atq` lists the jobs of every user with the owner in the last field, which is the only way to
/// select one user's jobs for `atrm`.
async fn remove_at_jobs(name: &str) -> anyhow::Result<()> {
    let Some(queue) = optional_command("/usr/bin/atq", &[]).await? else {
        return Ok(());
    };
    if !queue.status.success() {
        anyhow::bail!(
            "Failed to list the at jobs of '{name}': {}",
            String::from_utf8_lossy(&queue.stderr)
        );
    }

    let job_ids: Vec<String> = String::from_utf8_lossy(&queue.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let job_id = fields.next()?;
            (fields.last()? == name).then(|| job_id.to_string())
        })
        .collect();
    if job_ids.is_empty() {
        return Ok(());
    }

    let output = process::Command::new("/usr/bin/atrm")
        .args(&job_ids)
        .output()
        .await
        .context("Failed to wait for atrm command to finish")?;
    if !output.status.success() {
        anyhow::bail!(
            "Failed to remove the at jobs of '{name}': {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    tracing::info!(
        jobs = job_ids.len(),
        "Removed the at jobs of the departed user"
    );
    Ok(())
}

/// Treat a missing optional command as no work to do.
async fn optional_command(
    program: &str,
    args: &[&str],
) -> anyhow::Result<Option<std::process::Output>> {
    match process::Command::new(program).args(args).output().await {
        Ok(output) => Ok(Some(output)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!("'{program}' is not installed");
            Ok(None)
        }
        Err(e) => Err(e).with_context(|| format!("Failed to run '{program}'")),
    }
}

/// Remove all supplementary groups, skipping `usermod` when none exist.
async fn strip_supplementary_groups(user: &nix::unistd::User) -> anyhow::Result<()> {
    let current = current_supplementary_groups(user)
        .with_context(|| format!("Failed to read current groups of '{}'", user.name))?;
    if current.is_empty() {
        return Ok(());
    }
    sync_user_supplementary_groups_by_name(&user.name, &[]).await
}

impl hannibal::Handler<PurgeAccount> for LinuxUserManager {
    #[tracing::instrument(
        name = "UserManager::purge_account",
        skip_all,
        fields(user = %msg.user.name(), uid = msg.user.uid().as_raw())
    )]
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: PurgeAccount,
    ) -> anyhow::Result<PurgeOutcome> {
        let AccountResolution::Matches(linux_user) = resolve_account(&msg.user, None)? else {
            return Ok(PurgeOutcome::NoAccount);
        };

        // Reactivation clears the shadow expiry, preventing a live account from being purged.
        if !account_is_expired(&linux_user.name, msg.expired_before)? {
            return Ok(PurgeOutcome::NotExpired);
        }

        // The account has been expired for the whole retention period, so nothing is
        // left to shut down cleanly into. userdel decides whether the account is busy,
        // so a failed sweep is only logged.
        if let Err(e) = force_kill_processes_for_user(&linux_user).await {
            tracing::warn!("Failed to sweep processes before userdel: {e:#}");
        }

        let o = process::Command::new("/usr/sbin/userdel")
            .arg("--remove")
            .arg(&linux_user.name)
            .output()
            .await
            .context("Failed to wait for userdel command to finish")?;
        if !o.status.success() {
            anyhow::bail!(
                "Failed to purge user '{}': {}",
                linux_user.name,
                String::from_utf8_lossy(&o.stderr)
            );
        }
        tracing::info!("Purged the expired account and its home directory");
        Ok(PurgeOutcome::Purged)
    }
}

impl hannibal::Handler<SyncSupplementaryGroups> for LinuxUserManager {
    #[tracing::instrument(
        name = "UserManager::sync_supplementary_groups",
        skip_all,
        fields(user = %msg.user.name(), uid = msg.user.uid().as_raw())
    )]
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: SyncSupplementaryGroups,
    ) -> anyhow::Result<()> {
        let user = &msg.user;
        let linux_user = match resolve_account(user, None)? {
            AccountResolution::Matches(linux_user) => linux_user,
            AccountResolution::Missing => anyhow::bail!(
                "User '{}' was not found while syncing supplementary groups",
                user.name()
            ),
        };

        let primary_group_name = nix::unistd::Group::from_gid(linux_user.gid)
            .context("Failed to read primary group while syncing groups")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Primary group for '{}' was not found while syncing groups",
                    user.name()
                )
            })?
            .name;

        let current_groups = current_supplementary_groups(&linux_user)
            .with_context(|| format!("Failed to read current groups of '{}'", user.name()))?;

        let Some(supplementary_groups) =
            supplementary_groups_update(&msg.groups, &primary_group_name, &current_groups)
        else {
            tracing::debug!("Supplementary groups are already up to date");
            return Ok(());
        };
        sync_user_supplementary_groups_by_name(&linux_user.name, &supplementary_groups).await
    }
}

impl hannibal::Handler<EnsureGroupsExist> for LinuxUserManager {
    #[tracing::instrument(name = "UserManager::ensure_groups_exist", skip_all, fields(groups = ?msg.groups))]
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: EnsureGroupsExist,
    ) -> anyhow::Result<()> {
        for group in &msg.groups {
            if nix::unistd::Group::from_name(group)
                .with_context(|| format!("Failed to check if group '{}' exists", group))?
                .is_some()
            {
                continue;
            }

            let output = process::Command::new("/usr/sbin/groupadd")
                .arg(group)
                .output()
                .await
                .with_context(|| format!("Failed to execute groupadd for '{}'", group))?;

            if output.status.success() {
                tracing::info!(group, "Created missing group");
            } else {
                return Err(anyhow::anyhow!(
                    "Failed to create missing group '{}': {}",
                    group,
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }

        Ok(())
    }
}

impl hannibal::Handler<UpdateAuthorizedKeys> for LinuxUserManager {
    #[tracing::instrument(
        name = "UserManager::update_authorized_keys",
        skip_all,
        fields(user = %msg.user.name(), keys = msg.keys.len())
    )]
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: UpdateAuthorizedKeys,
    ) -> anyhow::Result<()> {
        let AccountResolution::Matches(linux_user) = resolve_account(&msg.user, None)? else {
            anyhow::bail!(
                "User '{}' was not found while updating authorized_keys",
                msg.user.name()
            );
        };
        tokio::task::spawn_blocking(move || {
            crate::authorized_keys::update_authorized_keys(&linux_user, &msg.keys)
        })
        .await
        .context("Authorized keys update task failed")?
    }
}

/// A matching platform account or proof that neither its name nor UID exists.
enum AccountResolution {
    /// No account with the stored name exists and the stored UID is unused
    Missing,
    /// The account whose name and UID agree with the store
    Matches(nix::unistd::User),
}

/// Resolve the platform account of a stored user
///
/// This is a crucial check used by every mutating handler, because `shadow-utils` commands operate
/// on the account name, so the name is authoritative and the stored UID must agree with it.
///
/// Acting on a name whose UID changed on the system, or a UID whose name moved on the system, would
/// mutate an unrelated account.
///
/// `renamed_to` accepts one alternate name for the account holding the stored UID: a usermod
/// rename whose store update was lost leaves the account only findable by
/// UID, and the caller knows which new name proves it is still the managed account.
fn resolve_account(
    user: &store::User,
    renamed_to: Option<&str>,
) -> anyhow::Result<AccountResolution> {
    let Some(linux_user) = nix::unistd::User::from_name(user.name())? else {
        if let Some(other_user) = nix::unistd::User::from_uid(user.uid())? {
            if renamed_to.is_some_and(|name| name == other_user.name) {
                return Ok(AccountResolution::Matches(other_user));
            }
            // An unexpected name cannot safely be distinguished from UID reuse.
            anyhow::bail!(
                "No user named '{}' in the system, but UID {} belongs to '{}', refusing to act \
                 on the account",
                user.name(),
                user.uid(),
                other_user.name
            );
        }
        return Ok(AccountResolution::Missing);
    };
    if linux_user.uid != user.uid() {
        anyhow::bail!(
            "User '{}' has UID {} in the system but UID {} in the store, refusing to act on \
             the account",
            user.name(),
            linux_user.uid,
            user.uid()
        );
    }
    Ok(AccountResolution::Matches(linux_user))
}

/// Supplementary group names, excluding the primary group.
fn current_supplementary_groups(
    user: &nix::unistd::User,
) -> anyhow::Result<collections::BTreeSet<String>> {
    let user_name = std::ffi::CString::new(user.name.as_str())
        .context("User name contains an interior NUL byte")?;
    let gids = nix::unistd::getgrouplist(&user_name, user.gid)
        .context("Failed to list the user's groups")?;

    let mut names = collections::BTreeSet::new();
    for gid in gids {
        if gid == user.gid {
            continue;
        }
        // Skip groups deleted after getgrouplist.
        if let Some(group) = nix::unistd::Group::from_gid(gid)
            .with_context(|| format!("Failed to resolve group with GID {gid}"))?
        {
            names.insert(group.name);
        }
    }
    Ok(names)
}

async fn sync_user_supplementary_groups_by_name(
    user_name: &str,
    supplementary_groups: &[String],
) -> anyhow::Result<()> {
    let output = process::Command::new("/usr/sbin/usermod")
        .arg("--groups")
        .arg(supplementary_groups.join(","))
        .arg(user_name)
        .output()
        .await
        .context("Failed to execute usermod command for group updates")?;

    if output.status.success() {
        tracing::info!(
            user = user_name,
            groups = ?supplementary_groups,
            "Synchronized supplementary groups"
        );
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Failed to update groups for user '{}': {}",
            user_name,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// Expire the account as of yesterday; existing sessions are handled separately.
async fn expire_account(name: &str) -> anyhow::Result<()> {
    let expire_at = chrono::Utc::now() - chrono::Duration::days(1);
    if account_is_expired(name, expire_at)? {
        return Ok(());
    }
    let output = process::Command::new("/usr/sbin/usermod")
        .arg("--expiredate")
        // Use epoch days to avoid YYYY-MM-DD timezone interpretation.
        .arg(shadow_days(expire_at).to_string())
        .arg(name)
        .output()
        .await
        .context("Failed to execute usermod command for account expiry")?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to expire account '{}': {}",
            name,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    tracing::info!("Expired account so no new session can start");
    Ok(())
}

/// Clear an effective expiry while preserving a future scheduled expiry.
async fn clear_departure_expiry(name: &str) -> anyhow::Result<()> {
    if !account_is_expired(name, chrono::Utc::now())? {
        return Ok(());
    }

    let output = process::Command::new("/usr/sbin/usermod")
        .arg("--expiredate")
        .arg("")
        .arg(name)
        .output()
        .await
        .context("Failed to execute usermod command to clear the account expiry")?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to clear the expiry of account '{}': {}",
            name,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    tracing::info!("Cleared the expiry of the reactivated account");
    Ok(())
}

/// Whether the shadow expiry is on or before `as_of`.
fn account_is_expired(name: &str, as_of: chrono::DateTime<chrono::Utc>) -> anyhow::Result<bool> {
    Ok(account_expire_days(name)?.is_some_and(|expire_days| expire_days <= shadow_days(as_of)))
}

fn shadow_days(timestamp: chrono::DateTime<chrono::Utc>) -> i64 {
    timestamp.timestamp().div_euclid(SECONDS_PER_DAY)
}

/// Read the shadow expiry as days since the epoch through libc's `getspnam`.
fn account_expire_days(name: &str) -> anyhow::Result<Option<i64>> {
    let c_name = std::ffi::CString::new(name).context("User name contains an interior NUL byte")?;
    // SAFETY: getspnam returns a pointer into static storage, which is only unsound
    // when another thread calls it concurrently. Every call site runs on the user
    // manager actor, which processes one message at a time.
    let entry = unsafe { libc::getspnam(c_name.as_ptr()) };
    if entry.is_null() {
        return Ok(None);
    }
    // SAFETY: checked to be non-null above, and the field is copied out before any
    // following getspnam call can overwrite the storage
    let expire_days = unsafe { (*entry).sp_expire };
    // An empty expiry field is reported as -1
    Ok((expire_days >= 0).then_some(expire_days))
}

const KILL_GRACE_PERIOD: time::Duration = time::Duration::from_secs(3);
const KILL_POLL_INTERVAL: time::Duration = time::Duration::from_millis(200);
const SIGKILL_WAIT: time::Duration = time::Duration::from_secs(1);

/// Stop all processes for a user with SIGTERM, so they can shutdown cleanly.
/// After [`KILL_GRACE_PERIOD`], `SIGKILL` is sent to the remaining processes.
///
/// Errors are returned if any processes remain after [`SIGKILL_WAIT`].
#[tracing::instrument(name = "kill_processes", skip(user), fields(user = %user.name))]
async fn kill_processes_for_user(user: &nix::unistd::User) -> anyhow::Result<()> {
    let uid = user.uid.as_raw();
    let procs = processes_of_uid(uid).await?;
    if procs.is_empty() {
        return Ok(());
    }
    signal_processes(&procs, nix::sys::signal::Signal::SIGTERM);

    let remaining = wait_for_processes_to_exit(uid, KILL_GRACE_PERIOD).await?;
    if remaining.is_empty() {
        tracing::debug!("All processes exited after SIGTERM");
        return Ok(());
    }

    tracing::debug!(
        count = remaining.len(),
        "Processes still running after the grace period, killing them"
    );
    signal_processes(&remaining, nix::sys::signal::Signal::SIGKILL);
    ensure_processes_are_gone(uid, &user.name).await
}

/// Kill all user processes without a grace period.
#[tracing::instrument(name = "force_kill_processes", skip(user), fields(user = %user.name))]
async fn force_kill_processes_for_user(user: &nix::unistd::User) -> anyhow::Result<()> {
    let uid = user.uid.as_raw();
    let procs = processes_of_uid(uid).await?;
    if procs.is_empty() {
        return Ok(());
    }
    signal_processes(&procs, nix::sys::signal::Signal::SIGKILL);
    ensure_processes_are_gone(uid, &user.name).await
}

/// Confirm killed processes leave the process table.
async fn ensure_processes_are_gone(uid: u32, name: &str) -> anyhow::Result<()> {
    let survivors = wait_for_processes_to_exit(uid, SIGKILL_WAIT).await?;
    if survivors.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "{} processes of '{name}' still exist after SIGKILL",
        survivors.len()
    )
}

/// Wait for a UID's processes to exit, returning any survivors.
async fn wait_for_processes_to_exit(
    uid: u32,
    timeout: time::Duration,
) -> anyhow::Result<Vec<nix::unistd::Pid>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = processes_of_uid(uid).await?;
        if remaining.is_empty() || tokio::time::Instant::now() >= deadline {
            return Ok(remaining);
        }
        tokio::time::sleep(KILL_POLL_INTERVAL).await;
    }
}

/// Live non-zombie processes whose real UID matches `uid`.
async fn processes_of_uid(uid: u32) -> anyhow::Result<Vec<nix::unistd::Pid>> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<nix::unistd::Pid>> {
        let procs = procfs::process::all_processes().context("Failed to list processes")?;
        // Ignore processes that exit during inspection.
        Ok(procs
            .flatten()
            .filter(|proc| {
                proc.status()
                    .is_ok_and(|stat| stat.ruid == uid && !stat.state.starts_with('Z'))
            })
            .map(|proc| nix::unistd::Pid::from_raw(proc.pid))
            .collect())
    })
    .await
    .context("Process listing task failed")?
}

/// Signal every PID, ignoring processes that already exited.
fn signal_processes(pids: &[nix::unistd::Pid], signal: nix::sys::signal::Signal) {
    for &pid in pids {
        let _ = nix::sys::signal::kill(pid, signal);
        tracing::debug!(pid = pid.as_raw(), ?signal, "Signaled process");
    }
}
