//! Scripted backend for tests. Every message type pops its response from a queue, so
//! tests can mock successes and failures per operation.

use crate::store;
use crate::user_manager::{
    CreateUser, DeletionPreparation, EnsureGroupsExist, PrepareUserDeletion, RemoveAccount,
    SyncSupplementaryGroups, UpdateAuthorizedKeys, UpdateUser,
};
use std::collections;

#[derive(Default)]
pub struct TestingUserManager {
    pub create_user: collections::VecDeque<anyhow::Result<store::User>>,
    pub update_user: collections::VecDeque<anyhow::Result<store::User>>,
    pub prepare_user_deletion: collections::VecDeque<anyhow::Result<DeletionPreparation>>,
    pub remove_account: collections::VecDeque<anyhow::Result<()>>,
    pub sync_supplementary_groups: collections::VecDeque<anyhow::Result<()>>,
    pub ensure_groups_exist: collections::VecDeque<anyhow::Result<()>>,
    pub update_authorized_keys: collections::VecDeque<anyhow::Result<()>>,
}

fn next_response<T>(
    queue: &mut collections::VecDeque<anyhow::Result<T>>,
    message: &str,
) -> anyhow::Result<T> {
    queue
        .pop_front()
        .unwrap_or_else(|| Err(anyhow::anyhow!("No scripted response left for {message}")))
}

impl hannibal::Actor for TestingUserManager {}

impl hannibal::Handler<CreateUser> for TestingUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        _msg: CreateUser,
    ) -> anyhow::Result<store::User> {
        next_response(&mut self.create_user, "CreateUser")
    }
}

impl hannibal::Handler<UpdateUser> for TestingUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        _msg: UpdateUser,
    ) -> anyhow::Result<store::User> {
        next_response(&mut self.update_user, "UpdateUser")
    }
}

impl hannibal::Handler<PrepareUserDeletion> for TestingUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        _msg: PrepareUserDeletion,
    ) -> anyhow::Result<DeletionPreparation> {
        next_response(&mut self.prepare_user_deletion, "PrepareUserDeletion")
    }
}

impl hannibal::Handler<RemoveAccount> for TestingUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        _msg: RemoveAccount,
    ) -> anyhow::Result<()> {
        next_response(&mut self.remove_account, "RemoveAccount")
    }
}

impl hannibal::Handler<SyncSupplementaryGroups> for TestingUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        _msg: SyncSupplementaryGroups,
    ) -> anyhow::Result<()> {
        next_response(
            &mut self.sync_supplementary_groups,
            "SyncSupplementaryGroups",
        )
    }
}

impl hannibal::Handler<EnsureGroupsExist> for TestingUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        _msg: EnsureGroupsExist,
    ) -> anyhow::Result<()> {
        next_response(&mut self.ensure_groups_exist, "EnsureGroupsExist")
    }
}

impl hannibal::Handler<UpdateAuthorizedKeys> for TestingUserManager {
    async fn handle(
        &mut self,
        _ctx: &mut hannibal::Context<Self>,
        _msg: UpdateAuthorizedKeys,
    ) -> anyhow::Result<()> {
        next_response(&mut self.update_authorized_keys, "UpdateAuthorizedKeys")
    }
}
