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

/// Tombstone of a user whose account octosync deleted (or is deleting). It keeps the
/// UID so a rejoining member gets their old UID back and file ownership outside the
/// archived home directory survives the delete and re-create cycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
pub struct ArchivedUser {
    /// GitHub user ID, used as the primary key for identifying users in the store
    id: octocrab::models::UserId,
    /// GitHub login at the time of deletion
    name: String,
    /// Linux user UID the account had, reserved for a rejoin
    #[serde(with = "uid_serde")]
    uid: unistd::Uid,
    /// When the user was archived for deletion
    deleted_at: chrono::DateTime<chrono::Utc>,
    /// Path of the archived home directory, so a rejoin can later restore it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    home_archive: Option<path::PathBuf>,
}

#[allow(unused)]
impl ArchivedUser {
    /// Get the GitHub user ID
    pub fn id(&self) -> octocrab::models::UserId {
        self.id
    }

    /// Get the login the user had when they were archived
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the Linux user UID reserved for a rejoin
    pub fn uid(&self) -> unistd::Uid {
        self.uid
    }

    /// Get the time the user was archived for deletion
    pub fn deleted_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.deleted_at
    }

    /// Get the path of the archived home directory, if one was created
    pub fn home_archive(&self) -> Option<&path::Path> {
        self.home_archive.as_deref()
    }
}

impl From<&ArchivedUser> for User {
    /// Account identity of the tombstone, used to address platform operations
    fn from(archived: &ArchivedUser) -> Self {
        Self {
            id: archived.id,
            name: archived.name.clone(),
            uid: archived.uid,
        }
    }
}

pub type UserMap = collections::HashMap<octocrab::models::UserId, User>;
pub type ArchivedMap = collections::HashMap<octocrab::models::UserId, ArchivedUser>;

const USERS_FILE_NAME: &str = "users.json";
const USERS_V1_FILE_NAME: &str = "users-v1.json";
const STORE_VERSION: u64 = 2;

/// On-disk schema of the users database. A v1 file is a bare [`UserMap`] without the
/// `version` key and is migrated on load; [`UserStore::save`] always writes v2.
#[derive(Deserialize)]
struct StoreData {
    users: UserMap,
    #[serde(default)]
    archived: ArchivedMap,
}

/// Borrowing counterpart of [`StoreData`] for serialization
#[derive(Serialize)]
struct StoreDataRef<'a> {
    version: u64,
    users: &'a UserMap,
    archived: &'a ArchivedMap,
}

#[derive(Debug)]
pub struct UserStore {
    dir: path::PathBuf,
    /// In-memory cache of users loaded from the members database, keyed by GitHub user ID
    users: UserMap,
    /// Tombstones of deleted users, keyed by GitHub user ID. Kept separate from the
    /// active users so no consumer can process a tombstone as an active user.
    archived: ArchivedMap,
}

