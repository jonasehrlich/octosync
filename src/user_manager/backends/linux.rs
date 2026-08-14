//! Linux backend running the shadow-utils commands (useradd, usermod, userdel,
//! groupadd). It only ever runs as an actor: the commands fail instead of waiting when
//! another invocation holds the lock on /etc/passwd or /etc/group, so the actor's
//! one-message-at-a-time processing is what keeps them from racing each other.

use crate::store;
use crate::user_manager::{
    CreateUser, DeletionPreparation, EnsureGroupsExist, PrepareUserDeletion, RemoveAccount,
    SyncSupplementaryGroups, UpdateAuthorizedKeys, UpdateUser, supplementary_groups_update,
};
use anyhow::Context as _;
use std::{collections, time};
use tokio::process;

#[derive(Debug)]
pub struct LinuxUserManager {
    authorized_keys: crate::authorized_keys::AuthorizedKeysManager,
}

impl LinuxUserManager {
    pub fn new() -> Self {
        Self {
            authorized_keys: crate::authorized_keys::AuthorizedKeysManager,
        }
    }
}

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
            tracing::info!(
                "User '{}' already exists with UID {}. Skipping creation.",
                user.login,
                existing_user.uid
            );

            return Ok(store::User::builder()
                .id(user.id)
                .uid(existing_user.uid)
                .name(user.login.clone())
                .build());
        }

        add_account(&user.login, user.id, None).await
    }
}

/// Create a platform account with `useradd`. Re-creating a previously deleted account
/// with its stored UID keeps ownership of files outside the home directory intact.
async fn add_account(
    login: &str,
    id: octocrab::models::UserId,
    uid: Option<nix::unistd::Uid>,
) -> anyhow::Result<store::User> {
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

    let linux_user = nix::unistd::User::from_name(login)
        .context("Failed to retrieve user info for newly created user")?
        .ok_or_else(|| {
            anyhow::anyhow!("User '{login}' was created but could not be found in the system")
        })?;

    Ok(store::User::builder()
        .id(id)
        .uid(linux_user.uid)
        .name(login.to_string())
        .build())
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
        let Some(linux_user) = nix::unistd::User::from_uid(available_user.uid())? else {
            // The account is known in the store but gone from the system, e.g. deleted by
            // hand. Re-create it with the stored UID so files owned by the old account
            // keep their owner.
            tracing::info!("User no longer exists in the system, re-creating with stored UID");
            return add_account(&gh_user.login, gh_user.id, Some(available_user.uid())).await;
        };

        if gh_user.login == linux_user.name {
            return Ok(available_user.clone());
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

impl hannibal::Handler<PrepareUserDeletion> for LinuxUserManager {
    #[tracing::instrument(
        name = "UserManager::prepare_user_deletion",
        skip_all,
        fields(user = %msg.user.name(), uid = msg.user.uid().as_raw())
    )]
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: PrepareUserDeletion,
    ) -> anyhow::Result<DeletionPreparation> {
        let Some(linux_user) = resolve_account_checked(&msg.user)? else {
            tracing::warn!("User not found in system when attempting to delete, nothing to do");
            return Ok(DeletionPreparation::NothingToDo);
        };

        // Kill all of the user's processes so none can block the deletion or keep
        // writing to the home directory while it is archived
        kill_processes_for_user(&linux_user).await?;

        Ok(DeletionPreparation::Prepared {
            home_dir: linux_user.dir,
        })
    }
}

impl hannibal::Handler<RemoveAccount> for LinuxUserManager {
    #[tracing::instrument(
        name = "UserManager::remove_account",
        skip_all,
        fields(user = %msg.user.name(), uid = msg.user.uid().as_raw())
    )]
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: RemoveAccount,
    ) -> anyhow::Result<()> {
        // The actor handles other messages between preparation and removal, so verify
        // again that the name still belongs to the stored user before userdel
        let Some(linux_user) = resolve_account_checked(&msg.user)? else {
            tracing::warn!("User disappeared after deletion was prepared, nothing to do");
            return Ok(());
        };

        // A process spawned between the preparation sweep and here (archiving can take
        // a while) would make userdel fail, so sweep again right before it
        kill_processes_for_user(&linux_user).await?;

        let proc = process::Command::new("/usr/sbin/userdel")
            .arg("--remove")
            .arg(msg.user.name())
            .output();

        let o = proc
            .await
            .context("Failed to wait for userdel command to finish")?;

        if o.status.success() {
            tracing::info!(archive = ?msg.receipt.archive_path(), "Deleted user");
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Failed to delete user '{}': {}",
                msg.user.name(),
                String::from_utf8_lossy(&o.stderr)
            ))
        }
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
        let linux_user = nix::unistd::User::from_uid(user.uid())
            .context("Failed to read user before syncing groups")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "User '{}' was not found while syncing supplementary groups",
                    user.name()
                )
            })?;

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
        self.authorized_keys
            .update_authorized_keys(&msg.user, &msg.keys)
            .await
    }
}

