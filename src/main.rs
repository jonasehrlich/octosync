use anyhow::Context as _;
use clap::Parser as _;
use futures::TryStreamExt as _;
use std::{fs, path, sync};

mod public_keys;
mod store;

/// Sync the members of a GitHub organization with Linux user accounts for new members, installing their public keys for SSH access.
#[derive(clap::Parser, Debug)]
struct Cli {
    /// Name of the organization to query
    #[arg(long)]
    org: String,

    /// App ID of the GitHub App used for authentication (must have org member read permissions)
    #[arg(long, value_parser = |s: &str| s.parse::<u64>().map(octocrab::models::AppId::from))]
    app_id: octocrab::models::AppId,

    /// Path to a file containing the GitHub App private key in PEM format
    #[arg(long)]
    private_key: path::PathBuf,

    /// Preview actions without writing files or changing Linux users
    #[cfg(target_os = "linux")]
    #[arg(long, action = clap::ArgAction::SetTrue)]
    dry_run: bool,
    #[cfg(not(target_os = "linux"))]
    #[arg(skip = true)]
    dry_run: bool,

    /// Enable verbose logging
    #[arg(short, long, action = clap::ArgAction::SetTrue)]
    verbose: bool,
}

#[derive(Debug)]
struct Context {
    octocrab: octocrab::Octocrab,
    config: sync::Arc<Cli>,
}

impl Context {
    async fn try_from_config(config: sync::Arc<Cli>) -> anyhow::Result<Self> {
        let octocrab = org_client(&config).await?;
        Ok(Context { octocrab, config })
    }

    pub fn config(&self) -> &Cli {
        &self.config
    }

    pub fn octocrab(&self) -> &octocrab::Octocrab {
        &self.octocrab
    }
}

async fn org_client(args: &Cli) -> anyhow::Result<octocrab::Octocrab> {
    let private_key = fs::read(args.private_key.as_path()).with_context(|| {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = Cli::parse();
    env_logger::builder()
        .filter_level(if args.verbose {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        })
        .init();

    if !cfg!(target_os = "linux") {
        args.dry_run = true;
        log::warn!("Non-Linux host detected: running in dry-run mode for user management");
    }

    if args.dry_run {
        log::info!("Running in dry-run mode: no changes will be made to Linux users or files");
    }
    let ctx = sync::Arc::new(Context::try_from_config(sync::Arc::new(args)).await?);

    let store = store::Store::new(
        ctx.clone(),
        directories::ProjectDirs::from("", "", env!("CARGO_PKG_NAME"))
            .context("Error determining project directory")?
            .data_dir()
            .to_path_buf(),
    )?;

    let current_members = get_all_org_members(&ctx).await?;
    serde_json::to_writer_pretty(std::io::stdout(), &current_members)?;

    store.save()?;
    Ok(())
}

async fn get_all_org_members(ctx: &Context) -> anyhow::Result<Vec<octocrab::models::Author>> {
    let stream = ctx
        .octocrab()
        .orgs(&ctx.config().org)
        .list_members()
        .per_page(100)
        .send()
        .await
        .with_context(|| format!("Failed to list members for org '{}'", ctx.config().org))?
        .into_stream(ctx.octocrab());

    Ok(stream.try_collect().await?)
}
