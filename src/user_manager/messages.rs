//! Owned messages forming the platform user manager contract.

use crate::{public_keys, store};
use std::collections;

/// Creates a platform user for the given GitHub user without a password.
#[hannibal::message(response = anyhow::Result<store::User>)]
pub struct CreateUser {
    pub gh_user: octocrab::models::Author,
    pub ids: AccountIds,
}

/// UID and GID of the account [`CreateUser`] creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountIds {
    /// Reuse a rejoining member's IDs. Fail if either is unavailable.
    Stored {
        uid: nix::unistd::Uid,
        gid: Option<nix::unistd::Gid>,
    },
    /// Allocate fresh IDs without spending those reserved by tombstones.
    Fresh {
        reserved_uids: collections::HashSet<nix::unistd::Uid>,
        reserved_gids: collections::HashSet<nix::unistd::Gid>,
    },
}

/// Reconcile a stored platform account with the current GitHub user.
///
/// This re-creates the account with the stored name, UID and GID first when the user no longer
/// exists on the system. Refuses when the stored UID or name belongs to a different account.
#[hannibal::message(response = anyhow::Result<store::User>)]
pub struct UpdateUser {
    pub gh_user: octocrab::models::Author,
    pub available_user: store::User,
}

/// Reversibly expire a departed account and remove its active access.
///
/// Verifies that the stored user matches the platform account and expires the account, which
/// prevents logins and SSH key access. The account remains on the system and can be restored later.
#[hannibal::message(response = anyhow::Result<()>)]
pub struct ExpireAccount {
    pub user: store::User,
}

/// Permanently remove a departed user's account and home directory once its shadow
/// expiry meets the cutoff.
#[hannibal::message(response = anyhow::Result<PurgeOutcome>)]
pub struct PurgeAccount {
    pub user: store::User,
    /// Latest shadow-expiry day an account may have to be purged.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub expired_before: i64,
}

/// Result of checking and applying a [`PurgeAccount`] operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeOutcome {
    /// The account and home directory were removed.
    Purged,
    /// No matching account exists.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NoAccount,
    /// The account expiry does not meet the cutoff.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NotExpired,
}

/// Replace a user's supplementary groups, excluding their primary group.
///
/// All supplementary groups of synced users are managed by octosync. Any group not in the given
/// list is removed from the user.
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

/// Replace the octosync-managed authorized_keys block with the current public keys of the given
/// user.
#[hannibal::message(response = anyhow::Result<()>)]
pub struct UpdateAuthorizedKeys {
    pub user: store::User,
    pub keys: public_keys::PublicKeys,
}
