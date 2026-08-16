//! Maintains an octosync-managed block in each user's authorized_keys file while
//! preserving all manual entries.
//!
//! These operations run as root inside a directory the user owns and can replace at any
//! moment, so the user-controlled path is resolved exactly once: the home and `.ssh`
//! directories are opened without following symlinks, and every later open, rename,
//! permission and ownership change is performed relative to those pinned descriptors.
//! Swapping `.ssh` after it was validated therefore cannot redirect a later step, and
//! the atomic replacement of the file happens inside the pinned directory.

use crate::public_keys;
use anyhow::Context as _;
use nix::{
    errno::Errno,
    fcntl::{OFlag, open, openat, renameat},
    sys::stat::{Mode, SFlag, fchmod, fstat, mkdirat, mode_t},
    unistd::{Gid, Uid, UnlinkatFlags, User, fchown, fsync, unlinkat},
};
use std::{
    io::{Read as _, Write as _},
    os::fd::OwnedFd,
    path::{Path, PathBuf},
};

const SSH_DIR_MODE: u32 = 0o700;
const AUTHORIZED_KEYS_MODE: u32 = 0o600;
const SSH_DIR_NAME: &str = ".ssh";
const AUTHORIZED_KEYS_FILE_NAME: &str = "authorized_keys";
/// Name the new content is staged under before it is renamed into place. It lives inside
/// the pinned `.ssh` directory, so the rename never crosses a resolvable path.
const STAGING_FILE_NAME: &str = ".authorized_keys.octosync";

/// The mode constants above are plain `u32`. [`Mode`] is backed by the platform's
/// `mode_t`, which is narrower on some targets.
fn mode(bits: u32) -> Mode {
    Mode::from_bits_truncate(bits as mode_t)
}

const MANAGED_BLOCK_START: &str = "# >>> octosync managed keys - do not edit this block >>>";
const MANAGED_BLOCK_END: &str = "# <<< octosync managed keys - do not edit this block <<<";

/// Replace the octosync-managed block with the user's current public keys.
#[tracing::instrument(
    name = "authorized_keys::update_authorized_keys",
    skip_all,
    fields(user = %user.name)
)]
pub fn update_authorized_keys(user: &User, keys: &public_keys::PublicKeys) -> anyhow::Result<()> {
    sync_authorized_keys(&user.dir, keys, user.uid, user.gid)
}

/// Remove every authorized key of a departed member.
#[tracing::instrument(
    name = "authorized_keys::remove_authorized_keys",
    skip_all,
    fields(user = %user.name)
)]
pub fn remove_authorized_keys(user: &User) -> anyhow::Result<()> {
    let Some(ssh_dir) = DotSSHDirectory::open(&user.dir)? else {
        tracing::debug!("Departed user has no .ssh directory, no keys to remove");
        return Ok(());
    };
    ssh_dir.remove_authorized_keys()
}

/// Replace the managed block while preserving content outside it.
fn sync_authorized_keys(
    home: &Path,
    keys: &public_keys::PublicKeys,
    uid: Uid,
    gid: Gid,
) -> anyhow::Result<()> {
    DotSSHDirectory::create(home, uid, gid)?.set_managed_keys(keys, uid, gid)
}

/// A user's `.ssh` directory, pinned by an open descriptor.
struct DotSSHDirectory {
    fd: OwnedFd,
    /// Path the descriptor was opened from, used for messages only
    path: PathBuf,
}

impl DotSSHDirectory {
    /// Pin the `.ssh` directory of `home`, or `None` when the user has none.
    fn open(home: &Path) -> anyhow::Result<Option<Self>> {
        Self::open_inner(home, false)
    }

