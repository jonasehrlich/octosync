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
        // Keep the JSON format stable across platforms.
        #[allow(clippy::unnecessary_cast)]
        (uid.as_raw() as u32).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<nix::unistd::Uid, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = u32::deserialize(deserializer)?;

        Ok(nix::unistd::Uid::from_raw(val as _))
    }
}

mod gid_serde {
    use serde::{self, Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(gid: &Option<nix::unistd::Gid>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[allow(clippy::unnecessary_cast)]
        gid.map(|gid| gid.as_raw() as u32).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<nix::unistd::Gid>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = Option::<u32>::deserialize(deserializer)?;
        Ok(val.map(|val| nix::unistd::Gid::from_raw(val as _)))
    }
}

/// A GitHub member and their Linux account identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
pub struct User {
    /// GitHub user ID, used as the primary key for identifying users in the store
    id: octocrab::models::UserId,
    /// GitHub login/username, used for fetching user information and SSH keys from GitHub
    name: String,
    /// Linux user UID associated with this GitHub user
    #[serde(with = "uid_serde")]
    uid: unistd::Uid,
    /// GID of the user's primary group. `None` for entries migrated from v1, whose
    /// account may be gone from the system and have no GID to read. Backfilled from
    /// the system on the next update of the user.
    #[serde(default, with = "gid_serde", skip_serializing_if = "Option::is_none")]
    gid: Option<unistd::Gid>,
}

impl User {
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

    /// Get the Linux user GID of the primary group
    pub fn gid(&self) -> Option<unistd::Gid> {
        self.gid
    }
}

/// Tombstone of a member who left the org. Their account stays on the machine expired,
/// and the tombstone keeps the IDs so a rejoining member gets their old UID and GID
/// back and file ownership survives the departure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
pub struct DepartedUser {
    /// GitHub user ID, used as the primary key for identifying users in the store
    id: octocrab::models::UserId,
    /// GitHub login at the time of departure
    name: String,
    /// Linux user UID the account has, reserved for a rejoin
    #[serde(with = "uid_serde")]
    uid: unistd::Uid,
    /// GID of the primary group the account has, reserved for a rejoin. `None` when
    /// the user departed before their GID was backfilled.
    #[serde(default, with = "gid_serde", skip_serializing_if = "Option::is_none")]
    gid: Option<unistd::Gid>,
    /// When the member departed and their account was expired
    departed_at: chrono::DateTime<chrono::Utc>,
    /// When the account teardown completed: the account expired, its scheduled work
    /// removed, its sessions ended and its supplementary groups stripped. `None` while
    /// the teardown has not run or did not finish, which is what makes the sync retry
    /// it; a tombstone that carries the timestamp is left alone for the rest of the
    /// retention period.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expired_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When octosync started removing the account. This is persisted before `userdel`
    /// runs and retained when the result is ambiguous, allowing the next run to finish
    /// updating the departure record if the account is already gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    purge_started_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[allow(unused)]
impl DepartedUser {
    /// Get the GitHub user ID
    pub fn id(&self) -> octocrab::models::UserId {
        self.id
    }

    /// Get the login the user had when they departed
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the Linux user UID reserved for a rejoin
    pub fn uid(&self) -> unistd::Uid {
        self.uid
    }

    /// Get the GID of the primary group reserved for a rejoin, if it is tracked
    pub fn gid(&self) -> Option<unistd::Gid> {
        self.gid
    }

    /// Get the time the member departed
    pub fn departed_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.departed_at
    }

    /// Get the time the account teardown completed, `None` while it still has to run
    pub fn expired_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.expired_at
    }

    /// Get when account removal started but was not confirmed complete.
    pub fn purge_started_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.purge_started_at
    }
}

impl From<&DepartedUser> for User {
    /// Account identity of the tombstone, used to address platform operations
    fn from(departed: &DepartedUser) -> Self {
        Self {
            id: departed.id,
            name: departed.name.clone(),
            uid: departed.uid,
            gid: departed.gid,
        }
    }
}

