use crate::store;
use anyhow::Context as _;
use std::path;
use tokio::fs;

const ARCHIVE_DIR_PERMISSIONS: u32 = 0o700;
const ARCHIVE_FILE_PERMISSIONS: u32 = 0o600;
/// Number of archives kept per user, older archives are pruned after a successful archive
const MAX_ARCHIVES_PER_USER: usize = 3;

/// Proof that a user's home directory was archived, or that there was nothing to archive.
/// It can only be obtained from [`archive_home_dir`], so destructive operations that take a
/// receipt can not be performed without archiving first.
#[must_use]
pub struct ArchiveReceipt {
    /// Path of the created archive, `None` if the home directory did not exist
    archive_path: Option<path::PathBuf>,
}

impl ArchiveReceipt {
    /// Path of the created archive, `None` if the home directory did not exist
    pub fn archive_path(&self) -> Option<&path::Path> {
        self.archive_path.as_deref()
    }
}

/// Archive a user's home directory as a gzipped tarball in `archive_dir` before it is
/// deleted. The archive directory and the tarball have permissions 700 and 600 respectively
/// and are owned by root when running as root, so only root can access archived data.
///
/// Entries that cannot be archived (sockets and other special files) or that disappear
/// while archiving are skipped, a failed archive blocks the user deletion forever.
/// Only the [`MAX_ARCHIVES_PER_USER`] most recent archives are kept per user.
///
/// Returns an [`ArchiveReceipt`] carrying the path of the created archive, or no path if
/// the home directory does not exist.
#[tracing::instrument(name = "archive_home_dir", skip(archive_dir, user), fields(user = %user.name()))]
pub async fn archive_home_dir(
    archive_dir: &path::Path,
    user: &store::User,
    home_dir: &path::Path,
) -> anyhow::Result<ArchiveReceipt> {
    use std::os::unix::fs::PermissionsExt as _;

    // Use symlink_metadata so a home directory that is itself a symlink is refused instead
    // of silently archiving (and later deleting) whatever tree it points to
    let metadata = match fs::symlink_metadata(home_dir).await {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "Home directory '{}' does not exist, nothing to archive",
                home_dir.display()
            );
            return Ok(ArchiveReceipt { archive_path: None });
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!("Failed to read home directory '{}'", home_dir.display())
            });
        }
    };
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "Refusing to archive home directory '{}': it is a symlink",
            home_dir.display()
        );
    }
    if !metadata.is_dir() {
        anyhow::bail!(
            "Refusing to archive home directory '{}': not a directory",
            home_dir.display()
        );
    }
    if archive_dir.starts_with(home_dir) {
        anyhow::bail!(
            "Refusing to archive home directory '{}': it contains the archive directory '{}'",
            home_dir.display(),
            archive_dir.display()
        );
    }

    let home_name = home_dir.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "Refusing to archive home directory '{}'",
            home_dir.display()
        )
    })?;

    let mut dir_builder = fs::DirBuilder::new();
    dir_builder
        .recursive(true)
        .mode(ARCHIVE_DIR_PERMISSIONS)
        .create(archive_dir)
        .await
        .with_context(|| {
            format!(
                "Failed to create archive directory '{}'",
                archive_dir.display()
            )
        })?;
    // The directory may have existed before with different permissions or ownership,
    // enforce that only root can access it
    fs::set_permissions(
        archive_dir,
        std::fs::Permissions::from_mode(ARCHIVE_DIR_PERMISSIONS),
    )
    .await
    .with_context(|| {
        format!(
            "Failed to set permissions on archive directory '{}'",
            archive_dir.display()
        )
    })?;
    chown_to_root(archive_dir)
        .context("Failed to change ownership of archive directory to root")?;

    let archive_path = archive_dir.join(format!(
        "{}-{}-{}.tar.gz",
        user.name(),
        user.uid(),
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    // Create the archive with permissions 600 from the start, so its content is never
    // readable by other users. create_new fails if the archive already exists, so an
    // existing archive is never overwritten, and the deletion is retried on the next sync
    // with a new timestamp.
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(ARCHIVE_FILE_PERMISSIONS)
        .open(&archive_path)
        .await
        .with_context(|| format!("Failed to create archive file '{}'", archive_path.display()))?;

    let result = async {
        write_archive(file, home_name.as_ref(), home_dir).await?;
        chown_to_root(&archive_path).context("Failed to change ownership of archive to root")?;
        // Sync the directory entry as well, the archive must survive a crash once
        // userdel removes the home directory
        fs::File::open(archive_dir)
            .await
            .context("Failed to open archive directory for syncing")?
            .sync_all()
            .await
            .context("Failed to sync archive directory")?;
        Ok::<_, anyhow::Error>(())
    }
    .await;

    if let Err(e) = result {
        // Don't leave a partial or unsecured archive behind. The file was created by this
        // call (create_new above), so it is safe to remove here.
        let _ = fs::remove_file(&archive_path).await;
        return Err(e)
            .with_context(|| format!("Failed to archive home directory '{}'", home_dir.display()));
    }

    prune_old_archives(archive_dir, user).await;

    Ok(ArchiveReceipt {
        archive_path: Some(archive_path),
    })
}

/// Write the gzipped tarball of `home_dir` to `file` and sync it to disk. Flushing alone
/// only reaches the page cache, so the file is fsynced before the caller deletes the
/// archived data.
async fn write_archive(
    file: fs::File,
    home_name: &path::Path,
    home_dir: &path::Path,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt as _;

    // Buffer writes so the encoder output doesn't hit the blocking pool for every small
    // chunk, and use the fastest compression level, home directories are dominated by
    // already-compressed content
    let writer = tokio::io::BufWriter::with_capacity(1 << 20, file);
    let encoder = async_compression::tokio::write::GzipEncoder::with_quality(
        writer,
        async_compression::Level::Fastest,
    );
    let mut builder = tokio_tar::Builder::new(encoder);
    // Store symlinks as symlinks instead of following them out of the home directory
    builder.follow_symlinks(false);
    append_dir_recursive(&mut builder, home_name, home_dir).await?;
    let mut encoder = builder
        .into_inner()
        .await
        .context("Failed to finish tar archive")?;
    encoder
        .shutdown()
        .await
        .context("Failed to flush archive")?;
    let file = encoder.into_inner().into_inner();
    file.sync_all()
        .await
        .context("Failed to sync archive to disk")?;
    Ok(())
}

/// Append `src_root` recursively, storing entries under `dest_root`. Unlike
/// `Builder::append_dir_all`, entries that cannot be archived (sockets and other special
/// files) or that disappear while archiving are skipped with a warning instead of failing
/// the archive, because a failed archive blocks the user deletion until it succeeds.
async fn append_dir_recursive<W>(
    builder: &mut tokio_tar::Builder<W>,
    dest_root: &path::Path,
    src_root: &path::Path,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    builder
        .append_path_with_name(src_root, dest_root)
        .await
        .with_context(|| format!("Failed to archive '{}'", src_root.display()))?;

    let mut stack = vec![src_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = match fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!("Skipping '{}': removed while archiving", dir.display());
                continue;
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to read directory '{}'", dir.display()));
            }
        };

        while let Some(entry) = entries
            .next_entry()
            .await
            .with_context(|| format!("Failed to read directory '{}'", dir.display()))?
        {
            let entry_path = entry.path();
            let dest = dest_root.join(
                entry_path
                    .strip_prefix(src_root)
                    .expect("directory entry is below the source root"),
            );
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!(
                        "Skipping '{}': removed while archiving",
                        entry_path.display()
                    );
                    continue;
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("Failed to read file type of '{}'", entry_path.display())
                    });
                }
            };
            if !(file_type.is_file() || file_type.is_dir() || file_type.is_symlink()) {
                tracing::warn!(
                    "Skipping '{}': sockets and other special files can not be archived",
                    entry_path.display()
                );
                continue;
            }
            match builder.append_path_with_name(&entry_path, &dest).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!(
                        "Skipping '{}': removed while archiving",
                        entry_path.display()
                    );
                    continue;
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("Failed to archive '{}'", entry_path.display()));
                }
            }
            if file_type.is_dir() {
                stack.push(entry_path);
            }
        }
    }
    Ok(())
}

