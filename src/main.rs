use anyhow::Context as _;
use clap::Parser as _;
use std::{fs, path};

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
    #[arg(long, action = clap::ArgAction::SetTrue)]
    dry_run: bool,

    /// Enable verbose logging
    #[arg(short, long, action = clap::ArgAction::SetTrue)]
    verbose: bool,
}

#[derive(serde::Deserialize)]
struct GithubKey {
    key: String,
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

    Ok(app_client.installation(installation.id)?)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    env_logger::builder()
        .filter_level(if args.verbose {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        })
        .init();

    let is_linux = cfg!(target_os = "linux");
    let effective_dry_run = args.dry_run || !is_linux;
    if !is_linux {
        log::warn!("Non-Linux host detected: running in dry-run mode for user management");
    }

    let octo = org_client(&args).await?;
    log::debug!(
        "Successfully authenticated to GitHub API, starting member sync for org '{}'",
        args.org
    );

    Ok(())
}