/// A purged member whose IDs remain reserved for a rejoin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
pub struct PurgedUser {
    /// GitHub user ID, used as the primary key for identifying users in the store
    id: octocrab::models::UserId,
    /// GitHub login at the time of departure
    name: String,
    /// Linux user UID the account had, reserved for a rejoin
    #[serde(with = "uid_serde")]
    uid: unistd::Uid,
    /// GID of the primary group the account had, reserved for a rejoin
    #[serde(default, with = "gid_serde", skip_serializing_if = "Option::is_none")]
    gid: Option<unistd::Gid>,
    /// When the member departed and their account was expired
    departed_at: chrono::DateTime<chrono::Utc>,
    /// When the expired account and its home directory were purged
    purged_at: chrono::DateTime<chrono::Utc>,
}

#[allow(unused)]
impl PurgedUser {
    /// Get the GitHub user ID
    pub fn id(&self) -> octocrab::models::UserId {
        self.id
    }

    /// Get the login the user had when they departed
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the Linux user UID reserved for a rejoin
    pub fn uid(&self) -> unistd::Uid {
        self.uid
    }

    /// Get the GID of the primary group reserved for a rejoin, if it is tracked
    pub fn gid(&self) -> Option<unistd::Gid> {
        self.gid
    }

    /// Get the time the member departed
    pub fn departed_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.departed_at
    }

    /// Get the time the account was purged
    pub fn purged_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.purged_at
    }
}

pub type UserMap = collections::HashMap<octocrab::models::UserId, User>;
pub type DepartedMap = collections::HashMap<octocrab::models::UserId, DepartedUser>;
pub type PurgedMap = collections::HashMap<octocrab::models::UserId, PurgedUser>;

const USERS_FILE_NAME: &str = "users.json";
const USERS_V1_FILE_NAME: &str = "users-v1.json";
const STORE_VERSION: u64 = 2;

/// On-disk schema of the users database. A v1 file is a bare [`UserMap`] without the
/// `version` key and is migrated on load; [`UserStore::save`] always writes v2.
#[derive(Deserialize)]
struct StoreData {
    users: UserMap,
    #[serde(default)]
    departed: DepartedMap,
    #[serde(default)]
    purged: PurgedMap,
}

#[derive(Serialize)]
struct StoreDataRef<'a> {
    version: u64,
    users: &'a UserMap,
    departed: &'a DepartedMap,
    purged: &'a PurgedMap,
}

#[derive(Debug)]
pub struct UserStore {
    dir: path::PathBuf,
    /// Preview mode: [`UserStore::save`] never writes, so a dry run cannot persist
    /// invented mock IDs or tombstone changes into the real users database
    dry_run: bool,
    /// In-memory cache of users loaded from the members database, keyed by GitHub user ID
    users: UserMap,
    /// Tombstones of departed members whose account is expired on the machine, keyed by
    /// GitHub user ID. Kept separate from the active users so no consumer can process a
    /// tombstone as an active user.
    departed: DepartedMap,
    /// Tombstones of departed members whose expired account was purged, keyed by GitHub
    /// user ID. Kept separate from `departed` so the expiry reconciliation and the
    /// purge can never fight over an entry.
    purged: PurgedMap,
    /// Whether a v1 file was loaded whose backup is still outstanding. Loading must not
    /// touch the data directory, so the copy that keeps a rollback to a pre-v2 binary
    /// possible is deferred to the first save. The save that performs it clears the
    /// flag, so a later save cannot overwrite the backup with the v2 file.
    migrated_from_v1: sync::atomic::AtomicBool,
}

impl UserStore {
    /// Create an empty store without loading data.
    pub async fn new(dir: &path::Path, dry_run: bool) -> anyhow::Result<Self> {
        fs::create_dir_all(&dir).await?;
        Ok(Self {
            dir: dir.to_path_buf(),
            dry_run,
            users: UserMap::new(),
            departed: DepartedMap::new(),
            purged: PurgedMap::new(),
            migrated_from_v1: sync::atomic::AtomicBool::new(false),
        })
    }

    /// Load a store from a directory.
    #[tracing::instrument(name = "Store::from_dir")]
    pub async fn from_dir(dir: &path::Path, dry_run: bool) -> anyhow::Result<Self> {
        let mut s = Self::new(dir, dry_run).await?;
        s.load().await?;
        Ok(s)
    }

    pub fn data(&self) -> &UserMap {
        &self.users
    }

    pub fn data_mut(&mut self) -> &mut UserMap {
        &mut self.users
    }

    pub fn departed(&self) -> &DepartedMap {
        &self.departed
    }

    pub fn departed_mut(&mut self) -> &mut DepartedMap {
        &mut self.departed
    }

    pub fn purged(&self) -> &PurgedMap {
        &self.purged
    }

