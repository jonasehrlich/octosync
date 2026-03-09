use crate::{
    GlobalArgs, InstallationClientArgs, SyncArgs, store,
    user_manager::{self, CreateUser as _, DeleteUser as _, ManageAuthorizedKeys as _},
};
use anyhow::Context as _;
use futures::StreamExt as _;
use std::{collections, path, sync};
use tokio::fs;

async fn org_client(args: &InstallationClientArgs) -> anyhow::Result<octocrab::Octocrab> {
    let private_key = fs::read(args.private_key.as_path())
        .await
        .with_context(|| {
            format!(
                "Failed to read private key from file '{}'",
                args.private_key.display()
            )
        })?;
    let jwt = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_slice())?;

    let app_client = octocrab::Octocrab::builder()
        .app(args.app_id, jwt)
        .build()
        .with_context(|| {
            format!(
                "Failed to build App GitHub client with App ID {} and {}",
                args.app_id,
                args.private_key.display()
            )
        })?;

    let installation = app_client
        .apps()
        .get_org_installation(&args.org)
        .await
        .with_context(|| format!("Failed to get installation for org '{}'", args.org))?;
    let install_crab = app_client.installation(installation.id)?;
    tracing::debug!(
        "Successfully authenticated to GitHub API, starting member sync for org '{}'",
        args.org
    );
    Ok(install_crab)
}

pub struct Octosync {
    global_config: sync::Arc<GlobalArgs>,
    data_dir: path::PathBuf,
    user_manager: user_manager::PlatformUserManager,
}

impl Octosync {
    pub async fn new(
        global_config: sync::Arc<GlobalArgs>,
        data_dir: &path::Path,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            global_config,
            data_dir: data_dir.to_path_buf(),
            #[cfg(target_os = "linux")]
            user_manager: user_manager::PlatformUserManager::new(),
            #[cfg(not(target_os = "linux"))]
            user_manager: user_manager::PlatformUserManager::new(1000),
        })
    }

    #[tracing::instrument(
        name = "Octosync::process_user",
        skip(self, gh_user, store),
        fields(user = %gh_user.login, id = gh_user.id.into_inner(), )
    )]
    async fn process_user(
        &self,
        gh_user: &octocrab::models::Author,
        store: &store::UserStore,
    ) -> anyhow::Result<store::User> {
        let new_user = match store.data().get(&gh_user.id) {
            Some(user) => self.manage_existing_user(gh_user, user).await?,
            None => self.create_user(gh_user).await?,
        };

        self.user_manager
            .update_authorized_keys(&new_user)
            .await
            .context("Failed to sync SSH keys")?;
        Ok(new_user)
    }

    async fn create_user(&self, gh_user: &octocrab::models::Author) -> anyhow::Result<store::User> {
        if self.global_config.dry_run {
            tracing::info!("Would create user for GitHub user '{}'", gh_user.login);
            return Ok(store::User::builder()
                .id(gh_user.id)
                .name(gh_user.login.clone())
                .uid(nix::unistd::Uid::from_raw(1000))
                .build());
        }
        self.user_manager.create_user(gh_user, vec![]).await
    }

    async fn manage_existing_user(
        &self,
        _gh_user: &octocrab::models::Author,
        _user: &store::User,
    ) -> anyhow::Result<store::User> {
        tracing::debug!("User exists in store");

        // TODO: check if it exists on the platform, if not, re-create it
        // TODO: check if groups need to be updated
        // TODO: if everything is up to date, just return the existing user
        Ok(_user.clone())
    }

    #[tracing::instrument(
        name = "Octosync::sync",
        skip(self, args),
        fields(org = %args.octocrab.org)
    )]
    pub async fn sync(self, args: &SyncArgs) -> anyhow::Result<()> {
        let octocrab = sync::Arc::new(org_client(&args.octocrab).await?);

        let (org_members, store) = tokio::try_join!(
            get_all_org_members(&octocrab, &args.octocrab.org),
            store::UserStore::from_dir(&self.data_dir)
        )?;
        tracing::info!("Successfully retrieved {} members", org_members.len());
        let _org_member_map: collections::HashMap<octocrab::models::UserId, String> =
            collections::HashMap::from_iter(
                org_members.iter().map(|user| (user.id, user.login.clone())),
            );

        let self_arc = sync::Arc::new(self);
        let store_arc = sync::Arc::new(store);
        let mut set: tokio::task::JoinSet<_> = org_members
            .into_iter()
            .map(|gh_user| {
                let self_arc = self_arc.clone();
                let store_arc = store_arc.clone();
                async move { self_arc.process_user(&gh_user, &store_arc).await }
            })
            .collect();

        let user_stream = async_stream::stream! {
            while let Some(res) = set.join_next().await {
                yield res;
            }
        };

        let new_store = user_stream
            .filter_map(|r| async move { r.ok()?.ok() })
            .fold(
                store::UserStore::new(&self_arc.data_dir).await?,
                |mut store: store::UserStore, item| async move {
                    store.data_mut().insert(item.id(), item);
                    store
                },
            )
            .await;

        new_store.save().await?;
        Ok(())
    }

    #[tracing::instrument(name = "Octosync::delete", skip(self))]
    pub async fn delete(&self) -> anyhow::Result<()> {
        if self.global_config.dry_run {
            tracing::info!(
                "Would clear all stored user data and delete all Linux users created by octosync"
            );
            return Ok(());
        }
        let store = store::UserStore::from_dir(&self.data_dir).await?;
        let set: tokio::task::JoinSet<_> = store
            .data()
            .values()
            .map(|user| {
                let user = user.clone();
                let user_manager = self.user_manager.clone();
                let dry_run = self.global_config.dry_run;
                async move {
                    if dry_run {
                        tracing::info!("Would delete user '{}'", user.name());
                    } else {
                        user_manager.delete_user(&user).await?;
                    }
                    Ok::<store::User, anyhow::Error>(user)
                }
            })
            .collect();

        set.join_all().await;
        if !self.global_config.dry_run {
            store.delete().await?;
        } else {
            tracing::info!("Would delete store data file");
        }
        Ok(())
    }
}

async fn get_all_org_members(
    octocrab: &octocrab::Octocrab,
    org: &str,
) -> anyhow::Result<Vec<octocrab::models::Author>> {
    use futures::TryStreamExt as _;
    let stream = octocrab
        .orgs(org)
        .list_members()
        .per_page(100)
        .send()
        .await
        .with_context(|| format!("Failed to list members for org '{}'", org))?
        .into_stream(octocrab);

    Ok(stream.try_collect().await?)
}
