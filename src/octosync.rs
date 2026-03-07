use crate::{Cli, store, user_manager, user_manager::CreateUser as _};
use anyhow::Context as _;
use std::{path, sync};
use tokio::fs;

async fn org_client(args: &Cli) -> anyhow::Result<octocrab::Octocrab> {
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
    octocrab: sync::Arc<octocrab::Octocrab>,
    // _http: reqwest::Client,
    config: sync::Arc<Cli>,
    data_dir: path::PathBuf,
    user_manager: user_manager::PlatformUserManager,
}

impl Octosync {
    pub async fn new(config: sync::Arc<Cli>, data_dir: &path::Path) -> anyhow::Result<Self> {
        Ok(Self {
            octocrab: sync::Arc::new(org_client(&config).await?),
            config,
            // _http: reqwest::Client::new(),
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
        fields(org = %self.config.org, user = %gh_user.login, id = gh_user.id.into_inner(), )
    )]
    async fn process_user(
        &self,
        gh_user: &octocrab::models::Author,
        store: &store::Store,
    ) -> anyhow::Result<store::User> {
        let new_user = match store.users().get(&gh_user.id) {
            Some(user) => self.manage_existing_user(gh_user, user).await?,
            None => self.create_user(gh_user).await?,
        };
        // Check for SSH keys and update them if necessary
        Ok(new_user)
    }

    async fn create_user(&self, gh_user: &octocrab::models::Author) -> anyhow::Result<store::User> {
        if self.config.dry_run {
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

    pub async fn sync(self) -> anyhow::Result<()> {
        let (org_members, store) = tokio::try_join!(
            get_all_org_members(&self.octocrab, &self.config.org),
            store::Store::new(&self.data_dir)
        )?;
        tracing::info!(
            "Successfully retrieved {} members for org '{}'",
            org_members.len(),
            self.config.org
        );

        let tasks = org_members
            .iter()
            .map(|gh_user| self.process_user(gh_user, &store));
        futures::future::join_all(tasks).await;

        store.save().await?;
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