    pub fn purged_mut(&mut self) -> &mut PurgedMap {
        &mut self.purged
    }

    /// Turn a user into a departed tombstone, keeping their IDs for a later rejoin
    pub fn depart_user(&mut self, user: User, departed_at: chrono::DateTime<chrono::Utc>) {
        self.users.remove(&user.id);
        self.departed.insert(
            user.id,
            DepartedUser {
                id: user.id,
                name: user.name,
                uid: user.uid,
                gid: user.gid,
                departed_at,
                expired_at: None,
                purge_started_at: None,
            },
        );
    }

    /// Mark a departed user's account teardown as complete.
    pub fn mark_expired(
        &mut self,
        id: &octocrab::models::UserId,
        expired_at: chrono::DateTime<chrono::Utc>,
    ) {
        if let Some(departed) = self.departed.get_mut(id) {
            departed.expired_at = Some(expired_at);
        }
    }

    /// Record that account removal is about to start.
    pub fn mark_purge_started(
        &mut self,
        id: &octocrab::models::UserId,
        started_at: chrono::DateTime<chrono::Utc>,
    ) {
        if let Some(departed) = self.departed.get_mut(id) {
            departed.purge_started_at = Some(started_at);
        }
    }

    /// Clear the removal marker after confirming that no destructive operation ran.
    pub fn clear_purge_started(&mut self, id: &octocrab::models::UserId) {
        if let Some(departed) = self.departed.get_mut(id) {
            departed.purge_started_at = None;
        }
    }

    /// Move a departed tombstone to the purged map.
    pub fn mark_purged(
        &mut self,
        id: &octocrab::models::UserId,
        purged_at: chrono::DateTime<chrono::Utc>,
    ) {
        if let Some(departed) = self.departed.remove(id) {
            self.purged.insert(
                departed.id,
                PurgedUser {
                    id: departed.id,
                    name: departed.name,
                    uid: departed.uid,
                    gid: departed.gid,
                    departed_at: departed.departed_at,
                    purged_at,
                },
            );
        }
    }

    /// Drop tombstones for active users.
    pub fn prune_rejoined(&mut self) {
        self.departed.retain(|id, _| !self.users.contains_key(id));
        self.purged.retain(|id, _| !self.users.contains_key(id));
    }

    fn path(&self) -> path::PathBuf {
        self.dir.join(USERS_FILE_NAME)
    }

