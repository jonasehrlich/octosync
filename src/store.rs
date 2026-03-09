use anyhow::Context as _;
use nix::unistd;
use serde::{Deserialize, Serialize};
use std::{collections, path};
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
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

    pub fn public_keys_url(&self) -> String {
        format!("https://github.com/{}.keys", self.name)
    }

    /// Get the GitHub user ID
    pub fn id(&self) -> octocrab::models::UserId {
        self.id
    }

    /// Get the login/username
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the Linux user UID
    pub fn uid(&self) -> unistd::Uid {
        self.uid
    }
}

type UserMap = collections::HashMap<octocrab::models::UserId, User>;

const USERS_FILE_NAME: &str = "users.json";

#[derive(Debug)]
pub struct UserStore {
    dir: path::PathBuf,
    /// In-memory cache of users loaded from the members database, keyed by GitHub user ID
    users: UserMap,
}

impl UserStore {
    /// Create a new store instance with the given directory, without loading any data
    pub async fn new(dir: &path::Path) -> anyhow::Result<Self> {
        fs::create_dir_all(&dir).await?;
        Ok(Self {
            dir: dir.to_path_buf(),
            users: UserMap::new(),
        })
    }

    /// Create a new store loading data from the directory
    #[tracing::instrument(name = "Store::from_dir")]
    pub async fn from_dir(dir: &path::Path) -> anyhow::Result<Self> {
        let mut s = Self::new(dir).await?;
        s.load().await?;
        Ok(s)
    }

    pub fn data(&self) -> &UserMap {
        &self.users
    }

    pub fn data_mut(&mut self) -> &mut UserMap {
        &mut self.users
    }

    /// Get the file path for the users database file
    fn path(&self) -> path::PathBuf {
        self.dir.join(USERS_FILE_NAME)
    }

    /// Load the store from the file system
    async fn load(&mut self) -> anyhow::Result<()> {
        self.users = self.load_users().await?;

        Ok(())
    }

    /// Load the users from the users database file, returning an empty map if the file doesn't exist
    #[tracing::instrument(name = "Store::load_users", skip(self))]
    async fn load_users(&self) -> anyhow::Result<UserMap> {
        let path = self.path();
        tracing::debug!("Loading users '{}'", path.display());

        match fs::read_to_string(&path).await {
            Ok(content) => Ok(serde_json::from_str(&content).with_context(|| {
                format!("Failed to parse users database from '{}'", path.display())
            })?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tracing::info!(
                    "Users database file '{}' not found, starting with an empty user map",
                    path.display()
                );
                // If the file doesn't exist, start with an empty user map
                Ok(UserMap::new())
            }
            Err(e) => {
                // For other errors, return the error
                Err(e).with_context(|| {
                    format!("Failed to open users database file '{}'", path.display())
                })
            }
        }
    }

    pub async fn save(&self) -> anyhow::Result<()> {
        let content = serde_json::to_string_pretty(&self.users)?;
        fs::write(self.path(), content).await?;
        Ok(())
    }

    pub async fn delete(self) -> anyhow::Result<()> {
        fs::remove_file(self.path()).await.or_else(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e).with_context(|| {
                    format!(
                        "Failed to delete users database file '{}'",
                        self.path().display()
                    )
                })
            }
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> User {
        User {
            id: octocrab::models::UserId(12345),
            name: "testuser".to_string(),
            uid: unistd::Uid::from_raw(1000),
        }
    }

    mod user {
        use super::*;

        #[test]
        fn home_dir() {
            let user = user();
            assert_eq!(user.home_dir(), path::PathBuf::from("/home/testuser"));
        }

        #[test]
        fn ssh_dir() {
            let user = user();
            assert_eq!(user.ssh_dir(), path::PathBuf::from("/home/testuser/.ssh"));
        }

        #[test]
        fn serialization() {
            let user = user();
            let serialized = serde_json::to_string(&user).expect("Failed to serialize user");
            assert!(serialized.contains("\"id\":12345"));
            assert!(serialized.contains("\"name\":\"testuser\""));
            assert!(serialized.contains("\"uid\":1000"));
        }

