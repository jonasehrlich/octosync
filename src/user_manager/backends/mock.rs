//! Mock backend used for dry runs and on non-Linux platforms: logs the intended
//! actions without changing anything on the system.

use crate::store;
use crate::user_manager::{
    AccountIds, CreateUser, DeletionPreparation, EnsureGroupsExist, PrepareUserDeletion,
    RemoveAccount, SyncSupplementaryGroups, UpdateAuthorizedKeys, UpdateUser,
};

#[derive(Debug)]
pub struct MockUserManager {
    /// Next UID handed out for mock-created users. The actor owns its state
    /// exclusively, so a plain counter suffices.
    next_uid: usize,
}

impl MockUserManager {
    pub fn new(base_uid: usize) -> Self {
        Self { next_uid: base_uid }
    }
}

impl hannibal::Actor for MockUserManager {}

impl hannibal::Handler<CreateUser> for MockUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: CreateUser,
    ) -> anyhow::Result<store::User> {
        // Mimic the user-private group Linux creates: same numeric ID as the user
        let (uid, gid) = match &msg.ids {
            AccountIds::Stored { uid, gid } => (
                *uid,
                gid.unwrap_or_else(|| nix::unistd::Gid::from_raw(uid.as_raw())),
            ),
            AccountIds::Fresh {
                reserved_uids,
                reserved_gids,
            } => loop {
                self.next_uid += 1;
                let uid = nix::unistd::Uid::from_raw(self.next_uid as _);
                let gid = nix::unistd::Gid::from_raw(self.next_uid as _);
                if !reserved_uids.contains(&uid) && !reserved_gids.contains(&gid) {
                    break (uid, gid);
                }
            },
        };
        tracing::info!(
            "Mock creating user '{}' with UID {} and GID {} (not actually creating users on non-Linux OS)",
            msg.gh_user.login,
            uid,
            gid
        );
        Ok(store::User::builder()
            .name(msg.gh_user.login.clone())
            .uid(uid)
            .gid(gid)
            .id(msg.gh_user.id)
            .build())
    }
}

impl hannibal::Handler<UpdateUser> for MockUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: UpdateUser,
    ) -> anyhow::Result<store::User> {
        // Read-only system lookup so a dry run previews the re-create path truthfully
        if nix::unistd::User::from_uid(msg.available_user.uid())
            .ok()
            .flatten()
            .is_none()
        {
            tracing::info!(
                user=%msg.gh_user.login,
                uid=%msg.available_user.uid(),
                "Mock re-creating account (not actually creating users on non-Linux OS)",
            );
            return Ok(store::User::builder()
                .id(msg.available_user.id())
                .uid(msg.available_user.uid())
                .maybe_gid(msg.available_user.gid())
                .name(msg.gh_user.login.clone())
                .build());
        }
        if msg.gh_user.login != msg.available_user.name() {
            tracing::info!(
                "Mock updating username from '{}' to '{}' (not actually updating users on non-Linux OS)",
                msg.available_user.name(),
                msg.gh_user.login
            );
            Ok(store::User::builder()
                .id(msg.available_user.id())
                .uid(msg.available_user.uid())
                .maybe_gid(msg.available_user.gid())
                .name(msg.gh_user.login.clone())
                .build())
        } else {
            Ok(msg.available_user)
        }
    }
}

impl hannibal::Handler<PrepareUserDeletion> for MockUserManager {
    #[tracing::instrument(
        name = "UserManager::prepare_user_deletion",
        skip_all,
        fields(user = %msg.user.name(), uid = msg.user.uid().as_raw())
    )]
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: PrepareUserDeletion,
    ) -> anyhow::Result<DeletionPreparation> {
        tracing::info!("Would expire the account, archive the home directory and delete the user");
        Ok(DeletionPreparation::NothingToDo)
    }
}

impl hannibal::Handler<RemoveAccount> for MockUserManager {
    // Unreachable through the mock flow, since the preparation never reports an
    // account to remove, but the contract requires handling the full message set
    #[tracing::instrument(
        name = "UserManager::remove_account",
        skip_all,
        fields(user = %msg.user.name(), uid = msg.user.uid().as_raw())
    )]
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: RemoveAccount,
    ) -> anyhow::Result<()> {
        tracing::info!(
            archive = ?msg.receipt.archive_path(),
            "Mock removing account (not actually deleting users on non-Linux OS)"
        );
        Ok(())
    }
}

impl hannibal::Handler<SyncSupplementaryGroups> for MockUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: SyncSupplementaryGroups,
    ) -> anyhow::Result<()> {
        tracing::info!(
            user = %msg.user.name(),
            groups = ?msg.groups,
            "Mock syncing supplementary groups (not actually managing groups on non-Linux OS)"
        );
        Ok(())
    }
}

impl hannibal::Handler<EnsureGroupsExist> for MockUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: EnsureGroupsExist,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "Mock ensuring groups exist: {:?} (not actually managing groups on non-Linux OS)",
            msg.groups,
        );
        Ok(())
    }
}

impl hannibal::Handler<UpdateAuthorizedKeys> for MockUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        msg: UpdateAuthorizedKeys,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "Mock updating authorized keys for user '{}' to {} keys (not actually managing keys on non-Linux OS)",
            msg.user.name(),
            msg.keys.len()
        );
        Ok(())
    }
}