    /// Load the store, starting empty when no database exists.
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
            None => {
                let users: UserMap = serde_json::from_value(value).with_context(|| {
                    format!(
                        "Failed to parse v1 users database from '{}'",
                        path.display()
                    )
                })?;
                // Loading writes nothing; the save that rewrites the file as v2 takes
                // the backup that keeps a rollback possible.
                self.migrated_from_v1
                    .store(true, sync::atomic::Ordering::Relaxed);
                StoreData {
                    users,
                    departed: DepartedMap::new(),
                    purged: PurgedMap::new(),
                }
            }
            Some(version) if version.as_u64() == Some(STORE_VERSION) => {
                serde_json::from_value(value).with_context(|| {
                    format!("Failed to parse users database from '{}'", path.display())
                })?
            }
            Some(version) => anyhow::bail!(
                "Unsupported users database version {version} in '{}', this binary supports version {STORE_VERSION}",
                path.display()
            ),
        };

        self.users = data.users;
        self.departed = data.departed;
        self.purged = data.purged;
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

    /// Persist the store unless this is a dry run.
    pub async fn save(&self) -> anyhow::Result<()> {
        if self.dry_run {
            tracing::info!("Dry run: not writing the users database");
            return Ok(());
        }
        // The file on disk is still v1 until the write below replaces it, so this is the
        // last moment at which it can be preserved for a rollback.
        if self
            .migrated_from_v1
            .swap(false, sync::atomic::Ordering::Relaxed)
        {
            self.backup_v1_file().await?;
        }
        let content = serde_json::to_string_pretty(&StoreDataRef {
            version: STORE_VERSION,
            users: &self.users,
            departed: &self.departed,
            purged: &self.purged,
        })?;
        let path = self.path();
        // Avoid truncating the database if the write fails.
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
            gid: Some(unistd::Gid::from_raw(1000)),
        }
    }

    fn departed_at() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-08-14T09:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn departed_user() -> DepartedUser {
        DepartedUser {
            id: octocrab::models::UserId(456),
            name: "bob".to_string(),
            uid: unistd::Uid::from_raw(1005),
            gid: Some(unistd::Gid::from_raw(1005)),
            departed_at: departed_at(),
            expired_at: Some(departed_at()),
            purge_started_at: None,
        }
    }

    fn purged_user() -> PurgedUser {
        PurgedUser {
            id: octocrab::models::UserId(789),
            name: "carol".to_string(),
            uid: unistd::Uid::from_raw(1006),
            gid: Some(unistd::Gid::from_raw(1006)),
            departed_at: departed_at(),
            purged_at: chrono::DateTime::parse_from_rfc3339("2027-02-14T09:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        }
    }

    mod user {
        use super::*;

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
                "uid": 1000,
                "gid": 1000
            }"#;
            let expected_user = user();

            let user: User = serde_json::from_str(json).expect("Failed to deserialize user");
            assert_eq!(user, expected_user);
        }

        /// Entries migrated from a v1 file parse with no GID
        #[test]
        fn deserialization_without_gid() {
            let json = r#"{
                "id": 12345,
                "name": "testuser",
                "uid": 1000
            }"#;

            let user: User = serde_json::from_str(json).expect("Failed to deserialize user");
            assert_eq!(user.gid, None);
        }

        #[test]
        fn round_trip_serialization() {
            let original = User {
                id: octocrab::models::UserId(99999),
                name: "roundtripuser".to_string(),
                uid: unistd::Uid::from_raw(2000),
                gid: Some(unistd::Gid::from_raw(2000)),
            };

            let serialized = serde_json::to_string(&original).expect("Failed to serialize");
            let deserialized: User =
                serde_json::from_str(&serialized).expect("Failed to deserialize");

            assert_eq!(original.id, deserialized.id);
            assert_eq!(original.name, deserialized.name);
            assert_eq!(original.uid.as_raw(), deserialized.uid.as_raw());
            assert_eq!(original.gid, deserialized.gid);
        }

        /// An untracked GID is omitted from the file instead of written as null
        #[test]
        fn missing_gid_is_omitted() {
            let mut user = user();
            user.gid = None;

            let serialized = serde_json::to_string(&user).unwrap();
            assert!(!serialized.contains("gid"));
        }
    }

    mod departed_user {
        use super::*;

        #[test]
        fn round_trip_serialization() {
            let original = departed_user();

            let serialized = serde_json::to_string(&original).expect("Failed to serialize");
            let deserialized: DepartedUser =
                serde_json::from_str(&serialized).expect("Failed to deserialize");

            assert_eq!(original, deserialized);
        }

        #[test]
        fn departed_at_is_serialized_as_rfc3339() {
            let serialized = serde_json::to_string(&departed_user()).unwrap();
            assert!(serialized.contains("\"departed_at\":\"2026-08-14T09:00:00Z\""));
        }

        #[test]
        fn account_identity_conversion() {
            let departed = departed_user();
            let user = User::from(&departed);
            assert_eq!(user.id, departed.id);
            assert_eq!(user.name, departed.name);
            assert_eq!(user.uid, departed.uid);
            assert_eq!(user.gid, departed.gid);
        }
    }

    mod purged_user {
        use super::*;

        #[test]
        fn round_trip_serialization() {
            let original = purged_user();

            let serialized = serde_json::to_string(&original).expect("Failed to serialize");
            let deserialized: PurgedUser =
                serde_json::from_str(&serialized).expect("Failed to deserialize");

            assert_eq!(original, deserialized);
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

            let store = UserStore::from_dir(&store_path, false)
                .await
                .expect("Failed to create store");

            assert!(store_path.exists());
            assert!(store.users.is_empty());
            assert!(store.departed.is_empty());
            assert!(store.purged.is_empty());
        }

        #[tokio::test]
        async fn load_nonexistent_file() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            let store = UserStore::from_dir(temp_dir.path(), false)
                .await
                .expect("Failed to create store");

            assert!(store.users.is_empty());
            assert!(store.departed.is_empty());
            // No file, nothing to back up
            assert!(!temp_dir.path().join(USERS_V1_FILE_NAME).exists());
        }

        /// Loading a v1 file migrates it in memory only: a read, and a `--dry-run` in
        /// particular, must not write anything into the data directory.
        #[tokio::test]
        async fn v1_file_is_migrated_in_memory_without_writing() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), &v1_content()).await;

            let store = UserStore::from_dir(temp_dir.path(), false)
                .await
                .expect("Failed to load store");

            assert_eq!(store.users.len(), 1);
            assert_eq!(
                store.users[&octocrab::models::UserId(12345)].name,
                "testuser"
            );
            assert!(store.departed.is_empty());
            assert!(!temp_dir.path().join(USERS_V1_FILE_NAME).exists());
            // The file itself is untouched until a save rewrites it
            let on_disk = fs::read_to_string(temp_dir.path().join(USERS_FILE_NAME))
                .await
                .unwrap();
            assert_eq!(on_disk, v1_content());
        }

        /// A dry run over a v1 file leaves the data directory exactly as it found it.
        #[tokio::test]
        async fn dry_run_over_a_v1_file_creates_no_files() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), &v1_content()).await;

            let store = UserStore::from_dir(temp_dir.path(), true).await.unwrap();
            store.save().await.expect("Dry-run save must succeed");

            assert!(!temp_dir.path().join(USERS_V1_FILE_NAME).exists());
            let on_disk = fs::read_to_string(temp_dir.path().join(USERS_FILE_NAME))
                .await
                .unwrap();
            assert_eq!(on_disk, v1_content());
        }

        #[tokio::test]
        async fn save_after_v1_migration_writes_v2_and_backs_up_v1() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), &v1_content()).await;

            let store = UserStore::from_dir(temp_dir.path(), false).await.unwrap();
            store.save().await.expect("Failed to save store");

            let content = fs::read_to_string(temp_dir.path().join(USERS_FILE_NAME))
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert_eq!(value["version"], 2);
            assert!(value["users"]["12345"].is_object());
            assert_eq!(value["departed"], serde_json::json!({}));
            assert_eq!(value["purged"], serde_json::json!({}));

            // The rollback copy preserves the v1 file byte for byte
            let backup = fs::read_to_string(temp_dir.path().join(USERS_V1_FILE_NAME))
                .await
                .expect("Backup file was not created");
            assert_eq!(backup, v1_content());
        }

        /// The backup is taken by the first save only: a second save must not copy the
        /// v2 file over the rollback copy of the v1 one.
        #[tokio::test]
        async fn second_save_does_not_overwrite_the_v1_backup() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), &v1_content()).await;

            let store = UserStore::from_dir(temp_dir.path(), false).await.unwrap();
            store.save().await.expect("Failed to save store");
            store.save().await.expect("Failed to save store again");

            let backup = fs::read_to_string(temp_dir.path().join(USERS_V1_FILE_NAME))
                .await
                .unwrap();
            assert_eq!(backup, v1_content());
        }

        #[tokio::test]
        async fn unknown_newer_version_is_an_error() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), r#"{ "version": 3, "users": {} }"#).await;

            let err = UserStore::from_dir(temp_dir.path(), false)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("version 3"));
        }

        #[tokio::test]
        async fn round_trip_with_departed_and_purged_users() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            let mut store = UserStore::from_dir(temp_dir.path(), false)
                .await
                .expect("Failed to create store");
            let active = user();
            let departed = departed_user();
            let purged = purged_user();
            store.users.insert(active.id, active.clone());
            store.departed.insert(departed.id, departed.clone());
            store.purged.insert(purged.id, purged.clone());

            store.save().await.expect("Failed to save store");

            let loaded = UserStore::from_dir(temp_dir.path(), false)
                .await
                .expect("Failed to load store");
            assert_eq!(loaded.users[&active.id], active);
            assert_eq!(loaded.departed[&departed.id], departed);
            assert_eq!(loaded.purged[&purged.id], purged);
            // The file was already v2, no v1 backup is created
            assert!(!temp_dir.path().join(USERS_V1_FILE_NAME).exists());
        }

        #[tokio::test]
        async fn v2_file_without_tombstone_keys_parses() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), r#"{ "version": 2, "users": {} }"#).await;

            let store = UserStore::from_dir(temp_dir.path(), false).await.unwrap();
            assert!(store.users.is_empty());
            assert!(store.departed.is_empty());
            assert!(store.purged.is_empty());
        }

        #[tokio::test]
        async fn depart_user_moves_an_active_user_to_the_tombstones() {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let mut store = UserStore::new(temp_dir.path(), false).await.unwrap();
            let user = user();
            store.users.insert(user.id, user.clone());

            store.depart_user(user.clone(), departed_at());

            assert!(store.users.is_empty());
            let departed = &store.departed[&user.id];
            assert_eq!(departed.name, user.name);
            assert_eq!(departed.uid, user.uid);
            assert_eq!(departed.gid, user.gid);
            assert_eq!(departed.departed_at, departed_at());
        }

        #[tokio::test]
        async fn mark_purged_moves_a_departed_tombstone_to_the_purged_map() {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let mut store = UserStore::new(temp_dir.path(), false).await.unwrap();
            let departed = departed_user();
            store.departed.insert(departed.id, departed.clone());
            let purged_at = chrono::Utc::now();

            store.mark_purged(&departed.id, purged_at);

            assert!(store.departed.is_empty());
            let purged = &store.purged[&departed.id];
            assert_eq!(purged.name, departed.name);
            assert_eq!(purged.uid, departed.uid);
            assert_eq!(purged.gid, departed.gid);
            assert_eq!(purged.departed_at, departed.departed_at);
            assert_eq!(purged.purged_at, purged_at);
        }

        #[tokio::test]
        async fn prune_rejoined_drops_tombstones_of_active_users() {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let mut store = UserStore::new(temp_dir.path(), false).await.unwrap();
            let rejoined = user();
            let departed = departed_user();
            store
                .departed
                .insert(rejoined.id, DepartedUser::from_test_user(&rejoined));
            store.departed.insert(departed.id, departed.clone());
            store.users.insert(rejoined.id, rejoined.clone());

            store.prune_rejoined();

            assert!(!store.departed.contains_key(&rejoined.id));
            assert_eq!(store.departed[&departed.id], departed);
        }

        /// A member who rejoins after their account was purged spends the purged
        /// tombstone, exactly like a departed one
        #[tokio::test]
        async fn prune_rejoined_drops_purged_tombstones_of_active_users() {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let mut store = UserStore::new(temp_dir.path(), false).await.unwrap();
            let purged = purged_user();
            let rejoined = User::from_test_purged(&purged);
            store.purged.insert(purged.id, purged.clone());
            store.users.insert(rejoined.id, rejoined);

            store.prune_rejoined();

            assert!(store.purged.is_empty());
        }

        /// A store in dry-run mode previews everything in memory but never persists
        #[tokio::test]
        async fn dry_run_save_writes_nothing() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            let mut store = UserStore::new(temp_dir.path(), true).await.unwrap();
            let user = user();
            store.users.insert(user.id, user);

            store.save().await.expect("Dry-run save must succeed");

            assert!(!temp_dir.path().join(USERS_FILE_NAME).exists());
        }

        #[tokio::test]
        async fn load_invalid_json() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            write_users_file(temp_dir.path(), "{ invalid json content").await;

            let result = UserStore::from_dir(temp_dir.path(), false).await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn user_path() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            let store = UserStore::new(temp_dir.path(), false)
                .await
                .expect("Failed to create store");

            let expected_path = temp_dir.path().join(USERS_FILE_NAME);
            assert_eq!(store.path(), expected_path);
        }

        #[tokio::test]
        async fn multiple_users() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            let mut store = UserStore::new(temp_dir.path(), false)
                .await
                .expect("Failed to create store");

            for i in 0..10 {
                let user = User {
                    id: octocrab::models::UserId(1000 + i),
                    name: format!("user{}", i),
                    uid: unistd::Uid::from_raw(3000 + i as u32),
                    gid: Some(unistd::Gid::from_raw(3000 + i as u32)),
                };
                store.users.insert(user.id, user);
            }

            store.save().await.expect("Failed to save store");

            let loaded_store = UserStore::from_dir(temp_dir.path(), false)
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

    impl DepartedUser {
        /// Tombstone with the identity of `user`, for tests
        fn from_test_user(user: &User) -> Self {
            Self {
                id: user.id,
                name: user.name.clone(),
                uid: user.uid,
                gid: user.gid,
                departed_at: departed_at(),
                expired_at: None,
                purge_started_at: None,
            }
        }
    }

    impl User {
        /// Active user with the identity of a purged tombstone, for tests
        fn from_test_purged(purged: &PurgedUser) -> Self {
            Self {
                id: purged.id,
                name: purged.name.clone(),
                uid: purged.uid,
                gid: purged.gid,
            }
        }
    }
}
