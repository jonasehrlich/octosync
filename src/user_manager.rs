use crate::store;

pub trait CreateUser {
    /// Creates a platform user for the given GitHub user.
    async fn create_user(&self, user: &octocrab::models::Author) -> anyhow::Result<store::User>;
}

#[allow(unused)]
pub trait DeleteUser {
    /// Deletes the platform user associated with the given GitHub user.
    async fn delete_user(&self, user: &store::User) -> anyhow::Result<()>;
}

pub trait ManageAuthorizedKeys {
    /// Updates the authorized_keys for the given user based on their GitHub data.
    async fn update_authorized_keys(&self, user: &store::User) -> anyhow::Result<()>;
}

pub trait ManageSupplementaryGroups {
    /// Synchronizes supplementary groups for the given user.
    async fn sync_supplementary_groups(
        &self,
        user: &store::User,
        groups: &[String],
    ) -> anyhow::Result<()>;
}

pub trait UpdateUser {
    /// Updates the use name and home
    async fn update_user(
        &self,
        gh_user: &octocrab::models::Author,
        available_user: &store::User,
    ) -> anyhow::Result<store::User>;
}

#[cfg(target_os = "linux")]
pub type PlatformUserManager = linux::LinuxUserManager;
#[cfg(not(target_os = "linux"))]
pub type PlatformUserManager = mock::MockUserManager;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::public_keys;
    use anyhow::Context as _;
    use std::path;
    use tokio::{fs, process};

    #[derive(Clone, Debug)]
    pub struct LinuxUserManager {
        http_client: reqwest::Client,
    }

    impl LinuxUserManager {
        pub fn new() -> Self {
            Self {
                http_client: reqwest::Client::new(),
            }
        }
    }

    impl CreateUser for LinuxUserManager {
        /// Creates a
        #[tracing::instrument(name = "UserManager::create_user", skip(self, user))]
        async fn create_user(
            &self,
            user: &octocrab::models::Author,
        ) -> anyhow::Result<store::User> {
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

            let mut command = process::Command::new("/usr/sbin/useradd");
            command
                .arg("--create-home")
                .arg("--shell")
                .arg("/bin/bash")
                .arg("--password")
                .arg("!")
                .arg(&user.login);

            let proc = command.output();
            let o = proc
                .await
                .context("Failed to wait for useradd command to finish")?;

            if o.status.success() {
                tracing::info!("Created user");

                let linux_user = nix::unistd::User::from_name(&user.login)
                    .context("Failed to retrieve user info for newly created user ")?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "User '{}' was created but could not be found in the system",
                            user.login
                        )
                    })?;

                Ok(store::User::builder()
                    .id(user.id)
                    .uid(linux_user.uid)
                    .name(user.login.clone())
                    .build())
            } else {
                Err(anyhow::anyhow!(
                    "Failed to create user: {}",
                    String::from_utf8_lossy(&o.stderr)
                ))
            }
        }
    }

    impl DeleteUser for LinuxUserManager {
        #[tracing::instrument(name = "UserManager::delete_user", skip(self, user), fields(user = %user.name()))]
        async fn delete_user(&self, user: &store::User) -> anyhow::Result<()> {
            // Before deleting the user, we need to kill all their processes to ensure there are no running processes that would prevent deletion
            if let Some(linux_user) = nix::unistd::User::from_uid(user.uid())? {
                kill_processes_for_user(&linux_user).await?;
            } else {
                tracing::warn!(
                    "User not found in system when attempting to delete. Skipping process kill.",
                );
            }

            let proc = process::Command::new("/usr/sbin/userdel")
                .arg("--remove")
                .arg(user.name())
                .output();

            let o = proc
                .await
                .context("Failed to wait for userdel command to finish")?;

            if o.status.success() {
                tracing::info!("Deleted user");
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "Failed to delete user '{}': {}",
                    user.name(),
                    String::from_utf8_lossy(&o.stderr)
                ))
            }
        }
    }

    impl UpdateUser for LinuxUserManager {
        #[tracing::instrument(
            name = "UserManager::update_user",
            skip(self, gh_user, available_user),
            fields(from_uid = available_user.uid().as_raw(), from = %available_user.name(), to = %gh_user.login)
        )]
        async fn update_user(
            &self,
            gh_user: &octocrab::models::Author,
            available_user: &store::User,
        ) -> anyhow::Result<store::User> {
            let linux_user =
                nix::unistd::User::from_uid(available_user.uid())?.ok_or_else(|| {
                    anyhow::anyhow!("User not found in system when attempting to update user",)
                })?;

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
                    "Failed to update username: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Err(anyhow::anyhow!(
                    "Failed to update username for {}: {}",
                    linux_user.name,
                    String::from_utf8_lossy(&output.stderr)
                ))
            }
        }
    }

    impl ManageSupplementaryGroups for LinuxUserManager {
        #[tracing::instrument(name = "UserManager::sync_supplementary_groups", skip(self, user, groups), fields(user = %user.name()))]
        async fn sync_supplementary_groups(
            &self,
            user: &store::User,
            groups: &[String],
        ) -> anyhow::Result<()> {
            let groups = sanitize_groups(groups)?;
            ensure_groups_exist(&groups).await?;

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

            let supplementary_groups: Vec<String> = groups
                .iter()
                .filter(|group| group.as_str() != primary_group_name)
                .cloned()
                .collect();

            sync_user_supplementary_groups_by_name(&linux_user.name, &supplementary_groups).await
        }
    }

    fn sanitize_groups(groups: &[String]) -> anyhow::Result<Vec<String>> {
        let mut seen = std::collections::BTreeSet::new();
        for group in groups {
            let is_valid = !group.is_empty()
                && group.len() <= 32
                && !group.contains(',')
                && group
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');

            if !is_valid {
                anyhow::bail!(
                    "Invalid Linux group name '{}'. Allowed characters: [A-Za-z0-9_-], max length 32.",
                    group
                );
            }

            seen.insert(group.clone());
        }

        Ok(seen.into_iter().collect())
    }

    async fn ensure_groups_exist(groups: &[String]) -> anyhow::Result<()> {
        for group in groups {
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
                tracing::info!("Created missing group '{}'", group);
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
                "Synchronized supplementary groups for '{}' to {:?}",
                user_name,
                supplementary_groups
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

    impl ManageAuthorizedKeys for LinuxUserManager {
        async fn update_authorized_keys(&self, user: &store::User) -> anyhow::Result<()> {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            use tokio::io::AsyncWriteExt as _;

            let linux_user = nix::unistd::User::from_uid(user.uid())?.ok_or_else(|| {
                anyhow::anyhow!("User not found in system when updating authorized_keys",)
            })?;
            let ssh_dir = user.ssh_dir();
            ensure_ssh_dir_for_user(&linux_user, &ssh_dir).await?;
            let authorized_key_path = ssh_dir.join("authorized_keys");

            // TODO: Find a better way to determine if we need to update the keys than just always
            // writing them and updating the modified time.
            // Maybe we can store a hash of the keys in the store and compare it before writing?
            // We also need to limit the number of unverified GitHub requests (60 per hour)
            // let age = keys
            //     .modified()
            //     .map(|m| chrono::Utc::now().signed_duration_since(m));
            // if let Some(age) = age
            //     && age < chrono::Duration::seconds(3600)
            // {
            //     tracing::debug!(
            //         "Authorized keys file was modified less than newer than 1 hour",
            //         age = age
            //     );
            //     return Ok(());
            // }

            let fetched_keys = fetch_public_keys_for_user(&self.http_client, user).await?;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&authorized_key_path)
                .await?;

            file.write_all(fetched_keys.to_string().as_bytes()).await?;

            let metadata = fs::metadata(&authorized_key_path).await?;
            if metadata.permissions().mode() & AUTHORIZED_KEYS_PERMISSIONS
                != AUTHORIZED_KEYS_PERMISSIONS
            {
                tracing::info!(
                    "Setting permissions on '{}' to 600",
                    authorized_key_path.display(),
                );
                file.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .await
                    .context("Failed to set permissions on authorized_keys file")?;
            }

            if metadata.uid() != linux_user.uid.as_raw()
                || metadata.gid() != linux_user.gid.as_raw()
            {
                tracing::info!(
                    "Changing ownership of authorized_keys file for user '{}' to {}:{}",
                    user.name(),
                    linux_user.uid,
                    linux_user.gid
                );

                nix::unistd::chown(
                    &authorized_key_path,
                    Some(linux_user.uid),
                    Some(linux_user.gid),
                )?;
            }
            Ok(())
        }
    }

    const SSH_DIR_PERMISSIONS: u32 = 0o700;
    const AUTHORIZED_KEYS_PERMISSIONS: u32 = 0o600;

    async fn ensure_ssh_dir_for_user(
        user: &nix::unistd::User,
        ssh_dir: &path::Path,
    ) -> anyhow::Result<()> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        if !ssh_dir.exists() {
            tracing::info!("Creating .ssh directory at {:?}", ssh_dir);

            fs::create_dir(&ssh_dir).await?;
        }
        let metadata = fs::metadata(ssh_dir).await?;
        if !metadata.mode() & SSH_DIR_PERMISSIONS == SSH_DIR_PERMISSIONS {
            tracing::info!("Setting permissions on '{}' to 700", ssh_dir.display(),);
            fs::set_permissions(
                &ssh_dir,
                std::fs::Permissions::from_mode(SSH_DIR_PERMISSIONS),
            )
            .await?;
        }

        if metadata.uid() != user.uid.as_raw() || metadata.gid() != user.gid.as_raw() {
            tracing::info!(
                "Changing ownership of {} directory for user '{}' to {}:{}",
                ssh_dir.display(),
                user.name,
                user.uid,
                user.gid
            );
            nix::unistd::chown(ssh_dir, Some(user.uid), Some(user.gid))?;
        }

        Ok(())
    }

    #[tracing::instrument(name = "kill_processes", skip(user), fields(user = %user.name))]
    pub async fn kill_processes_for_user(user: &nix::unistd::User) -> anyhow::Result<()> {
        let uid = user.uid.as_raw();
        tokio::task::spawn_blocking(move || {
            if let Ok(procs) = procfs::process::all_processes() {
                for proc in procs.flatten() {
                    if let Ok(stat) = proc.status()
                        && stat.ruid == uid
                    {
                        let pid = nix::unistd::Pid::from_raw(proc.pid);
                        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);

                        tracing::debug!(pid = proc.pid, "Killed process");
                    }
                }
            }
        })
        .await?;

        Ok(())
    }

    async fn fetch_public_keys_for_user(
        http_client: &reqwest::Client,
        user: &store::User,
    ) -> anyhow::Result<public_keys::PublicKeys> {
        let keys = http_client
            .get(user.public_keys_url())
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        keys.parse()
    }
}

