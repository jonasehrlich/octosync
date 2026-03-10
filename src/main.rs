#![cfg(any(target_os = "linux", target_os = "macos"))]

use anyhow::Context as _;
use clap::Parser as _;
use fs2::FileExt as _;
use std::str::FromStr;
use std::{fs, path, sync};

mod octosync;
mod public_keys;
mod store;
mod user_manager;

#[derive(Debug)]
struct ProcessLock {
    path: path::PathBuf,
    file: fs::File,
}

impl ProcessLock {
    fn acquire(data_dir: &path::Path) -> anyhow::Result<Self> {
        fs::create_dir_all(data_dir).with_context(|| {
            format!(
                "Failed to create data directory for lockfile '{}'",
                data_dir.display()
            )
        })?;

        let lock_path = data_dir.join("octosync.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("Failed to open lockfile '{}'", lock_path.display()))?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self {
                path: lock_path,
                file,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                anyhow::bail!(
                    "Another octosync process is already running (lockfile: '{}')",
                    lock_path.display()
                );
            }
            Err(err) => Err(err)
                .with_context(|| format!("Failed to acquire lockfile '{}'", lock_path.display())),
        }
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        if let Err(err) = self.file.unlock() {
            tracing::warn!(
                "Failed to unlock lockfile '{}' on exit: {}",
                self.path.display(),
                err
            );
            // If unlocking failed, keep the lockfile to avoid races with other processes.
            return;
        }
        if let Err(err) = fs::remove_file(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                "Failed to remove lockfile '{}' on exit: {}",
                self.path.display(),
                err
            );
        }
    }
}

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
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((_a, _b)) = s.split_once(':') {
            Err(anyhow::anyhow!(
                "Mapping GitHub teams to Linux groups is not implemented yet"
            ))
            // Ok(Self::MapGitHubTeam {
            //     gh_team: validate_group_name(_a)?,
            //     linux_group: validate_group_name(_b)?,
            // })
        } else {
            Ok(Self::AddGroup(validate_group_name(s)?))
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
        tracing::info!(
            "Running in dry-run mode: using mock platform user manager for user/group/key operations"
        );
    }
    let data_dir = directories::ProjectDirs::from("", "", env!("CARGO_PKG_NAME"))
        .context("Error determining project directory")?
        .data_dir()
        .to_path_buf();
    let _process_lock = ProcessLock::acquire(&data_dir)?;

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

pub fn validate_group_name(group: &str) -> anyhow::Result<String> {
    let is_valid = !group.is_empty()
        && group.len() <= 32
        && !group.starts_with("-")
        && !group.ends_with("-")
        && group
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');

    if !is_valid {
        return Err(anyhow::anyhow!(
            "Invalid  group name '{}'. Allowed characters: [A-Za-z0-9_-], max length 32.",
            group
        ));
    }
    Ok(group.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod validate_group_name {
        use super::*;

        #[test]
        fn valid_groups() {
            let groups = vec![
                "developers".to_string(),
                "team_alpha".to_string(),
                "ops-team".to_string(),
                "group123".to_string(),
            ];
            let result = groups
                .iter()
                .map(|group| validate_group_name(group))
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(result.len(), 4);
            assert_eq!(result, groups);
        }

        #[test]
        fn invalid_groups() {
            let groups = vec![
                "invalid group".to_string(),
                "toolonggroupname_exceeding_32_characters".to_string(),
                "invalid,comma".to_string(),
                "invalid$char".to_string(),
                "-foobar".to_string(),
                "foo-".to_string(),
            ];

            for group in groups {
                let result = validate_group_name(&group);
                assert!(
                    result.is_err(),
                    "Expected group '{}' to be invalid, but it was accepted",
                    group
                );
            }
        }
    }
}