impl UserStore {
    /// Create a new store instance with the given directory, without loading any data
    pub async fn new(dir: &path::Path) -> anyhow::Result<Self> {
        fs::create_dir_all(&dir).await?;
        Ok(Self {
            dir: dir.to_path_buf(),
            users: UserMap::new(),
            archived: ArchivedMap::new(),
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

    pub fn archived(&self) -> &ArchivedMap {
        &self.archived
    }

    pub fn archived_mut(&mut self) -> &mut ArchivedMap {
        &mut self.archived
    }

    /// Turn a user into a tombstone, keeping their UID for a later rejoin
    pub fn archive_user(&mut self, user: User, deleted_at: chrono::DateTime<chrono::Utc>) {
        self.users.remove(&user.id);
        self.archived.insert(
            user.id,
            ArchivedUser {
                id: user.id,
                name: user.name,
                uid: user.uid,
                deleted_at,
                home_archive: None,
            },
        );
    }

    /// Record the home directory archive of an archived user
    pub fn record_home_archive(&mut self, id: &octocrab::models::UserId, archive: path::PathBuf) {
        if let Some(archived) = self.archived.get_mut(id) {
            archived.home_archive = Some(archive);
        }
    }

    /// Drop the tombstones of users that are active again, keeping every user either
    /// active or archived but never both
    pub fn prune_rejoined(&mut self) {
        self.archived.retain(|id, _| !self.users.contains_key(id));
    }

    /// Get the file path for the users database file
    fn path(&self) -> path::PathBuf {
        self.dir.join(USERS_FILE_NAME)
    }

    /// Load the store from the file system, starting empty if the file doesn't exist.
    /// A v1 file is backed up once and migrated with an empty archived map.
    #[tracing::instrument(name = "Store::load", skip(self))]
    async fn load(&mut self) -> anyhow::Result<()> {
        let path = self.path();
        tracing::debug!("Loading users '{}'", path.display());

        let content = match fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tracing::info!(
                    "Users database file '{}' not found, starting with an empty user map",
                    path.display()
                );
                return Ok(());
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to open users database file '{}'", path.display())
                });
            }
        };

        let value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse users database from '{}'", path.display()))?;

        let data = match value.get("version") {
            // A v1 file is a bare user map without a version key
            None => {
                let users: UserMap = serde_json::from_value(value).with_context(|| {
                    format!(
                        "Failed to parse v1 users database from '{}'",
                        path.display()
                    )
                })?;
                // save() overwrites the file with v2, back it up first so rolling back
                // to an older binary keeps working
                self.backup_v1_file().await?;
                StoreData {
                    users,
                    archived: ArchivedMap::new(),
                }
            }
            Some(version) if version.as_u64() == Some(STORE_VERSION) => {
                serde_json::from_value(value).with_context(|| {
                    format!("Failed to parse users database from '{}'", path.display())
                })?
            }
            // Refuse an unknown newer version instead of silently dropping its data
            Some(version) => anyhow::bail!(
                "Unsupported users database version {version} in '{}', this binary supports version {STORE_VERSION}",
                path.display()
            ),
        };