#[cfg(not(target_os = "linux"))]
mod mock {
    use super::*;
    use std::sync;

    #[derive(Clone, Debug, bon::Builder)]
    pub struct MockUserManager {
        uid_generator: AsyncCounter,
    }

    impl MockUserManager {
        pub fn new(base_uid: usize) -> Self {
            Self {
                uid_generator: AsyncCounter::new(base_uid),
            }
        }
    }

    impl CreateUser for MockUserManager {
        async fn create_user(
            &self,
            user: &octocrab::models::Author,
        ) -> anyhow::Result<store::User> {
            let uid = self.uid_generator.get_next();
            tracing::info!(
                "Mock creating user '{}' with UID {} (not actually creating users on non-Linux OS)",
                user.login,
                uid
            );
            Ok(store::User::builder()
                .name(user.login.clone())
                .uid(nix::unistd::Uid::from_raw(uid as _))
                .id(user.id)
                .build())
        }
    }

    impl DeleteUser for MockUserManager {
        #[tracing::instrument(name = "UserManager::delete_user", skip(self, user), fields(user = %user.name()))]
        async fn delete_user(&self, user: &store::User) -> anyhow::Result<()> {
            tracing::info!("Would delete user");
            Ok(())
        }
    }

    impl ManageAuthorizedKeys for MockUserManager {
        async fn update_authorized_keys(&self, user: &store::User) -> anyhow::Result<()> {
            tracing::info!(
                "Mock updating authorized keys for user '{}' (not actually managing keys on non-Linux OS)",
                user.name()
            );
            Ok(())
        }
    }