/// Remove the oldest archives of a user, keeping the [`MAX_ARCHIVES_PER_USER`] most recent
/// ones. Pruning is best-effort: failures are logged and never fail the archiving.
async fn prune_old_archives(archive_dir: &path::Path, user: &store::User) {
    let prefix = format!("{}-{}-", user.name(), user.uid());

    let mut entries = match fs::read_dir(archive_dir).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(
                "Failed to read archive directory '{}' for pruning: {e}",
                archive_dir.display()
            );
            return;
        }
    };

    let mut archives = Vec::new();
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                let file_name = entry.file_name();
                let Some(file_name) = file_name.to_str() else {
                    continue;
                };
                let Some(suffix) = file_name
                    .strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_suffix(".tar.gz"))
                else {
                    continue;
                };
                if !is_archive_timestamp(suffix) {
                    continue;
                }
                archives.push(file_name.to_owned());
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(
                    "Failed to read archive directory '{}' for pruning: {e}",
                    archive_dir.display()
                );
                return;
            }
        }
    }

    if archives.len() <= MAX_ARCHIVES_PER_USER {
        return;
    }
    // Timestamped file names sort chronologically
    archives.sort();
    for file_name in &archives[..archives.len() - MAX_ARCHIVES_PER_USER] {
        let archive_path = archive_dir.join(file_name);
        match fs::remove_file(&archive_path).await {
            Ok(()) => tracing::info!("Pruned old archive '{}'", archive_path.display()),
            Err(e) => tracing::warn!(
                "Failed to prune old archive '{}': {e}",
                archive_path.display()
            ),
        }
    }
}