        self.users = data.users;
        self.archived = data.archived;
        Ok(())
    }

    /// Copy the v1 users database to `users-v1.json`
    async fn backup_v1_file(&self) -> anyhow::Result<()> {
        let backup = self.dir.join(USERS_V1_FILE_NAME);
        fs::copy(self.path(), &backup).await.with_context(|| {
            format!(
                "Failed to back up the v1 users database to '{}'",
                backup.display()
            )
        })?;
        tracing::info!(
            "Migrating users database to v2, backed up the v1 file to '{}'",
            backup.display()
        );
        Ok(())
    }

    pub async fn save(&self) -> anyhow::Result<()> {
        let content = serde_json::to_string_pretty(&StoreDataRef {
            version: STORE_VERSION,
            users: &self.users,
            archived: &self.archived,
        })?;
        let path = self.path();
        // Stage in a temporary file that atomically replaces the database on commit, so a
        // failed write (e.g. on a full disk) can not truncate it
        let dest = path.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::Write as _;

            let mut file = atomic_write_file::AtomicWriteFile::open(&dest)?;
            file.write_all(content.as_bytes())?;
            file.commit()
        })
        .await
        .context("Atomic write task failed")?
        .with_context(|| format!("Failed to write users database file '{}'", path.display()))?;
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

    fn deleted_at() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-08-14T09:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn archived_user() -> ArchivedUser {
        ArchivedUser {
            id: octocrab::models::UserId(456),
            name: "bob".to_string(),
            uid: unistd::Uid::from_raw(1005),
            deleted_at: deleted_at(),
            home_archive: Some(path::PathBuf::from(
                "/var/lib/octosync/home-archive/bob.tar.gz",
            )),
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

    mod archived_user {
        use super::*;

        #[test]
        fn round_trip_serialization() {
            let original = archived_user();

            let serialized = serde_json::to_string(&original).expect("Failed to serialize");
            let deserialized: ArchivedUser =
                serde_json::from_str(&serialized).expect("Failed to deserialize");

            assert_eq!(original, deserialized);
        }

        #[test]
        fn deleted_at_is_serialized_as_rfc3339() {
            let serialized = serde_json::to_string(&archived_user()).unwrap();
            assert!(serialized.contains("\"deleted_at\":\"2026-08-14T09:00:00Z\""));
        }

        #[test]
        fn missing_home_archive_is_omitted_and_parses_back() {
            let mut archived = archived_user();
            archived.home_archive = None;

            let serialized = serde_json::to_string(&archived).unwrap();
            assert!(!serialized.contains("home_archive"));

            let deserialized: ArchivedUser = serde_json::from_str(&serialized).unwrap();
            assert_eq!(deserialized.home_archive, None);
        }

        #[test]
        fn account_identity_conversion() {
            let archived = archived_user();
            let user = User::from(&archived);
            assert_eq!(user.id, archived.id);
            assert_eq!(user.name, archived.name);
            assert_eq!(user.uid, archived.uid);
        }
    }

    mod user_store {
        use super::*;

        /// Write a users database file with the given content
        async fn write_users_file(dir: &path::Path, content: &str) {
            fs::write(dir.join(USERS_FILE_NAME), content)
                .await
                .expect("Failed to write users file");
        }

        /// A v1 users database: a bare user map without a version key
        fn v1_content() -> String {
            let mut users = UserMap::new();
            let user = user();
            users.insert(user.id, user);
            serde_json::to_string_pretty(&users).unwrap()
        }

        #[tokio::test]
        async fn new_creates_directory() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            let store_path = temp_dir.path().join("store");

            let store = UserStore::from_dir(&store_path)
                .await
                .expect("Failed to create store");

            assert!(store_path.exists());
            assert!(store.users.is_empty());
            assert!(store.archived.is_empty());
        }

        #[tokio::test]
        async fn load_nonexistent_file() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            let store = UserStore::from_dir(temp_dir.path())
                .await
                .expect("Failed to create store");

            assert!(store.users.is_empty());
            assert!(store.archived.is_empty());
            // No file, nothing to back up
            assert!(!temp_dir.path().join(USERS_V1_FILE_NAME).exists());
        }

        #[tokio::test]
        async fn v1_file_is_migrated_and_backed_up() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), &v1_content()).await;

            let store = UserStore::from_dir(temp_dir.path())
                .await
                .expect("Failed to load store");

            assert_eq!(store.users.len(), 1);
            assert_eq!(
                store.users[&octocrab::models::UserId(12345)].name,
                "testuser"
            );
            assert!(store.archived.is_empty());

            // The backup preserves the v1 file byte for byte
            let backup = fs::read_to_string(temp_dir.path().join(USERS_V1_FILE_NAME))
                .await
                .expect("Backup file was not created");
            assert_eq!(backup, v1_content());
        }

        #[tokio::test]
        async fn save_after_v1_migration_writes_v2() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), &v1_content()).await;

            let store = UserStore::from_dir(temp_dir.path()).await.unwrap();
            store.save().await.expect("Failed to save store");

            let content = fs::read_to_string(temp_dir.path().join(USERS_FILE_NAME))
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert_eq!(value["version"], 2);
            assert!(value["users"]["12345"].is_object());
            assert_eq!(value["archived"], serde_json::json!({}));
        }

        #[tokio::test]
        async fn unknown_newer_version_is_an_error() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), r#"{ "version": 3, "users": {} }"#).await;

            let err = UserStore::from_dir(temp_dir.path()).await.unwrap_err();
            assert!(err.to_string().contains("version 3"));
        }

        #[tokio::test]
        async fn round_trip_with_archived_users() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            let mut store = UserStore::from_dir(temp_dir.path())
                .await
                .expect("Failed to create store");
            let active = user();
            let archived = archived_user();
            store.users.insert(active.id, active.clone());
            store.archived.insert(archived.id, archived.clone());

            store.save().await.expect("Failed to save store");

            let loaded = UserStore::from_dir(temp_dir.path())
                .await
                .expect("Failed to load store");
            assert_eq!(loaded.users[&active.id], active);
            assert_eq!(loaded.archived[&archived.id], archived);
            // The file was already v2, no v1 backup is created
            assert!(!temp_dir.path().join(USERS_V1_FILE_NAME).exists());
        }

        #[tokio::test]
        async fn v2_file_without_archived_key_parses() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), r#"{ "version": 2, "users": {} }"#).await;

            let store = UserStore::from_dir(temp_dir.path()).await.unwrap();
            assert!(store.users.is_empty());
            assert!(store.archived.is_empty());
        }

        #[tokio::test]
        async fn archive_user_moves_an_active_user_to_the_tombstones() {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let mut store = UserStore::new(temp_dir.path()).await.unwrap();
            let user = user();
            store.users.insert(user.id, user.clone());

            store.archive_user(user.clone(), deleted_at());

            assert!(store.users.is_empty());
            let archived = &store.archived[&user.id];
            assert_eq!(archived.name, user.name);
            assert_eq!(archived.uid, user.uid);
            assert_eq!(archived.deleted_at, deleted_at());
            assert_eq!(archived.home_archive, None);
        }

        #[tokio::test]
        async fn record_home_archive_sets_the_path_on_the_tombstone() {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let mut store = UserStore::new(temp_dir.path()).await.unwrap();
            let user = user();
            store.archive_user(user.clone(), deleted_at());

            let archive = path::PathBuf::from("/data/home-archive/testuser.tar.gz");
            store.record_home_archive(&user.id, archive.clone());

            assert_eq!(store.archived[&user.id].home_archive, Some(archive));
        }

        #[tokio::test]
        async fn prune_rejoined_drops_tombstones_of_active_users() {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let mut store = UserStore::new(temp_dir.path()).await.unwrap();
            let rejoined = user();
            let departed = archived_user();
            store
                .archived
                .insert(rejoined.id, ArchivedUser::from_test_user(&rejoined));
            store.archived.insert(departed.id, departed.clone());
            store.users.insert(rejoined.id, rejoined.clone());

            store.prune_rejoined();

            assert!(!store.archived.contains_key(&rejoined.id));
            assert_eq!(store.archived[&departed.id], departed);
        }

        #[tokio::test]
        async fn load_invalid_json() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), "{ invalid json content").await;

            let result = UserStore::from_dir(temp_dir.path()).await;
            assert!(result.is_err());
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
        async fn multiple_users() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            let mut store = UserStore::new(temp_dir.path())
                .await
                .expect("Failed to create store");

            for i in 0..10 {
                let user = User {
                    id: octocrab::models::UserId(1000 + i),
                    name: format!("user{}", i),
                    uid: unistd::Uid::from_raw(3000 + i as u32),
                };
                store.users.insert(user.id, user);
            }

            store.save().await.expect("Failed to save store");

            let loaded_store = UserStore::from_dir(temp_dir.path())
                .await
                .expect("Failed to load store");

            assert_eq!(loaded_store.users.len(), 10);
            for i in 0..10 {
                let user_id = octocrab::models::UserId(1000 + i);
                assert!(loaded_store.users.contains_key(&user_id));
                assert_eq!(loaded_store.users[&user_id].name, format!("user{}", i));
                assert_eq!(loaded_store.users[&user_id].uid.as_raw(), 3000 + i as u32);
            }
        }
    }

    impl ArchivedUser {
        /// Tombstone with the identity of `user`, for tests
        fn from_test_user(user: &User) -> Self {
            Self {
                id: user.id,
                name: user.name.clone(),
                uid: user.uid,
                deleted_at: deleted_at(),
                home_archive: None,
            }
        }
    }
}
