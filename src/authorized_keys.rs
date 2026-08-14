//! Maintains an octosync-managed block in each user's authorized_keys file while
//! preserving all manual entries. Files are opened without following symlinks and
//! replaced atomically because these operations run as root in user-owned directories.

use crate::{public_keys, store};
use anyhow::Context as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path;
use tokio::{fs, io};

const SSH_DIR_MODE: u32 = 0o700;
const AUTHORIZED_KEYS_MODE: u32 = 0o600;
const AUTHORIZED_KEYS_FILE_NAME: &str = "authorized_keys";

const MANAGED_BLOCK_START: &str = "# >>> octosync managed keys - do not edit this block >>>";
const MANAGED_BLOCK_END: &str = "# <<< octosync managed keys - do not edit this block <<<";

#[tracing::instrument(
    name = "authorized_keys::update_authorized_keys",
    skip_all,
    fields(user = %user.name())
)]
pub async fn update_authorized_keys(
    user: &store::User,
    keys: &public_keys::PublicKeys,
) -> anyhow::Result<()> {
    let system_user = nix::unistd::User::from_uid(user.uid())
        .context("Failed to look up user before updating authorized_keys")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "User '{}' not found in system when updating authorized_keys",
                user.name()
            )
        })?;

    sync_authorized_keys(
        &system_user.dir.join(".ssh"),
        keys,
        system_user.uid,
        system_user.gid,
    )
    .await
}

/// Replace the managed block while preserving content outside it.
async fn sync_authorized_keys(
    ssh_dir: &path::Path,
    keys: &public_keys::PublicKeys,
    uid: nix::unistd::Uid,
    gid: nix::unistd::Gid,
) -> anyhow::Result<()> {
    ensure_ssh_dir(ssh_dir, uid, gid).await?;

    let authorized_keys_path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
    let mut existing_file = match open_no_follow(&authorized_keys_path, false).await {
        Ok(file) => Some(file),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "Failed to open '{}' (is it a symlink?)",
                    authorized_keys_path.display()
                )
            });
        }
    };

    let current = match existing_file.as_mut() {
        Some(file) => read_content(file, &authorized_keys_path).await?,
        None => String::new(),
    };
    let (before, after) = split_around_managed_block(&current);
    let updated = render_content(&before, &after, keys);

    if let Some(file) = existing_file
        && updated == current
    {
        tracing::debug!(
            "authorized_keys '{}' is up to date",
            authorized_keys_path.display()
        );
        return ensure_owner_and_mode(&file, &authorized_keys_path, uid, gid, AUTHORIZED_KEYS_MODE)
            .await;
    }

    write_atomic(&authorized_keys_path, updated, uid, gid)
        .await
        .with_context(|| {
            format!(
                "Failed to write authorized_keys file '{}'",
                authorized_keys_path.display()
            )
        })?;

    tracing::info!(
        "Updated managed block in '{}' to {} public keys",
        authorized_keys_path.display(),
        keys.len(),
    );
    Ok(())
}

/// Split `content` into the lines before the first octosync-managed block and the lines
/// after it, removing every managed block. The split point marks where the block is
/// re-rendered, so manual lines keep their position relative to it.
///
/// A start marker without a matching end marker means the block was tampered with or an
/// editor stripped it; everything after the start marker is treated as managed so stale
/// fetched keys can not survive outside an intact block.
fn split_around_managed_block(content: &str) -> (Vec<&str>, Vec<&str>) {
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut in_block = false;
    let mut seen_block = false;
    for line in content.lines() {
        match line.trim() {
            MANAGED_BLOCK_START => {
                in_block = true;
                seen_block = true;
            }
            MANAGED_BLOCK_END => in_block = false,
            _ if in_block => {}
            _ if seen_block => after.push(line),
            _ => before.push(line),
        }
    }
    if in_block {
        tracing::warn!(
            "authorized_keys contains an unterminated octosync-managed block, treating everything after the start marker as managed"
        );
    }
    (before, after)
}

/// Render the full authorized_keys content: the lines around the managed block unchanged and
/// in place, with the block holding exactly the fetched keys where the first block was found
/// (or at the end of the file if there was none). The block is always present, so an empty
/// fetch result renders an empty block and thereby revokes previously fetched keys.
fn render_content(before: &[&str], after: &[&str], keys: &public_keys::PublicKeys) -> String {
    let mut content = String::new();
    for line in before {
        content.push_str(line);
        content.push('\n');
    }
    content.push_str(MANAGED_BLOCK_START);
    content.push('\n');
    let keys_str = keys.to_string();
    if !keys_str.is_empty() {
        content.push_str(&keys_str);
        content.push('\n');
    }
    content.push_str(MANAGED_BLOCK_END);
    content.push('\n');
    for line in after {
        content.push_str(line);
        content.push('\n');
    }
    content
}