    /// Pin the `.ssh` directory of `home`, creating it when it does not exist yet.
    fn create(home: &Path, uid: Uid, gid: Gid) -> anyhow::Result<Self> {
        let path = home.join(SSH_DIR_NAME);
        let ssh_dir = Self::open_inner(home, true)?.with_context(|| {
            format!(
                "The .ssh directory '{}' disappeared right after it was created",
                path.display()
            )
        })?;
        fchmod(&ssh_dir.fd, mode(SSH_DIR_MODE))
            .with_context(|| format!("Failed to set permissions on '{}'", path.display()))?;
        fchown(&ssh_dir.fd, Some(uid), Some(gid))
            .with_context(|| format!("Failed to set ownership on '{}'", path.display()))?;
        Ok(ssh_dir)
    }

    fn open_inner(home: &Path, create: bool) -> anyhow::Result<Option<Self>> {
        let home_fd = open(
            home,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("Failed to open home directory '{}'", home.display()))?;
        let path = home.join(SSH_DIR_NAME);
        if create {
            match mkdirat(&home_fd, SSH_DIR_NAME, mode(SSH_DIR_MODE)) {
                Ok(()) => tracing::info!("Created .ssh directory '{}'", path.display()),
                Err(Errno::EEXIST) => {}
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("Failed to create .ssh directory '{}'", path.display())
                    });
                }
            }
        }

        let fd = match openat(
            &home_fd,
            SSH_DIR_NAME,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to open .ssh directory '{}'", path.display(),)
                });
            }
        };

        Ok(Some(Self { fd, path }))
    }

    fn file_path(&self) -> PathBuf {
        self.path.join(AUTHORIZED_KEYS_FILE_NAME)
    }

    /// Rewrite the managed block to hold exactly `keys`, creating the file if needed.
    fn set_managed_keys(
        &self,
        keys: &public_keys::PublicKeys,
        uid: Uid,
        gid: Gid,
    ) -> anyhow::Result<()> {
        let current = self.read_file()?.unwrap_or_default();
        let (before, after) = split_around_managed_block(&current);
        let updated = render_content(&before, &after, keys);
        self.replace(updated, uid, gid)?;
        tracing::info!(
            "Updated managed block in '{}' to {} public keys",
            self.file_path().display(),
            keys.len(),
        );
        Ok(())
    }

    /// Delete the complete authorized_keys file through the pinned directory.
    fn remove_authorized_keys(&self) -> anyhow::Result<()> {
        match unlinkat(
            &self.fd,
            AUTHORIZED_KEYS_FILE_NAME,
            UnlinkatFlags::NoRemoveDir,
        ) {
            Ok(()) => {}
            Err(Errno::ENOENT) => {
                tracing::debug!("Departed user has no authorized_keys file, no keys to remove");
                return Ok(());
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to remove '{}'", self.file_path().display()));
            }
        }
        fsync(&self.fd).with_context(|| {
            format!(
                "Failed to persist authorized_keys removal in '{}'",
                self.path.display()
            )
        })?;
        tracing::info!(
            "Removed all authorized keys from '{}'",
            self.file_path().display()
        );
        Ok(())
    }

    /// Read the current authorized_keys file, or `None` when there is none.
    ///
    fn read_file(&self) -> anyhow::Result<Option<String>> {
        let fd = match openat(
            &self.fd,
            AUTHORIZED_KEYS_FILE_NAME,
            OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "Failed to open '{}' (is it a symlink?)",
                        self.file_path().display()
                    )
                });
            }
        };

        let stat = fstat(&fd)
            .with_context(|| format!("Failed to inspect '{}'", self.file_path().display()))?;
        if !SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFREG) {
            anyhow::bail!("'{}' is not a regular file", self.file_path().display());
        }

        let mut file = std::fs::File::from(fd);
        let mut content = String::new();
        file.read_to_string(&mut content)
            .with_context(|| format!("Failed to read '{}'", self.file_path().display()))?;
        Ok(Some(content))
    }

    /// Atomically replace authorized_keys with `content`, owned by the user and with the
    /// authorized_keys mode. Ownership and permissions are set through the descriptor
    /// before the rename, so the file never appears at its final name with the wrong
    /// ones, and both names are resolved inside the pinned directory.
    fn replace(&self, content: String, uid: Uid, gid: Gid) -> anyhow::Result<()> {
        // A staging file left behind by an interrupted run would block the exclusive
        // create below.
        self.remove_staging_file();

        let result = (|| {
            let fd = openat(
                &self.fd,
                STAGING_FILE_NAME,
                OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC,
                mode(AUTHORIZED_KEYS_MODE),
            )
            .context("Failed to create the staging file")?;

            let mut file = std::fs::File::from(fd);
            file.write_all(content.as_bytes())
                .context("Failed to write the staging file")?;
            let fd = OwnedFd::from(file);

            // The creation mode is masked by the umask, so set it explicitly before
            // publishing the file.
            fchmod(&fd, mode(AUTHORIZED_KEYS_MODE))
                .context("Failed to set the permissions of the staging file")?;
            fchown(&fd, Some(uid), Some(gid))
                .context("Failed to set the ownership of the staging file")?;
            fsync(&fd).context("Failed to flush the staging file to disk")?;
            drop(fd);

            renameat(
                &self.fd,
                STAGING_FILE_NAME,
                &self.fd,
                AUTHORIZED_KEYS_FILE_NAME,
            )
            .context("Failed to move the staging file into place")?;
            fsync(&self.fd).context("Failed to persist the file replacement")
        })();

        if result.is_err() {
            self.remove_staging_file();
        }
        result
    }

    /// Drop a staging file, ignoring an absent one.
    fn remove_staging_file(&self) {
        if let Err(e) = unlinkat(&self.fd, STAGING_FILE_NAME, UnlinkatFlags::NoRemoveDir)
            && e != Errno::ENOENT
        {
            tracing::warn!(
                "Failed to remove the staging file in '{}': {e}",
                self.path.display()
            );
        }
    }
}