        #[test]
        fn deserialization() {
            let json = r#"{
                "id": 12345,
                "name": "testuser",
                "uid": 1000
            }"#;
            let expected_user = user();

            let user: User = serde_json::from_str(json).expect("Failed to deserialize user");
            assert_eq!(user, expected_user);
        }

        #[test]
        fn round_trip_serialization() {
            let original = User {
                id: octocrab::models::UserId(99999),
                name: "roundtripuser".to_string(),
                uid: unistd::Uid::from_raw(2000),
            };

            let serialized = serde_json::to_string(&original).expect("Failed to serialize");
            let deserialized: User =
                serde_json::from_str(&serialized).expect("Failed to deserialize");

            assert_eq!(original.id, deserialized.id);
            assert_eq!(original.name, deserialized.name);
            assert_eq!(original.uid.as_raw(), deserialized.uid.as_raw());
        }
    }

    mod user_store {
        use super::*;

        #[tokio::test]
        async fn new_creates_directory() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            let store_path = temp_dir.path().join("store");

            let store = UserStore::from_dir(&store_path)
                .await
                .expect("Failed to create store");

            assert!(store_path.exists());
            assert!(store.users.is_empty());
        }

        #[tokio::test]
        async fn new_with_existing_directory() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            // Create store in existing directory
            let store = UserStore::from_dir(temp_dir.path())
                .await
                .expect("Failed to create store");

            assert!(store.users.is_empty());
        }

        #[tokio::test]
        async fn save_and_load() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            // Create a store and add some users
            let _store = UserStore::new(temp_dir.path())
                .await
                .expect("Failed to create store");

            // Manually add users to the store
            let user1 = User {
                id: octocrab::models::UserId(111),
                name: "user1".to_string(),
                uid: unistd::Uid::from_raw(1001),
            };
            let user2 = User {
                id: octocrab::models::UserId(222),
                name: "user2".to_string(),
                uid: unistd::Uid::from_raw(1002),
            };

            // We need to access the private field for testing - this is a limitation
            // In a real scenario, you might want to add methods to add/remove users
            // For now, we'll test the save/load functionality by writing the file manually
            let mut users = UserMap::new();
            users.insert(user1.id, user1);
            users.insert(user2.id, user2);

            let content = serde_json::to_string_pretty(&users).expect("Failed to serialize");
            fs::write(temp_dir.path().join(USERS_FILE_NAME), content)
                .await
                .expect("Failed to write users file");

            // Now load the store
            let loaded_store = UserStore::from_dir(temp_dir.path())
                .await
                .expect("Failed to load store");

            assert_eq!(loaded_store.users.len(), 2);
            assert!(
                loaded_store
                    .users
                    .contains_key(&octocrab::models::UserId(111))
            );
            assert!(
                loaded_store
                    .users
                    .contains_key(&octocrab::models::UserId(222))
            );

            let loaded_user1 = &loaded_store.users[&octocrab::models::UserId(111)];
            assert_eq!(loaded_user1.name, "user1");
            assert_eq!(loaded_user1.uid.as_raw(), 1001);

            let loaded_user2 = &loaded_store.users[&octocrab::models::UserId(222)];
            assert_eq!(loaded_user2.name, "user2");
            assert_eq!(loaded_user2.uid.as_raw(), 1002);
        }

        #[tokio::test]
        async fn load_nonexistent_file() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            // Create store in a directory without users.json
            let store = UserStore::from_dir(temp_dir.path())
                .await
                .expect("Failed to create store");

            // Should start with empty user map
            assert!(store.users.is_empty());
        }

        #[tokio::test]
        async fn save() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            let mut store = UserStore::new(temp_dir.path())
                .await
                .expect("Failed to create store");

            // Add users
            let user = User {
                id: octocrab::models::UserId(333),
                name: "user3".to_string(),
                uid: unistd::Uid::from_raw(1003),
            };
            store.users.insert(user.id, user);

            // Save the store
            store.save().await.expect("Failed to save store");

            // Verify the file was created
            let users_file = temp_dir.path().join(USERS_FILE_NAME);
            assert!(users_file.exists());

            // Read and verify contents
            let content = fs::read_to_string(&users_file)
                .await
                .expect("Failed to read users file");
            let loaded_users: UserMap =
                serde_json::from_str(&content).expect("Failed to parse users file");

            assert_eq!(loaded_users.len(), 1);
            assert!(loaded_users.contains_key(&octocrab::models::UserId(333)));

            let loaded_user = &loaded_users[&octocrab::models::UserId(333)];
            assert_eq!(loaded_user.name, "user3");
            assert_eq!(loaded_user.uid.as_raw(), 1003);
        }

        #[tokio::test]
        async fn round_trip() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            // Create store and add users
            let mut store = UserStore::from_dir(temp_dir.path())
                .await
                .expect("Failed to create store");

            let user1 = User {
                id: octocrab::models::UserId(444),
                name: "roundtrip1".to_string(),
                uid: unistd::Uid::from_raw(2001),
            };
            let user2 = User {
                id: octocrab::models::UserId(555),
                name: "roundtrip2".to_string(),
                uid: unistd::Uid::from_raw(2002),
            };

            store.users.insert(user1.id, user1);
            store.users.insert(user2.id, user2);

            // Save
            store.save().await.expect("Failed to save store");

            // Load in a new store instance
            let loaded_store = UserStore::from_dir(temp_dir.path())
                .await
                .expect("Failed to load store");

            // Verify all data matches
            assert_eq!(loaded_store.users.len(), 2);
            assert_eq!(
                loaded_store.users[&octocrab::models::UserId(444)].name,
                "roundtrip1"
            );
            assert_eq!(
                loaded_store.users[&octocrab::models::UserId(444)]
                    .uid
                    .as_raw(),
                2001
            );
            assert_eq!(
                loaded_store.users[&octocrab::models::UserId(555)].name,
                "roundtrip2"
            );
            assert_eq!(
                loaded_store.users[&octocrab::models::UserId(555)]
                    .uid
                    .as_raw(),
                2002
            );
        }

        #[tokio::test]
        async fn user_path() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            let store = UserStore::new(temp_dir.path())
                .await
                .expect("Failed to create store");

            let expected_path = temp_dir.path().join(USERS_FILE_NAME);
            assert_eq!(store.path(), expected_path);
        }

        #[tokio::test]
        async fn load_invalid_json() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            // Write invalid JSON to users.json
            fs::write(
                temp_dir.path().join(USERS_FILE_NAME),
                "{ invalid json content",
            )
            .await
            .expect("Failed to write invalid JSON");

            // Attempting to create/load the store should fail
            let result = UserStore::from_dir(temp_dir.path()).await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn multiple_users() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            let mut store = UserStore::new(temp_dir.path())
                .await
                .expect("Failed to create store");

            // Add multiple users
            for i in 0..10 {
                let user = User {
                    id: octocrab::models::UserId(1000 + i),
                    name: format!("user{}", i),
                    uid: unistd::Uid::from_raw(3000 + i as u32),
                };
                store.users.insert(user.id, user);
            }

            // Save
            store.save().await.expect("Failed to save store");

            // Load in new instance
            let loaded_store = UserStore::from_dir(temp_dir.path())
                .await
                .expect("Failed to load store");

            // Verify all users loaded correctly
            assert_eq!(loaded_store.users.len(), 10);
            for i in 0..10 {
                let user_id = octocrab::models::UserId(1000 + i);
                assert!(loaded_store.users.contains_key(&user_id));
                assert_eq!(loaded_store.users[&user_id].name, format!("user{}", i));
                assert_eq!(loaded_store.users[&user_id].uid.as_raw(), 3000 + i as u32);
            }
        }
    }
}
