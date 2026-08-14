//! The messages of the platform user manager contract.
//!
//! A platform backend is an actor handling the full message set. The messages carry
//! owned data so they can cross the actor boundary.

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
    /// The stored IDs of a rejoining member whose account is gone from the system
    /// (purged, or removed by hand): the account is created with exactly this UID and
    /// its private group with exactly this GID, so file ownership survives the removal
    /// and re-create cycle. When either ID is taken, creation fails instead of falling
    /// back to a fresh one. A tombstone migrated from v1 carries no GID and leaves the
    /// private group to `useradd`.
    Stored {
        uid: nix::unistd::Uid,
        gid: Option<nix::unistd::Gid>,
    },
    /// System-allocated IDs for a brand-new member, avoiding the IDs reserved by
    /// tombstones: shadow-utils allocates the highest ID in range plus one, so
    /// purging the user with the highest UID frees exactly the next UID to be
    /// allocated.
    Fresh {
        reserved_uids: collections::HashSet<nix::unistd::Uid>,
        reserved_gids: collections::HashSet<nix::unistd::Gid>,
    },
}

/// Renames the platform user of `available_user` (login and home directory) to the
/// GitHub login of `gh_user`, re-creating the account with the stored name, UID and
/// GID first when it no longer exists on the system. Refuses when the stored UID or
/// name belongs to a different account.
#[hannibal::message(response = anyhow::Result<store::User>)]
pub struct UpdateUser {
    pub gh_user: octocrab::models::Author,
    pub available_user: store::User,
}

/// Expires the platform account of a departed user, replacing its deletion: verifies
/// that the stored user still matches the platform account, expires it so no new
/// session (password or pubkey SSH) can start, ends the running sessions and strips
/// the supplementary groups. The account, its home directory and its primary group
/// stay on the machine as the durable departure record, so the departure is a
/// reversible lockout that a rejoin heals, until the purge removes the account after
/// the retention period.
///
/// Idempotent, so the sync can send it for every departed user on every run and an
/// expiry interrupted at any point converges.
#[hannibal::message(response = anyhow::Result<()>)]
pub struct ExpireAccount {
    pub user: store::User,
}

/// Permanently removes the expired platform account of a departed user with
/// `userdel --remove`, deleting the home directory without an archive: the one
/// deliberately irreversible operation left now that expiry replaced deletion.
///
/// The account-side half of the purge eligibility is verified here: the shadow expiry
/// must be set and at most `expired_before`. It is the evidence on the machine itself
/// that survives store damage and is cleared on any reactivation, so a wrongly
/// resurrected tombstone can never purge a live account.
#[hannibal::message(response = anyhow::Result<PurgeOutcome>)]
pub struct PurgeAccount {
    pub user: store::User,
    /// Latest shadow expiry, in days since the epoch, an account may have to be purged
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub expired_before: i64,
}

/// Outcome of [`PurgeAccount`]. Only the Linux backend can verify the account-side
/// eligibility, the mock previews every purge as performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeOutcome {
    /// The account and its home directory were removed
    Purged,
    /// No account for the user exists on the system, nothing was done
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NoAccount,
    /// The account's shadow expiry is missing or newer than `expired_before`: the
    /// account-side clock does not agree with the store's, nothing was done
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NotExpired,
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
