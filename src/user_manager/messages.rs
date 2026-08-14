//! The messages of the platform user manager contract.
//!
//! A platform backend is an actor handling the full message set. The messages carry
//! owned data so they can cross the actor boundary.

use crate::{archiver, public_keys, store};
use std::path;

/// Creates a platform user for the given GitHub user without a password.
#[hannibal::message(response = anyhow::Result<store::User>)]
pub struct CreateUser {
    pub gh_user: octocrab::models::Author,
}

/// Renames the platform user of `available_user` (login and home directory) to the
/// GitHub login of `gh_user`, re-creating the account with the stored UID when it no
/// longer exists on the system.
#[hannibal::message(response = anyhow::Result<store::User>)]
pub struct UpdateUser {
    pub gh_user: octocrab::models::Author,
    pub available_user: store::User,
}

/// Prepares the deletion of a platform user: verifies that the stored user still
/// matches the platform account and stops everything that could block the deletion
/// or keep writing to the home directory while it is archived.
#[hannibal::message(response = anyhow::Result<DeletionPreparation>)]
pub struct PrepareUserDeletion {
    pub user: store::User,
}

/// Outcome of [`PrepareUserDeletion`].
#[derive(Debug)]
pub enum DeletionPreparation {
    /// No platform account for the user exists, there is nothing to delete.
    NothingToDo,
    /// The account can be removed once its home directory is archived.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Prepared { home_dir: path::PathBuf },
}

/// Removes the platform account of a prepared user. Taking an
/// [`archiver::ArchiveReceipt`] forces the home directory to be archived before the
/// account and its files can be deleted.
#[hannibal::message(response = anyhow::Result<()>)]
pub struct RemoveAccount {
    pub user: store::User,
    pub receipt: archiver::ArchiveReceipt,
}

/// Synchronizes the supplementary groups of a user.
///
/// octosync owns the supplementary groups of synced users: the user's memberships are
/// replaced with `groups`, keeping only the primary group. Groups assigned through
/// other channels are removed.
#[hannibal::message(response = anyhow::Result<()>)]
pub struct SyncSupplementaryGroups {
    pub user: store::User,
    pub groups: Vec<String>,
}

/// Ensures that all given groups exist on the system, creating any that are missing.
#[hannibal::message(response = anyhow::Result<()>)]
pub struct EnsureGroupsExist {
    pub groups: Vec<String>,
}

/// Replaces the octosync-managed key block in the user's authorized_keys file with the
/// given keys, so a key revoked on GitHub is removed on the next sync. Lines outside
/// the managed block are never touched, so keys installed through other channels stay
/// intact.
#[hannibal::message(response = anyhow::Result<()>)]
pub struct UpdateAuthorizedKeys {
    pub user: store::User,
    pub keys: public_keys::PublicKeys,
}