/// Open a path read-only without following symlinks
async fn open_no_follow(path: &path::Path, dir: bool) -> io::Result<fs::File> {
    let mut flags = nix::fcntl::OFlag::O_NOFOLLOW;
    if dir {
        flags |= nix::fcntl::OFlag::O_DIRECTORY;
    }
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits())
        .open(path)
        .await
}

/// Read the content of an already opened authorized_keys file
async fn read_content(file: &mut fs::File, path: &path::Path) -> anyhow::Result<String> {
    use tokio::io::AsyncReadExt as _;

    let mut content = String::new();
    file.read_to_string(&mut content)
        .await
        .with_context(|| format!("Failed to read '{}'", path.display()))?;
    Ok(content)
}

/// Atomically replace `dest_path` with `content`, owned by `uid:gid` with the
/// authorized_keys mode. Permissions and ownership are set through the file descriptor
/// before committing, so the file never appears at its final path with the wrong ones.
async fn write_atomic(
    dest_path: &path::Path,
    content: String,
    uid: nix::unistd::Uid,
    gid: nix::unistd::Gid,
) -> anyhow::Result<()> {
    let dest_path = dest_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use atomic_write_file::unix::OpenOptionsExt as _;
        use std::io::Write as _;

        let mut options = atomic_write_file::OpenOptions::new();
        options.preserve_mode(false).try_preserve_owner(false);
        let mut file = options.open(&dest_path)?;
        file.write_all(content.as_bytes())?;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(AUTHORIZED_KEYS_MODE))?;
        nix::unistd::fchown(file.as_file(), Some(uid), Some(gid)).map_err(std::io::Error::from)?;
        file.commit()
    })
    .await
    .context("Atomic write task failed")??;
    Ok(())
}

/// Create the .ssh directory if it doesn't exist and enforce its permissions and ownership
async fn ensure_ssh_dir(
    ssh_dir: &path::Path,
    uid: nix::unistd::Uid,
    gid: nix::unistd::Gid,
) -> anyhow::Result<()> {
    match fs::create_dir(ssh_dir).await {
        Ok(()) => tracing::info!("Created .ssh directory '{}'", ssh_dir.display()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!("Failed to create .ssh directory '{}'", ssh_dir.display())
            });
        }
    }

    let dir = open_no_follow(ssh_dir, true).await.with_context(|| {
        format!(
            "Failed to open .ssh directory '{}' (is it a symlink?)",
            ssh_dir.display()
        )
    })?;
    ensure_owner_and_mode(&dir, ssh_dir, uid, gid, SSH_DIR_MODE).await
}