/// Split `content` into the lines before the first octosync-managed block and the lines
/// after it, removing every managed block. The split point marks where the block is
/// re-rendered, so manual lines keep their position relative to it.
///
/// A start marker without a matching end marker means the block was tampered with or an
/// editor stripped it. Everything after the start marker is treated as managed so stale
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    /// Valid public keys, [`PublicKeys`] rejects anything that does not parse
    const KEY1: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBV7RbtHsMgxdZoHYjAxh4myaRJ0ujTrHkww1YmbpY67 key1@host";
    const KEY2: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF39Jis8OSS4JRN+T/Putk9u5ym85EMfRPKM8mFTlcsH key2@host";

    fn own_ids() -> (nix::unistd::Uid, nix::unistd::Gid) {
        (nix::unistd::getuid(), nix::unistd::getgid())
    }

    fn keys(lines: &[&str]) -> public_keys::PublicKeys {
        lines.join("\n").parse().expect("Failed to parse keys")
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path)
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

    /// A home directory with an existing `.ssh` directory
    fn home_with_ssh_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let ssh_dir = tmp.path().join(SSH_DIR_NAME);
        fs::create_dir(&ssh_dir).expect("mkdir failed");
        (tmp, ssh_dir)
    }

    #[test]
    fn creates_ssh_dir_and_file_with_permissions() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let (uid, gid) = own_ids();

        sync_authorized_keys(tmp.path(), &keys(&[KEY1]), uid, gid).expect("sync failed");

        let ssh_dir = tmp.path().join(SSH_DIR_NAME);
        assert_eq!(mode_of(&ssh_dir), SSH_DIR_MODE);
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        assert_eq!(mode_of(&path), AUTHORIZED_KEYS_MODE);
        let content = fs::read_to_string(&path).expect("read failed");
        assert_eq!(content, block(&[KEY1]));
    }

    #[test]
    fn preserves_manual_lines_outside_managed_block() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        // Manual content the PublicKeys parser could not round-trip: a comment and a
        // restricted key with an options prefix
        let manual = "# added by ops\ncommand=\"/usr/bin/backup\" ssh-rsa MANUAL backup@host\n";
        fs::write(&path, manual).expect("write failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(tmp.path(), &keys(&[KEY1]), uid, gid).expect("sync failed");

        let content = fs::read_to_string(&path).expect("read failed");
        assert_eq!(content, format!("{manual}{}", block(&[KEY1])));
    }

    #[test]
    fn revoked_key_is_removed_from_managed_block() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let (uid, gid) = own_ids();

        sync_authorized_keys(tmp.path(), &keys(&[KEY1, KEY2]), uid, gid)
            .expect("first sync failed");

        // KEY2 was revoked on GitHub
        sync_authorized_keys(tmp.path(), &keys(&[KEY1]), uid, gid).expect("second sync failed");

        let content = fs::read_to_string(
            tmp.path()
                .join(SSH_DIR_NAME)
                .join(AUTHORIZED_KEYS_FILE_NAME),
        )
        .expect("read failed");
        assert_eq!(content, block(&[KEY1]));
    }

    #[test]
    fn syncing_is_idempotent() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let fetched = keys(&[KEY1, KEY2]);
        let (uid, gid) = own_ids();

        sync_authorized_keys(tmp.path(), &fetched, uid, gid).expect("first sync failed");
        let path = tmp
            .path()
            .join(SSH_DIR_NAME)
            .join(AUTHORIZED_KEYS_FILE_NAME);
        let first = fs::read_to_string(&path).expect("read failed");

        sync_authorized_keys(tmp.path(), &fetched, uid, gid).expect("second sync failed");
        let second = fs::read_to_string(&path).expect("read failed");

        assert_eq!(first, second);
        assert_eq!(first.matches(KEY1).count(), 1);
    }

    #[test]
    fn empty_fetched_keys_keep_manual_lines_and_empty_the_block() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        fs::write(
            &path,
            format!("ssh-rsa MANUAL manually@added\n{}", block(&[KEY1])),
        )
        .expect("write failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(tmp.path(), &public_keys::PublicKeys::default(), uid, gid)
            .expect("sync failed");

        let content = fs::read_to_string(&path).expect("read failed");
        assert_eq!(
            content,
            format!("ssh-rsa MANUAL manually@added\n{}", block(&[]))
        );
    }

    #[test]
    fn repairs_loose_permissions() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o755))
            .expect("chmod failed");
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        // Content already matches, but the replacement still corrects its metadata.
        let up_to_date = format!("ssh-rsa MANUAL manually@added\n{}", block(&[KEY1]));
        fs::write(&path, &up_to_date).expect("write failed");
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(tmp.path(), &keys(&[KEY1]), uid, gid).expect("sync failed");

        assert_eq!(mode_of(&ssh_dir), SSH_DIR_MODE);
        assert_eq!(mode_of(&path), AUTHORIZED_KEYS_MODE);
        let content = fs::read_to_string(&path).expect("read failed");
        assert_eq!(content, up_to_date);
    }

    #[test]
    fn unterminated_block_is_treated_as_managed_to_end_of_file() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        fs::write(
            &path,
            format!(
                "ssh-rsa MANUAL manually@added\n{MANAGED_BLOCK_START}\nssh-ed25519 STALE gone@host\n"
            ),
        )
        .expect("write failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(tmp.path(), &keys(&[KEY1]), uid, gid).expect("sync failed");

        let content = fs::read_to_string(&path).expect("read failed");
        assert_eq!(
            content,
            format!("ssh-rsa MANUAL manually@added\n{}", block(&[KEY1]))
        );
    }

    #[test]
    fn duplicate_managed_blocks_are_collapsed_into_one() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        fs::write(
            &path,
            format!(
                "{}ssh-rsa MANUAL manually@added\n{}",
                block(&["ssh-ed25519 STALE1 gone@host"]),
                block(&["ssh-ed25519 STALE2 gone@host"])
            ),
        )
        .expect("write failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(tmp.path(), &keys(&[KEY1]), uid, gid).expect("sync failed");

        // The block is rendered at the position of the first one. The manual line that
        // followed it keeps its place after the block.
        let content = fs::read_to_string(&path).expect("read failed");
        assert_eq!(
            content,
            format!("{}ssh-rsa MANUAL manually@added\n", block(&[KEY1]))
        );
    }

    #[test]
    fn manual_lines_after_the_block_stay_in_place() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        fs::write(
            &path,
            format!(
                "# preamble\n{}# trailer\ncommand=\"/usr/bin/backup\" ssh-rsa MANUAL backup@host\n",
                block(&["ssh-ed25519 STALE gone@host"])
            ),
        )
        .expect("write failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(tmp.path(), &keys(&[KEY1]), uid, gid).expect("sync failed");

        let content = fs::read_to_string(&path).expect("read failed");
        assert_eq!(
            content,
            format!(
                "# preamble\n{}# trailer\ncommand=\"/usr/bin/backup\" ssh-rsa MANUAL backup@host\n",
                block(&[KEY1])
            )
        );
    }

    #[test]
    fn refuses_symlinked_authorized_keys() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        let victim = tmp.path().join("victim");
        fs::write(&victim, "victim content").expect("write failed");
        std::os::unix::fs::symlink(&victim, ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME))
            .expect("symlink failed");
        let (uid, gid) = own_ids();

        let result = sync_authorized_keys(tmp.path(), &keys(&[KEY1]), uid, gid);

        assert!(result.is_err());
        let content = fs::read_to_string(&victim).expect("read failed");
        assert_eq!(content, "victim content");
    }

    #[test]
    fn refuses_symlinked_ssh_dir() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let target = tmp.path().join("target-dir");
        fs::create_dir(&target).expect("mkdir failed");
        std::os::unix::fs::symlink(&target, tmp.path().join(SSH_DIR_NAME)).expect("symlink failed");
        let (uid, gid) = own_ids();

        let result = sync_authorized_keys(tmp.path(), &keys(&[KEY1]), uid, gid);

        assert!(result.is_err());
    }

    #[test]
    fn refuses_symlinked_home() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let target = tmp.path().join("elsewhere");
        fs::create_dir(&target).expect("mkdir failed");
        let home = tmp.path().join("home");
        std::os::unix::fs::symlink(&target, &home).expect("symlink failed");
        let (uid, gid) = own_ids();

        let result = sync_authorized_keys(&home, &keys(&[KEY1]), uid, gid);

        assert!(result.is_err());
        assert!(!target.join(SSH_DIR_NAME).exists());
    }

    /// The race the pinning exists for: between validating `.ssh` and writing into it,
    /// the user renames the real directory aside and drops a symlink in its place. Every
    /// later step resolves through the descriptor rather than the name, so the keys land
    /// in the directory that was validated and the substituted target is never touched.
    #[test]
    fn substituting_the_ssh_dir_after_pinning_does_not_redirect_the_write() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        let (uid, gid) = own_ids();
        let dir = DotSSHDirectory::create(tmp.path(), uid, gid).expect("pinning failed");

        let moved_aside = tmp.path().join("real-ssh");
        fs::rename(&ssh_dir, &moved_aside).expect("mv failed");
        let attacker_dir = tmp.path().join("attacker");
        fs::create_dir(&attacker_dir).expect("mkdir failed");
        std::os::unix::fs::symlink(&attacker_dir, &ssh_dir).expect("symlink failed");

        dir.set_managed_keys(&keys(&[KEY1]), uid, gid)
            .expect("write failed");

        assert!(!attacker_dir.join(AUTHORIZED_KEYS_FILE_NAME).exists());
        let content =
            fs::read_to_string(moved_aside.join(AUTHORIZED_KEYS_FILE_NAME)).expect("read failed");
        assert_eq!(content, block(&[KEY1]));
    }

    /// A `.ssh` directory removed after it was pinned makes the write fail cleanly
    /// rather than fall back to resolving the path again.
    #[test]
    fn removing_the_pinned_ssh_dir_fails_the_write() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        let (uid, gid) = own_ids();
        let dir = DotSSHDirectory::create(tmp.path(), uid, gid).expect("pinning failed");

        let attacker_dir = tmp.path().join("attacker");
        fs::create_dir(&attacker_dir).expect("mkdir failed");
        fs::remove_dir(&ssh_dir).expect("rmdir failed");
        std::os::unix::fs::symlink(&attacker_dir, &ssh_dir).expect("symlink failed");

        assert!(dir.set_managed_keys(&keys(&[KEY1]), uid, gid).is_err());
        assert!(!attacker_dir.join(AUTHORIZED_KEYS_FILE_NAME).exists());
    }

    /// Disabling an account removes the complete file, including manual entries.
    #[test]
    fn departure_removes_all_authorized_keys() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        let path = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        let manual = "command=\"/usr/bin/backup\" ssh-rsa MANUAL backup@host\n";
        fs::write(&path, format!("{manual}{}", block(&[KEY1, KEY2]))).expect("write failed");
        DotSSHDirectory::open(tmp.path())
            .expect("open failed")
            .expect("no .ssh directory")
            .remove_authorized_keys()
            .expect("removal failed");

        assert!(!path.exists());
    }

    #[test]
    fn departure_unlinks_authorized_keys_symlink_without_following_it() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        let victim = tmp.path().join("victim");
        fs::write(&victim, "must remain").expect("write failed");
        let authorized_keys = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        std::os::unix::fs::symlink(&victim, &authorized_keys).expect("symlink failed");
        DotSSHDirectory::open(tmp.path())
            .expect("open failed")
            .expect("no .ssh directory")
            .remove_authorized_keys()
            .expect("removal failed");

        assert!(!authorized_keys.exists());
        assert_eq!(
            fs::read_to_string(victim).expect("read failed"),
            "must remain"
        );
    }

    #[test]
    fn replacing_ssh_dir_after_pinning_does_not_redirect_removal() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        let original_file = ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME);
        fs::write(&original_file, "original keys").expect("write failed");
        let dir = DotSSHDirectory::open(tmp.path())
            .expect("open failed")
            .expect("no .ssh directory");

        let moved_aside = tmp.path().join("real-ssh");
        fs::rename(&ssh_dir, &moved_aside).expect("mv failed");
        let replacement = tmp.path().join("replacement");
        fs::create_dir(&replacement).expect("mkdir failed");
        let replacement_file = replacement.join(AUTHORIZED_KEYS_FILE_NAME);
        fs::write(&replacement_file, "replacement keys").expect("write failed");
        std::os::unix::fs::symlink(&replacement, &ssh_dir).expect("symlink failed");

        dir.remove_authorized_keys().expect("removal failed");

        assert!(!moved_aside.join(AUTHORIZED_KEYS_FILE_NAME).exists());
        assert_eq!(
            fs::read_to_string(replacement_file).expect("read failed"),
            "replacement keys"
        );
    }

    /// A departed user without a `.ssh` directory or authorized_keys file must not have
    /// either created for them by the reset.
    #[test]
    fn removal_creates_nothing_when_there_is_nothing_to_remove() {
        let tmp = tempfile::TempDir::new().expect("Failed to create temp dir");
        assert!(
            DotSSHDirectory::open(tmp.path())
                .expect("open failed")
                .is_none()
        );

        let (tmp, ssh_dir) = home_with_ssh_dir();
        DotSSHDirectory::open(tmp.path())
            .expect("open failed")
            .expect("no .ssh directory")
            .remove_authorized_keys()
            .expect("removal failed");

        assert!(!ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME).exists());
    }

    /// A staging file left behind by an interrupted run must not block the next write.
    #[test]
    fn stale_staging_file_does_not_block_the_write() {
        let (tmp, ssh_dir) = home_with_ssh_dir();
        fs::write(ssh_dir.join(STAGING_FILE_NAME), "leftover").expect("write failed");
        let (uid, gid) = own_ids();

        sync_authorized_keys(tmp.path(), &keys(&[KEY1]), uid, gid).expect("sync failed");

        let content =
            fs::read_to_string(ssh_dir.join(AUTHORIZED_KEYS_FILE_NAME)).expect("read failed");
        assert_eq!(content, block(&[KEY1]));
        assert!(!ssh_dir.join(STAGING_FILE_NAME).exists());
    }
}
