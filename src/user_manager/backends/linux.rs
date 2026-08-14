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
                user = user.login,
                uid = existing_user.uid.as_raw(),
                "User already exists. Skipping creation."
            );

            // The adopted account may carry the expiry of a failed deletion whose
            // store entry is gone, e.g. after the delete command wiped the store
            clear_deletion_expiry(&existing_user.name).await?;

            return Ok(store::User::builder()
                .id(user.id)
                .uid(existing_user.uid)
                .name(existing_user.name.clone())
                .build());
        }

        let linux_user = add_account(&user.login, None).await?;
        Ok(store::User::builder()
            .id(user.id)
            .uid(linux_user.uid)
            .name(linux_user.name.clone())
            .build())
    }
}

/// Create a platform account with `useradd` and return it. Re-creating a previously
/// deleted account with its stored UID keeps ownership of files outside the home
/// directory intact.
async fn add_account(
    login: &str,
    uid: Option<nix::unistd::Uid>,
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

/// Re-create the platform account of a stored user that is gone from the system, using
/// the stored name and UID so file ownership and a home directory that survived the
/// deletion stay intact. A changed GitHub login is applied afterwards by the regular
/// rename path.
async fn recreate_account(
    gh_user: &octocrab::models::Author,
    available_user: &store::User,
) -> anyhow::Result<nix::unistd::User> {
    // The account may have been re-created by hand under a new UID. Adopting it would
    // silently accept the ownership drift the stored UID exists to prevent, so refuse
    // and leave the resolution to an operator.
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
    add_account(available_user.name(), Some(available_user.uid())).await
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
        let linux_user = match nix::unistd::User::from_uid(available_user.uid())? {
            Some(linux_user) => linux_user,
            // The account is known in the store but gone from the system, e.g. deleted
            // by hand
            None => recreate_account(gh_user, available_user).await?,
        };

        // The stored UID may have been freed and recycled by an unrelated account. Only
        // the stored name (the normal case) or the GitHub login (a rename whose store
        // update was lost) prove this is the managed account.
        if linux_user.name != available_user.name() && linux_user.name != gh_user.login {
            anyhow::bail!(
                "UID {} belongs to '{}', not to stored user '{}', refusing to update",
                available_user.uid(),
                linux_user.name,
                available_user.name()
            );
        }

        // The account may still carry the expiry of a deletion that failed after
        // PrepareUserDeletion, e.g. when the user left and rejoined within one window
        clear_deletion_expiry(&linux_user.name).await?;

        if gh_user.login == linux_user.name {
            // Rebuild the entry from the system account, healing a stored name that a
            // lost store update after a rename left stale

            return Ok(store::User::builder()
                .id(available_user.id())
                .uid(linux_user.uid)
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

        // The password is locked, but pubkey SSH keeps working while the home directory
        // is archived. Expire the account before the sweep so no new session can start
        // anywhere in the deletion window.
        expire_account(&linux_user.name).await?;

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
        // a while) would make userdel fail, so sweep again right before it. The home
        // directory is already archived, so nothing is left for a process to shut down
        // cleanly into: kill hard without a grace period. userdel decides
        // authoritatively whether the account is busy, so a failed sweep is only logged.
        if let Err(e) = force_kill_processes_for_user(&linux_user).await {
            tracing::warn!("Failed to sweep processes before userdel: {e:#}");
        }

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

const SECONDS_PER_DAY: u64 = 86_400;

/// Today as the day number since the epoch, the unit of the shadow expiry field
fn days_since_epoch() -> anyhow::Result<i64> {
    let days = time::SystemTime::now()
        .duration_since(time::UNIX_EPOCH)
        .context("System time is before the epoch")?
        .as_secs()
        / SECONDS_PER_DAY;
    Ok(days as i64)
}

/// Expire the account so no new session (password or pubkey SSH) can start while its
/// deletion is in progress. Sessions already running are handled by the process sweep.
///
/// The expiry is set to the day before the deletion: the shadow field has day
/// granularity and some login paths treat an account as expired only strictly after
/// its date, so the deletion day itself could keep logins open until midnight.
async fn expire_account(name: &str) -> anyhow::Result<()> {
    let expire_days = days_since_epoch()? - 1;
    let output = process::Command::new("/usr/sbin/usermod")
        .arg("--expiredate")
        // shadow parses a plain number as days since the epoch, which sidesteps the
        // timezone interpretation of a YYYY-MM-DD date. chage displays it as a date.
        .arg(expire_days.to_string())
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
    tracing::info!("Expired account so no new session can start during deletion");
    Ok(())
}

/// Lift an expiry that is already in effect, so a user whose account survived a failed
/// deletion and who is synced again is not locked out silently. An expiry in the
/// future is an operator's scheduled offboarding and stays.
///
/// octosync owns the account lifecycle of synced users, so a past expiry set by an
/// operator does not survive a sync either. Suspending a member is done by removing
/// them from the org.
async fn clear_deletion_expiry(name: &str) -> anyhow::Result<()> {
    let Some(expire_days) = account_expire_days(name)? else {
        return Ok(());
    };
    if expire_days > days_since_epoch()? {
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
    tracing::info!("Cleared the expiry of the re-activated account");
    Ok(())
}

/// The account's expiry as days since the epoch, `None` when no expiry is set, read
/// through the same NSS lookup the passwd queries of this module use.
fn account_expire_days(name: &str) -> anyhow::Result<Option<i64>> {
    let c_name = std::ffi::CString::new(name).context("User name contains an interior NUL byte")?;
    // SAFETY: getspnam returns a pointer into static storage, which is only unsound
    // when another thread calls it concurrently. Every call site runs on the user
    // manager actor, which processes one message at a time.
    let entry = unsafe { libc::getspnam(c_name.as_ptr()) };
    if entry.is_null() {
        // No shadow entry means no expiry to consider
        return Ok(None);
    }
    // SAFETY: checked to be non-null above, and the field is copied out before any
    // following getspnam call can overwrite the storage
    let expire_days = unsafe { (*entry).sp_expire };
    // An empty expiry field is reported as -1
    Ok((expire_days >= 0).then_some(expire_days))
}

/// Grace period a SIGTERM'd process gets to exit before it is killed
const KILL_GRACE_PERIOD: time::Duration = time::Duration::from_secs(3);
/// Poll interval while waiting for terminated processes to exit
const KILL_POLL_INTERVAL: time::Duration = time::Duration::from_millis(200);
/// Time SIGKILL'd processes get to disappear from the process table, so a following
/// userdel cannot race a process that is still being torn down
const SIGKILL_WAIT: time::Duration = time::Duration::from_secs(1);

/// Stop all processes of the user: SIGTERM first so they can shut down cleanly, then
/// SIGKILL whatever is still running after [`KILL_GRACE_PERIOD`]. Errors when
/// processes survive the SIGKILL, e.g. stuck in uninterruptible I/O.
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

/// SIGKILL all processes of the user without a grace period
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

/// Confirm that the SIGKILL'd processes are gone from the process table within
/// [`SIGKILL_WAIT`]
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

/// Poll until no process of `uid` is left or `timeout` elapses, returning the
/// processes still running
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

/// PIDs of all live processes whose real UID is `uid`. Zombies are excluded: no signal
/// removes them and only their parent reaping them does, so counting them would stall
/// every wait for the full timeout.
async fn processes_of_uid(uid: u32) -> anyhow::Result<Vec<nix::unistd::Pid>> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<nix::unistd::Pid>> {
        let procs = procfs::process::all_processes().context("Failed to list processes")?;
        // Processes that exit while being inspected are skipped
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

/// Send `signal` to all `pids`. A process that is already gone is not an error.
fn signal_processes(pids: &[nix::unistd::Pid], signal: nix::sys::signal::Signal) {
    for &pid in pids {
        let _ = nix::sys::signal::kill(pid, signal);
        tracing::debug!(pid = pid.as_raw(), ?signal, "Signaled process");
    }
}
