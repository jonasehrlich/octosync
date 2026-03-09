use crate::store;

pub trait CreateUser {
    /// Creates a platform user for the given GitHub user.
    async fn create_user(
        &self,
        user: &octocrab::models::Author,
        groups: Vec<String>,
    ) -> anyhow::Result<store::User>;
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
            groups: Vec<String>,
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
            if groups.len() > 0 {
                unimplemented!(
                    "Group management is not yet implemented. Cannot add user to groups: {:?}",
                    groups
                );
                // command.arg("--groups").arg(groups.join(","));
            }
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
                tracing::info!("Created user with additional groups: {:?}", groups);

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
        #[tracing::instrument(name = "UserManager::delete_user", skip(self, user))]
        async fn delete_user(&self, user: &store::User) -> anyhow::Result<()> {
            // Before deleting the user, we need to kill all their processes to ensure there are no running processes that would prevent deletion
            if let Some(linux_user) = nix::unistd::User::from_uid(user.uid())? {
                kill_all_processes_for_user(&linux_user).await?;
            } else {
                tracing::warn!(
                    "User not found in system when attempting to delete. Skipping process kill.",
                );
            }

            let proc = process::Command::new("/usr/sbin/userdel")
                .arg("--remove")
                .arg(&user.name())
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

    /// Forcefully kills all processes owned by the given user.
    /// This is used before modifying or deleting a user to ensure there are no running processes
    /// that would prevent deletion.
    async fn kill_all_processes_for_user(user: &nix::unistd::User) -> anyhow::Result<()> {
        tracing::info!("Kill all processes");
        let proc = process::Command::new("/usr/bin/killall")
            .arg("--signal")
            .arg("KILL")
            .arg("--user")
            .arg(&user.name)
            .output();

        let o = proc.await.with_context(|| {
            format!(
                "Failed to wait for killall command to finish when killing processes for user '{}'",
                user.name
            )
        })?;

        if o.status.success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Failed to kill processes for user '{}': {}",
                user.name,
                String::from_utf8_lossy(&o.stderr)
            ))
        }
    }

    /// Get the appropriate admin group for the current Linux distribution.
    #[allow(unused)]
    async fn get_admin_group() -> &'static str {
        if let Ok(os_info) = fs::read_to_string("/etc/os-release").await {
            let os_info = os_info.to_lowercase();
            if os_info.contains("fedora")
                || os_info.contains("arch")
                || os_info.contains("rhel")
                || os_info.contains("centos")
            {
                return "wheel";
            }
        }
        // Default to sudo for Debian/Ubuntu and others
        "sudo"
    }

    async fn fetch_public_keys_for_user(
        http_client: &reqwest::Client,
        user: &store::User,
    ) -> anyhow::Result<public_keys::PublicKeys> {
        let keys = http_client
            .get(&user.public_keys_url())
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(keys.parse()?)
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
            _groups: Vec<String>,
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