/// Resolve the platform account of a stored user for deletion, refusing when name and
/// UID do not match. `Ok(None)` when no account with that name exists and the UID is
/// unused.
fn resolve_account_checked(user: &store::User) -> anyhow::Result<Option<nix::unistd::User>> {
    // userdel operates on the account name, so resolve the account by name to
    // guarantee the home directory that is archived is the one userdel --remove deletes
    let Some(linux_user) = nix::unistd::User::from_name(user.name())? else {
        // The account may exist under a different name, e.g. when a usermod rename
        // succeeded but the store update was lost. Whether that is this user or an
        // unrelated account that reuses the UID can not be decided here, refuse
        // instead of orphaning the account or deleting a wrong one.
        if let Some(other_user) = nix::unistd::User::from_uid(user.uid())? {
            anyhow::bail!(
                "No user named '{}' in the system, but UID {} belongs to '{}', refusing to delete",
                user.name(),
                user.uid(),
                other_user.name
            );
        }
        return Ok(None);
    };
    if linux_user.uid != user.uid() {
        anyhow::bail!(
            "User '{}' has UID {} in the system but UID {} in the store, refusing to delete",
            user.name(),
            linux_user.uid,
            user.uid()
        );
    }
    Ok(Some(linux_user))
}

/// The names of the supplementary groups the user is currently a member of,
/// excluding the primary group
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
        // A group deleted since getgrouplist has no name to compare or pass to
        // usermod, skip it
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

/// Grace period a SIGTERM'd process gets to exit before it is killed
const KILL_GRACE_PERIOD: time::Duration = time::Duration::from_secs(3);
/// Poll interval while waiting for terminated processes to exit
const KILL_POLL_INTERVAL: time::Duration = time::Duration::from_millis(200);

/// Stop all processes of the user: SIGTERM first so they can shut down cleanly, then
/// SIGKILL whatever is still running after [`KILL_GRACE_PERIOD`].
///
/// Runs inside the actor, so the grace period stalls other platform operations. It is
/// only paid when the user actually has running processes.
#[tracing::instrument(name = "kill_processes", skip(user), fields(user = %user.name))]
async fn kill_processes_for_user(user: &nix::unistd::User) -> anyhow::Result<()> {
    let uid = user.uid.as_raw();
    let procs = processes_of_uid(uid).await?;
    if procs.is_empty() {
        return Ok(());
    }
    signal_processes(&procs, nix::sys::signal::Signal::SIGTERM);

    let deadline = tokio::time::Instant::now() + KILL_GRACE_PERIOD;
    let remaining = loop {
        tokio::time::sleep(KILL_POLL_INTERVAL).await;
        let remaining = processes_of_uid(uid).await?;
        if remaining.is_empty() {
            tracing::debug!("All processes exited after SIGTERM");
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            break remaining;
        }
    };

    tracing::debug!(
        count = remaining.len(),
        "Processes still running after the grace period, killing them"
    );
    signal_processes(&remaining, nix::sys::signal::Signal::SIGKILL);
    Ok(())
}

/// PIDs of all processes whose real UID is `uid`
async fn processes_of_uid(uid: u32) -> anyhow::Result<Vec<nix::unistd::Pid>> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<nix::unistd::Pid>> {
        let procs = procfs::process::all_processes().context("Failed to list processes")?;
        // Processes that exit while being inspected are skipped
        Ok(procs
            .flatten()
            .filter(|proc| proc.status().is_ok_and(|stat| stat.ruid == uid))
            .map(|proc| nix::unistd::Pid::from_raw(proc.pid))
            .collect())
    })
    .await
    .context("Process listing task failed")?
}

/// Send `signal` to all `pids`. A process that is already gone is not an error.
fn signal_processes(pids: &[nix::unistd::Pid], signal: nix::sys::signal::Signal) {
    for &pid in pids {
        let _ = nix::sys::signal::kill(pid, signal);
        tracing::debug!(pid = pid.as_raw(), ?signal, "Signaled process");
    }
}
