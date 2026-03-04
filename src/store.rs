use crate::Context;
use anyhow::Context as _;
use nix::unistd;
use serde::{Deserialize, Serialize};
use std::{collections, fs, io, path, sync};

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
struct User {
    id: octocrab::models::UserId,
    name: String,
    #[serde(with = "uid_serde")]
    uid: unistd::Uid,
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
    pub fn new(ctx: sync::Arc<Context>, dir: path::PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(&dir)?;
        let mut s = Self {
            _ctx: ctx,
            dir,
            users: UserMap::new(),
        };
        s.load()?;
        Ok(s)
    }

    /// Get the file path for a given user in the store
    fn user_path(&self) -> path::PathBuf {
        self.dir.join("users.json")
    }

    /// Load the store from the file system
    fn load(&mut self) -> anyhow::Result<()> {
        self.users = self.load_users()?;

        Ok(())
    }

    /// Load the users from the users database file, returning an empty map if the file doesn't exist
    fn load_users(&self) -> anyhow::Result<UserMap> {
        match fs::File::open(self.user_path()) {
            Ok(file) => {
                let reader = io::BufReader::new(file);
                Ok(serde_json::from_reader(reader).with_context(|| {
                    format!(
                        "Failed to parse users database from '{}'",
                        self.user_path().display()
                    )
                })?)
            }
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

    pub fn save(&self) -> anyhow::Result<()> {
        let writer = io::BufWriter::new(fs::File::create(self.user_path()).with_context(|| {
            format!(
                "Failed to create users database file '{}'",
                self.user_path().display()
            )
        })?);
        serde_json::to_writer_pretty(writer, &self.users)?;
        Ok(())
    }
}
