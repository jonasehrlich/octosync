use crate::{Cli, store};
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
    log::debug!(
        "Successfully authenticated to GitHub API, starting member sync for org '{}'",
        args.org
    );
    Ok(install_crab)
}

#[derive(Debug)]
pub struct Octosync {
    octocrab: sync::Arc<octocrab::Octocrab>,
    _http: reqwest::Client,
    config: sync::Arc<Cli>,
    data_dir: path::PathBuf,
}

impl Octosync {
    pub async fn new(config: sync::Arc<Cli>, data_dir: &path::Path) -> anyhow::Result<Self> {
        Ok(Self {
            octocrab: sync::Arc::new(org_client(&config).await?),
            config,
            _http: reqwest::Client::new(),
            data_dir: data_dir.to_path_buf(),
        })
    }

    async fn process_user(
        &self,
        gh_user: &octocrab::models::Author,
        store: &store::Store,
    ) -> anyhow::Result<()> {
        log::debug!("Processing user '{}'", gh_user.login);

        match store.users().get(&gh_user.id) {
            Some(user) => self.manage_existing_user(gh_user, user).await?,
            None => self.manage_new_user(gh_user).await?,
        }

        Ok(())
    }

    async fn manage_existing_user(
        &self,
        gh_user: &octocrab::models::Author,
        user: &store::User,
    ) -> anyhow::Result<()> {
        log::debug!(
            "User '{}' already exists in store {:?}",
            gh_user.login,
            user
        );
        Ok(())
    }

    async fn manage_new_user(&self, gh_user: &octocrab::models::Author) -> anyhow::Result<()> {
        log::info!(
            "New user '{}' (ID {}) found in org '{}', creating Linux user account",
            gh_user.login,
            gh_user.id,
            self.config.org
        );
        Ok(())
    }

    pub async fn sync(self) -> anyhow::Result<()> {
        let mut store = store::Store::new(&self.data_dir).await?;
        let org_members: Vec<octocrab::models::Author> = async {
            let (org_members, _) = tokio::try_join!(
                get_all_org_members(&self.octocrab, &self.config.org),
                store.load()
            )?;
            log::info!(
                "Successfully retrieved {} members for org '{}'",
                org_members.len(),
                self.config.org
            );
            Ok::<Vec<octocrab::models::Author>, anyhow::Error>(org_members)
        }
        .await?;

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