/// Check whether the suffix of an archive file name is a timestamp formatted as
/// `%Y%m%dT%H%M%SZ`, so archives of a user whose name merely starts with another user's
/// name and uid are never pruned by that user.
fn is_archive_timestamp(suffix: &str) -> bool {
    suffix.len() == 16
        && suffix.as_bytes()[8] == b'T'
        && suffix.as_bytes()[15] == b'Z'
        && suffix[..8].bytes().all(|b| b.is_ascii_digit())
        && suffix[9..15].bytes().all(|b| b.is_ascii_digit())
}

/// Change ownership of a path to root:root when running as root. When not running as root
/// (e.g. in tests), the path stays owned by the current user and remains inaccessible to
/// others through its permissions.
fn chown_to_root(path: &path::Path) -> anyhow::Result<()> {
    if !nix::unistd::geteuid().is_root() {
        tracing::debug!(
            "Not running as root, '{}' stays owned by the current user",
            path.display()
        );
        return Ok(());
    }
    nix::unistd::chown(
        path,
        Some(nix::unistd::Uid::from_raw(0)),
        Some(nix::unistd::Gid::from_raw(0)),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use std::collections;
    use std::os::unix::fs::PermissionsExt as _;

    fn user() -> store::User {
        store::User::builder()
            .id(octocrab::models::UserId(12345))
            .name("testuser".to_string())
            .uid(nix::unistd::Uid::from_raw(1000))
            .build()
    }

    async fn create_home(root: &path::Path) -> path::PathBuf {
        let home = root.join("testuser");
        fs::create_dir(&home).await.expect("Failed to create home");
        fs::write(home.join("file.txt"), b"hello")
            .await
            .expect("Failed to write file");
        fs::create_dir(home.join(".ssh"))
            .await
            .expect("Failed to create .ssh");
        fs::write(home.join(".ssh").join("authorized_keys"), b"key")
            .await
            .expect("Failed to write authorized_keys");
        fs::symlink("/etc/passwd", home.join("link"))
            .await
            .expect("Failed to create symlink");
        home
    }

    async fn read_entries(
        archive_path: &path::Path,
    ) -> collections::HashMap<path::PathBuf, (tokio_tar::EntryType, Vec<u8>)> {
        use tokio::io::AsyncReadExt as _;

        let file = fs::File::open(archive_path)
            .await
            .expect("Failed to open archive");
        let decoder =
            async_compression::tokio::bufread::GzipDecoder::new(tokio::io::BufReader::new(file));
        let mut archive = tokio_tar::Archive::new(decoder);
        let mut entries = archive.entries().expect("Failed to read entries");

        let mut result = collections::HashMap::new();
        while let Some(entry) = entries.next().await {
            let mut entry = entry.expect("Failed to read entry");
            let path = entry.path().expect("Invalid entry path").to_path_buf();
            let entry_type = entry.header().entry_type();
            let mut content = Vec::new();
            entry
                .read_to_end(&mut content)
                .await
                .expect("Failed to read entry content");
            result.insert(path, (entry_type, content));
        }
        result
    }

    async fn archive_file_names(archive_dir: &path::Path) -> Vec<String> {
        let mut names = Vec::new();
        let mut entries = fs::read_dir(archive_dir)
            .await
            .expect("Failed to read archive dir");
        while let Some(entry) = entries.next_entry().await.expect("Failed to read entry") {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        names
    }

    #[tokio::test]
    async fn archives_home_directory_with_root_only_permissions() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let home = create_home(temp_dir.path()).await;
        let archive_dir = temp_dir.path().join("archive");

        let receipt = archive_home_dir(&archive_dir, &user(), &home)
            .await
            .expect("Archiving failed");
        let archive_path = receipt
            .archive_path()
            .expect("Expected an archive to be created");

        let dir_mode = fs::metadata(&archive_dir)
            .await
            .expect("Failed to read archive dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, ARCHIVE_DIR_PERMISSIONS);

        let file_mode = fs::metadata(&archive_path)
            .await
            .expect("Failed to read archive metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, ARCHIVE_FILE_PERMISSIONS);

        let entries = read_entries(archive_path).await;
        let (entry_type, content) = &entries[path::Path::new("testuser/file.txt")];
        assert_eq!(*entry_type, tokio_tar::EntryType::Regular);
        assert_eq!(content, b"hello");

        let (entry_type, content) = &entries[path::Path::new("testuser/.ssh/authorized_keys")];
        assert_eq!(*entry_type, tokio_tar::EntryType::Regular);
        assert_eq!(content, b"key");

        // Symlinks are stored as symlinks, their target content is not archived
        let (entry_type, content) = &entries[path::Path::new("testuser/link")];
        assert_eq!(*entry_type, tokio_tar::EntryType::Symlink);
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn missing_home_dir_returns_none() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let archive_dir = temp_dir.path().join("archive");

        let receipt = archive_home_dir(&archive_dir, &user(), &temp_dir.path().join("nonexistent"))
            .await
            .expect("Archiving failed");

        assert!(receipt.archive_path().is_none());
        // No archive directory is created when there is nothing to archive
        assert!(!archive_dir.exists());
    }

    #[tokio::test]
    async fn symlinked_home_dir_is_refused() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let home = create_home(temp_dir.path()).await;
        let link = temp_dir.path().join("home_link");
        fs::symlink(&home, &link)
            .await
            .expect("Failed to create symlink");
        let archive_dir = temp_dir.path().join("archive");

        let result = archive_home_dir(&archive_dir, &user(), &link).await;

        assert!(result.is_err());
        assert!(!archive_dir.exists());
    }

    #[tokio::test]
    async fn home_dir_containing_archive_dir_is_refused() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let home = create_home(temp_dir.path()).await;
        let archive_dir = home.join("archive");

        let result = archive_home_dir(&archive_dir, &user(), &home).await;

        assert!(result.is_err());
        assert!(!archive_dir.exists());
    }

    #[tokio::test]
    async fn sockets_are_skipped() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let home = create_home(temp_dir.path()).await;
        let _listener = std::os::unix::net::UnixListener::bind(home.join("agent.sock"))
            .expect("Failed to bind unix socket");
        let archive_dir = temp_dir.path().join("archive");

        let receipt = archive_home_dir(&archive_dir, &user(), &home)
            .await
            .expect("Archiving failed");
        let archive_path = receipt
            .archive_path()
            .expect("Expected an archive to be created");

        let entries = read_entries(archive_path).await;
        assert!(!entries.contains_key(path::Path::new("testuser/agent.sock")));
        assert!(entries.contains_key(path::Path::new("testuser/file.txt")));
    }

    #[tokio::test]
    async fn failed_archive_is_removed() {
        if nix::unistd::geteuid().is_root() {
            // Root can read the unreadable file, the failure can not be provoked
            return;
        }
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let home = create_home(temp_dir.path()).await;
        let unreadable = home.join("unreadable.txt");
        fs::write(&unreadable, b"secret")
            .await
            .expect("Failed to write file");
        fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))
            .await
            .expect("Failed to set permissions");
        let archive_dir = temp_dir.path().join("archive");

        let result = archive_home_dir(&archive_dir, &user(), &home).await;

        assert!(result.is_err());
        // The partial archive is not left behind
        assert!(archive_file_names(&archive_dir).await.is_empty());
    }

    #[tokio::test]
    async fn old_archives_are_pruned() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let home = create_home(temp_dir.path()).await;
        let archive_dir = temp_dir.path().join("archive");
        fs::create_dir(&archive_dir)
            .await
            .expect("Failed to create archive dir");
        for name in [
            "testuser-1000-20200101T000000Z.tar.gz",
            "testuser-1000-20200102T000000Z.tar.gz",
            "testuser-1000-20200103T000000Z.tar.gz",
            // Not a timestamp suffix, never pruned
            "testuser-1000-keepme.tar.gz",
            // Different user whose name and uid merely extend the prefix, never pruned
            "testuser-1000-2-20200101T000000Z.tar.gz",
        ] {
            fs::write(archive_dir.join(name), b"")
                .await
                .expect("Failed to create old archive");
        }

        let receipt = archive_home_dir(&archive_dir, &user(), &home)
            .await
            .expect("Archiving failed");
        let archive_path = receipt
            .archive_path()
            .expect("Expected an archive to be created");

        let names = archive_file_names(&archive_dir).await;
        // The oldest archive is pruned, keeping the most recent MAX_ARCHIVES_PER_USER plus
        // the two names that don't belong to this user's archives
        assert!(!names.contains(&"testuser-1000-20200101T000000Z.tar.gz".to_string()));
        assert!(names.contains(&"testuser-1000-20200102T000000Z.tar.gz".to_string()));
        assert!(names.contains(&"testuser-1000-20200103T000000Z.tar.gz".to_string()));
        assert!(names.contains(&"testuser-1000-keepme.tar.gz".to_string()));
        assert!(names.contains(&"testuser-1000-2-20200101T000000Z.tar.gz".to_string()));
        assert!(
            names.contains(
                &archive_path
                    .file_name()
                    .expect("Archive has a file name")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        assert_eq!(names.len(), 5);
    }
}
