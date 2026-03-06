use anyhow::Context as _;
use clap::Parser as _;
use std::{path, sync};

mod octosync;
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
    let data_dir = directories::ProjectDirs::from("", "", env!("CARGO_PKG_NAME"))
        .context("Error determining project directory")?
        .data_dir()
        .to_path_buf();
    let app = octosync::Octosync::new(sync::Arc::new(args), &data_dir).await?;

    app.sync().await?;

    Ok(())
}