    impl UpdateUser for MockUserManager {
        async fn update_user(
            &self,
            gh_user: &octocrab::models::Author,
            available_user: &store::User,
        ) -> anyhow::Result<store::User> {
            if gh_user.login != available_user.name() {
                tracing::info!(
                    "Mock updating username from '{}' to '{}' (not actually updating users on non-Linux OS)",
                    available_user.name(),
                    gh_user.login
                );
                Ok(store::User::builder()
                    .id(available_user.id())
                    .uid(available_user.uid())
                    .name(gh_user.login.clone())
                    .build())
            } else {
                Ok(available_user.clone())
            }
        }
    }

    impl ManageSupplementaryGroups for MockUserManager {
        async fn sync_supplementary_groups(
            &self,
            user: &store::User,
            groups: &[String],
        ) -> anyhow::Result<()> {
            tracing::info!(
                "Mock syncing supplementary groups {:?} for user '{}' (not actually managing groups on non-Linux OS)",
                groups,
                user.name()
            );
            Ok(())
        }
    }

    #[derive(Clone, Debug)]
    pub struct AsyncCounter {
        // Arc allows multiple tasks to own a reference to this same atomic value
        inner: sync::Arc<sync::atomic::AtomicUsize>,
    }

    impl AsyncCounter {
        pub fn new(start: usize) -> Self {
            Self {
                inner: sync::Arc::new(sync::atomic::AtomicUsize::new(start)),
            }
        }

        // This function can be called from any task to get a unique, incremented number
        pub fn get_next(&self) -> usize {
            // fetch_add increments the value and returns the PREVIOUS value.
            // We add 1 to the result to return the "new" incremented number.
            self.inner.fetch_add(1, sync::atomic::Ordering::SeqCst) + 1
        }
    }
}