/// Enforce exact permissions and ownership on an opened file or directory, fixing them if
/// they differ. Operates on the file descriptor, so the path cannot be swapped underneath.
async fn ensure_owner_and_mode(
    file: &fs::File,
    path: &path::Path,
    uid: nix::unistd::Uid,
    gid: nix::unistd::Gid,
    mode: u32,
) -> anyhow::Result<()> {
    let metadata = file
        .metadata()
        .await
        .with_context(|| format!("Failed to read metadata of '{}'", path.display()))?;

    if metadata.permissions().mode() & 0o7777 != mode {
        tracing::info!("Setting permissions on '{}' to {:o}", path.display(), mode);
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .await
            .with_context(|| format!("Failed to set permissions on '{}'", path.display()))?;
    }

    if metadata.uid() != uid.as_raw() || metadata.gid() != gid.as_raw() {
        tracing::info!(
            "Changing ownership of '{}' to {}:{}",
            path.display(),
            uid,
            gid
        );
        nix::unistd::fchown(file, Some(uid), Some(gid))
            .with_context(|| format!("Failed to change ownership of '{}'", path.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Valid public keys, [`PublicKeys`] rejects anything that does not parse
    const KEY1: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBV7RbtHsMgxdZoHYjAxh4myaRJ0ujTrHkww1YmbpY67 key1@host";
    const KEY2: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF39Jis8OSS4JRN+T/Putk9u5ym85EMfRPKM8mFTlcsH key2@host";

    fn own_ids() -> (nix::unistd::Uid, nix::unistd::Gid) {
        (nix::unistd::getuid(), nix::unistd::getgid())
    }

    fn keys(lines: &[&str]) -> public_keys::PublicKeys {
        lines.join("\n").parse().expect("Failed to parse keys")
    }

    async fn mode_of(path: &path::Path) -> u32 {
        fs::metadata(path)
            .await
            .expect("Failed to read metadata")
            .permissions()
            .mode()
            & 0o7777
    }

    /// The managed block wrapping the given key lines, as it appears in the file
    fn block(lines: &[&str]) -> String {
        let mut s = format!("{MANAGED_BLOCK_START}\n");
        for line in lines {
            s.push_str(line);
            s.push('\n');
        }
        s.push_str(MANAGED_BLOCK_END);
        s.push('\n');
        s
    }

    #[tokio::test]
    async fn creates_ssh_dir_and_file_with_permissions() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let ssh_dir = tmp.path().join(".ssh");
        let (uid, gid) = own_ids();

        sync_authorized_keys(&ssh_dir, &keys(&[KEY1]), uid, gid)
            .await
            .expect("sync failed");

        assert_eq!(mode_of(&ssh_dir).await, SSH_DIR_MODE);
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        assert_eq!(mode_of(&path).await, AUTHORIZED_KEYS_MODE);
        let content = fs::read_to_string(&path).await.expect("read failed");
        assert_eq!(content, block(&[KEY1]));
    }

    #[tokio::test]
    async fn preserves_manual_lines_outside_managed_block() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let ssh_dir = tmp.path().join(".ssh");
        fs::create_dir(&ssh_dir).await.expect("mkdir failed");
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        // Manual content the PublicKeys parser could not round-trip: a comment and a
        // restricted key with an options prefix
        let manual = "# added by ops\ncommand=\"/usr/bin/backup\" ssh-rsa MANUAL backup@host\n";
        fs::write(&path, manual).await.expect("write failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(&ssh_dir, &keys(&[KEY1]), uid, gid)
            .await
            .expect("sync failed");

        let content = fs::read_to_string(&path).await.expect("read failed");
        assert_eq!(content, format!("{manual}{}", block(&[KEY1])));
    }

    #[tokio::test]
    async fn revoked_key_is_removed_from_managed_block() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let ssh_dir = tmp.path().join(".ssh");
        let (uid, gid) = own_ids();

        sync_authorized_keys(&ssh_dir, &keys(&[KEY1, KEY2]), uid, gid)
            .await
            .expect("first sync failed");

        // BBBB2 was revoked on GitHub
        sync_authorized_keys(&ssh_dir, &keys(&[KEY1]), uid, gid)
            .await
            .expect("second sync failed");

        let content = fs::read_to_string(ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME))
            .await
            .expect("read failed");
        assert_eq!(content, block(&[KEY1]));
    }

    #[tokio::test]
    async fn syncing_is_idempotent() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let ssh_dir = tmp.path().join(".ssh");
        let fetched = keys(&[KEY1, KEY2]);
        let (uid, gid) = own_ids();

        sync_authorized_keys(&ssh_dir, &fetched, uid, gid)
            .await
            .expect("first sync failed");
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        let first = fs::read_to_string(&path).await.expect("read failed");

        sync_authorized_keys(&ssh_dir, &fetched, uid, gid)
            .await
            .expect("second sync failed");
        let second = fs::read_to_string(&path).await.expect("read failed");

        assert_eq!(first, second);
        assert_eq!(first.matches(KEY1).count(), 1);
    }

    #[tokio::test]
    async fn empty_fetched_keys_keep_manual_lines_and_empty_the_block() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let ssh_dir = tmp.path().join(".ssh");
        fs::create_dir(&ssh_dir).await.expect("mkdir failed");
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        fs::write(
            &path,
            format!("ssh-rsa MANUAL manually@added\n{}", block(&[KEY1])),
        )
        .await
        .expect("write failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(&ssh_dir, &public_keys::PublicKeys::default(), uid, gid)
            .await
            .expect("sync failed");

        let content = fs::read_to_string(&path).await.expect("read failed");
        assert_eq!(
            content,
            format!("ssh-rsa MANUAL manually@added\n{}", block(&[]))
        );
    }

    #[tokio::test]
    async fn tightens_loose_permissions_without_rewriting() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let ssh_dir = tmp.path().join(".ssh");
        fs::create_dir(&ssh_dir).await.expect("mkdir failed");
        fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o755))
            .await
            .expect("chmod failed");
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        // Content already matches what the sync would render, so this takes the fix-up
        // path instead of rewriting the file
        let up_to_date = format!("ssh-rsa MANUAL manually@added\n{}", block(&[KEY1]));
        fs::write(&path, &up_to_date).await.expect("write failed");
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .await
            .expect("chmod failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(&ssh_dir, &keys(&[KEY1]), uid, gid)
            .await
            .expect("sync failed");

        assert_eq!(mode_of(&ssh_dir).await, SSH_DIR_MODE);
        assert_eq!(mode_of(&path).await, AUTHORIZED_KEYS_MODE);
        let content = fs::read_to_string(&path).await.expect("read failed");
        assert_eq!(content, up_to_date);
    }

    #[tokio::test]
    async fn unterminated_block_is_treated_as_managed_to_end_of_file() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let ssh_dir = tmp.path().join(".ssh");
        fs::create_dir(&ssh_dir).await.expect("mkdir failed");
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        fs::write(
            &path,
            format!(
                "ssh-rsa MANUAL manually@added\n{MANAGED_BLOCK_START}\nssh-ed25519 STALE gone@host\n"
            ),
        )
        .await
        .expect("write failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(&ssh_dir, &keys(&[KEY1]), uid, gid)
            .await
            .expect("sync failed");

        let content = fs::read_to_string(&path).await.expect("read failed");
        assert_eq!(
            content,
            format!("ssh-rsa MANUAL manually@added\n{}", block(&[KEY1]))
        );
    }

    #[tokio::test]
    async fn duplicate_managed_blocks_are_collapsed_into_one() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let ssh_dir = tmp.path().join(".ssh");
        fs::create_dir(&ssh_dir).await.expect("mkdir failed");
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        fs::write(
            &path,
            format!(
                "{}ssh-rsa MANUAL manually@added\n{}",
                block(&["ssh-ed25519 STALE1 gone@host"]),
                block(&["ssh-ed25519 STALE2 gone@host"])
            ),
        )
        .await
        .expect("write failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(&ssh_dir, &keys(&[KEY1]), uid, gid)
            .await
            .expect("sync failed");

        // The block is re-rendered at the position of the first one; the manual line that
        // followed it keeps its place after the block
        let content = fs::read_to_string(&path).await.expect("read failed");
        assert_eq!(
            content,
            format!("{}ssh-rsa MANUAL manually@added\n", block(&[KEY1]))
        );
    }

    #[tokio::test]
    async fn manual_lines_after_the_block_stay_in_place() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let ssh_dir = tmp.path().join(".ssh");
        fs::create_dir(&ssh_dir).await.expect("mkdir failed");
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        fs::write(
            &path,
            format!(
                "# preamble\n{}# trailer\ncommand=\"/usr/bin/backup\" ssh-rsa MANUAL backup@host\n",
                block(&["ssh-ed25519 STALE gone@host"])
            ),
        )
        .await
        .expect("write failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(&ssh_dir, &keys(&[KEY1]), uid, gid)
            .await
            .expect("sync failed");

        let content = fs::read_to_string(&path).await.expect("read failed");
        assert_eq!(
            content,
            format!(
                "# preamble\n{}# trailer\ncommand=\"/usr/bin/backup\" ssh-rsa MANUAL backup@host\n",
                block(&[KEY1])
            )
        );
    }

    #[tokio::test]
    async fn refuses_symlinked_authorized_keys() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let ssh_dir = tmp.path().join(".ssh");
        fs::create_dir(&ssh_dir).await.expect("mkdir failed");
        let victim = tmp.path().join("victim");
        fs::write(&victim, "victim content")
            .await
            .expect("write failed");
        std::os::unix::fs::symlink(&victim, ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME))
            .expect("symlink failed");
        let (uid, gid) = own_ids();

        let result = sync_authorized_keys(&ssh_dir, &keys(&[KEY1]), uid, gid).await;

        assert!(result.is_err());
        let content = fs::read_to_string(&victim).await.expect("read failed");
        assert_eq!(content, "victim content");
    }

    #[tokio::test]
    async fn refuses_symlinked_ssh_dir() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let target = tmp.path().join("target-dir");
        fs::create_dir(&target).await.expect("mkdir failed");
        let ssh_dir = tmp.path().join(".ssh");
        std::os::unix::fs::symlink(&target, &ssh_dir).expect("symlink failed");
        let (uid, gid) = own_ids();

        let result = sync_authorized_keys(&ssh_dir, &keys(&[KEY1]), uid, gid).await;

        assert!(result.is_err());
    }
}
