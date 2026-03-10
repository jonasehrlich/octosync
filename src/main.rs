#![cfg(any(target_os = "linux", target_os = "macos"))]

use anyhow::Context as _;
use clap::Parser as _;
use std::str::FromStr;
use std::{path, sync};

mod octosync;
mod public_keys;
mod store;
mod user_manager;

#[derive(clap::Args, Debug)]
struct InstallationClientArgs {
    /// Name of the organization to query
    #[arg(long)]
    org: String,

    /// App ID of the GitHub App used for authentication (must have org member read permissions)
    #[arg(long, value_parser = |s: &str| s.parse::<u64>().map(octocrab::models::AppId::from))]
    app_id: octocrab::models::AppId,

    /// Path to a file containing the GitHub App private key in PEM format
    #[arg(long)]
    private_key: path::PathBuf,
}

#[derive(clap::Args, Debug)]
struct GlobalArgs {
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

/// Sync the members of a GitHub organization with Linux user accounts for new members,
/// installing their public keys for SSH access.
#[derive(clap::Parser, Debug)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(subcommand)]
    command: Commands,
}

/// Arguments for the sync subcommand
#[derive(clap::Args, Debug)]
struct SyncArgs {
    #[command(flatten)]
    octocrab: InstallationClientArgs,
    /// Groups to add to the users. Can be used multiple times.
    /// To add groups to all users, use `--group <linux-group>`.
    /// To map GitHub Teams to Linux user groups use `--group <gh-team>:<linux-group>`.
    #[arg(long, value_parser = clap::value_parser!(GroupMapping), verbatim_doc_comment)]
    group: Vec<GroupMapping>,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Synchronize GitHub organization members with Linux user accounts
    Sync(SyncArgs),
    /// Delete all stored user data and Linux users created by octosync
    Delete,
}

#[allow(unused)]
#[derive(Debug, Clone)]
enum GroupMapping {
    AddGroup(String),
    MapGitHubTeam {
        gh_team: String,
        linux_group: String,
    },
}

impl FromStr for GroupMapping {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((_a, _b)) = s.split_once(':') {
            Err("Mapping GitHub teams to Linux groups is not implemented yet".to_string())
            // Ok(Self::MapGitHubTeam {
            //     gh_team: a.to_string(),
            //     linux_group: b.to_string(),
            // })
        } else {
            Ok(Self::AddGroup(s.to_string()))
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Failed to install rustls crypto provider"))?;

    let mut args = Cli::parse();
    let level = if args.global.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    tracing_subscriber::fmt()
        .compact()
        .with_max_level(level)
        .with_writer(std::io::stdout)
        .with_file(true)
        .with_line_number(true)
        .with_target(false)
        .init();

    if !cfg!(target_os = "linux") {
        tracing::warn!("Non-Linux OS detected, forcing dry-run mode");
        args.global.dry_run = true;
    }

    if args.global.dry_run {
        tracing::info!("Running in dry-run mode: no changes will be made to Linux users or files");
    }
    let data_dir = directories::ProjectDirs::from("", "", env!("CARGO_PKG_NAME"))
        .context("Error determining project directory")?
        .data_dir()
        .to_path_buf();
    let app = octosync::Octosync::new(sync::Arc::new(args.global), &data_dir).await?;

    match args.command {
        Commands::Sync(a) => {
            app.sync(&a).await?;
        }
        Commands::Delete => {
            app.delete().await?;
        }
    }
    Ok(())
}
