use crate::Context;
use anyhow::Context as _;
use nix::unistd;
use serde::{Deserialize, Serialize};
use std::{collections, path, sync};
use tokio::{fs, io};

mod uid_serde {
    use serde::{self, Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(uid: &nix::unistd::Uid, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Use the public getter provided by the crate
        // Cast to u32 to keep the JSON format stable across OSs
        #[allow(clippy::unnecessary_cast)]
        (uid.as_raw() as u32).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<nix::unistd::Uid, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = u32::deserialize(deserializer)?;

        // Use the public constructor/factory provided by the crate
        // The 'as _' handles the platform-specific uid_t conversion
        Ok(nix::unistd::Uid::from_raw(val as _))
    }
}

/// Canonical representation of a user that exists both on GitHub and as a Linux user,
/// with the necessary information to manage their Linux account and SSH keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// GitHub user ID, used as the primary key for identifying users in the store
    id: octocrab::models::UserId,
    /// GitHub login/username, used for fetching user information and SSH keys from GitHub
    name: String,
    /// Linux user UID associated with this GitHub user
    #[serde(with = "uid_serde")]
    uid: unistd::Uid,
}

#[allow(unused)]
impl User {
    /// Get the home directory path for this user, typically "/home/{name}"
    pub fn home_dir(&self) -> path::PathBuf {
        path::PathBuf::from(format!("/home/{}", self.name))
    }

    /// Get the SSH directory path for this user, typically "/home/{name}/.ssh"
    pub fn ssh_dir(&self) -> path::PathBuf {
        self.home_dir().join(".ssh")
    }
}

type UserMap = collections::HashMap<octocrab::models::UserId, User>;

pub struct Store {
    _ctx: sync::Arc<Context>,
    dir: path::PathBuf,
    /// In-memory cache of users loaded from the members database, keyed by GitHub user ID
    users: UserMap,
}

impl Store {
    /// Create a new store instance with the given context and path to the members database
    pub async fn new(ctx: sync::Arc<Context>, dir: path::PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(&dir).await?;
        let mut s = Self {
            _ctx: ctx,
            dir,
            users: UserMap::new(),
        };
        s.load().await?;
        Ok(s)
    }

    /// Get the file path for a given user in the store
    fn user_path(&self) -> path::PathBuf {
        self.dir.join("users.json")
    }

    /// Load the store from the file system
    async fn load(&mut self) -> anyhow::Result<()> {
        self.users = self.load_users().await?;

        Ok(())
    }

    /// Load the users from the users database file, returning an empty map if the file doesn't exist
    async fn load_users(&self) -> anyhow::Result<UserMap> {
        match fs::read_to_string(self.user_path()).await {
            Ok(content) => Ok(serde_json::from_str(&content).with_context(|| {
                format!(
                    "Failed to parse users database from '{}'",
                    self.user_path().display()
                )
            })?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                log::info!(
                    "Users database file '{}' not found, starting with an empty user map",
                    self.user_path().display()
                );
                // If the file doesn't exist, start with an empty user map
                Ok(UserMap::new())
            }
            Err(e) => {
                // For other errors, return the error
                Err(e).with_context(|| {
                    format!(
                        "Failed to open users database file '{}'",
                        self.user_path().display()
                    )
                })
            }
        }
    }

    pub async fn save(&self) -> anyhow::Result<()> {
        let content = serde_json::to_string_pretty(&self.users)?;
        fs::write(self.user_path(), content).await?;
        Ok(())
    }
}
