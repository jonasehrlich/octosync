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
    async fn delete_user(&self, user: &octocrab::models::Author) -> anyhow::Result<()>;
}

#[cfg(target_os = "linux")]
pub type PlatformUserManager = linux::LinuxUserManager;
#[cfg(not(target_os = "linux"))]
pub type PlatformUserManager = mock::MockUserManager;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use anyhow::Context as _;
    use tokio::{fs, process};

    pub struct LinuxUserManager {}

    impl LinuxUserManager {
        pub fn new() -> Self {
            Self {}
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
        async fn delete_user(&self, user: &octocrab::models::Author) -> anyhow::Result<()> {
            // Before deleting the user, we need to kill all their processes to ensure there are no running processes that would prevent deletion
            if let Some(linux_user) = nix::unistd::User::from_name(&user.login)? {
                kill_all_processes_for_user(&linux_user).await?;
            } else {
                tracing::warn!(
                    "User not found in system when attempting to delete. Skipping process kill.",
                );
            }

            let proc = process::Command::new("/usr/sbin/userdel")
                .arg("--remove")
                .arg(&user.login)
                .output();

            let o = proc
                .await
                .context("Failed to wait for userdel command to finish ")?;

            if o.status.success() {
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "Failed to delete user '{}': {}",
                    user.login,
                    String::from_utf8_lossy(&o.stderr)
                ))
            }
        }
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
        async fn delete_user(&self, user: &octocrab::models::Author) -> anyhow::Result<()> {
            tracing::info!(
                "Mock deleting user '{}' (not actually deleting users on non-Linux OS)",
                user.login
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
