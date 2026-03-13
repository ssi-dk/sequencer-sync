use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use config::{Config, ConfigError};
use thiserror::Error;

mod config;

#[derive(Debug, Parser)]
#[command(name = "sequencer-sync")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Setup(CommandArgs),
    Test(CommandArgs),
    Run(CommandArgs),
}

#[derive(Args, Debug)]
struct CommandArgs {
    #[arg(long)]
    config_path: PathBuf,
    #[arg(long)]
    platform: Platform,
}

#[derive(Clone, Debug, ValueEnum)]
enum Platform {
    Nanopore,
    NextSeq,
}

fn main() -> ExitCode {
    if let Err(error) = try_main() {
        eprintln!("Error: {error}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn try_main() -> Result<(), AppError> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Setup(args) => setup(args),
        Commands::Test(args) => test(args),
        Commands::Run(args) => run_command(args),
    }
}

fn setup(args: CommandArgs) -> Result<(), AppError> {
    let _platform = args.platform;
    let config = load_config(&args.config_path)?;

    check_ssh_access(&config)?;
    check_remote_write_access(&config)?;

    // TODO later: Check that landing zone exists, and you have write permissions
    // TODO later: Check that source exists, and you have read permissions
    // TODO later: Check the flock dir exists, and you have write permissions

    Ok(())
}

fn test(args: CommandArgs) -> Result<(), AppError> {
    match args.platform {
        Platform::Nanopore => test_nanopore(&args.config_path),
        Platform::NextSeq => test_nextseq(&args.config_path),
    }
}

fn run_command(args: CommandArgs) -> Result<(), AppError> {
    match args.platform {
        Platform::Nanopore => run_nanopore(&args.config_path),
        Platform::NextSeq => run_nextseq(&args.config_path),
    }
}

fn test_nanopore(config_path: &Path) -> Result<(), AppError> {
    let _config = load_config(config_path)?;
    todo!()
}

fn test_nextseq(config_path: &Path) -> Result<(), AppError> {
    let _config = load_config(config_path)?;
    todo!()
}

fn run_nanopore(config_path: &Path) -> Result<(), AppError> {
    let _config = load_config(config_path)?;
    todo!()
}

fn run_nextseq(config_path: &Path) -> Result<(), AppError> {
    let _config = load_config(config_path)?;
    todo!()
}

fn load_config(config_path: &Path) -> Result<Config, AppError> {
    Config::from_path(config_path).map_err(|source| AppError::LoadConfig {
        path: config_path.to_path_buf(),
        source,
    })
}

fn check_ssh_access(config: &Config) -> Result<(), AppError> {
    let status = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-p")
        .arg(config.server_port.to_string())
        .arg(format!("{}@{}", config.server_user, config.server_host))
        .arg("--")
        .arg("true")
        .status()
        .map_err(|source| AppError::SpawnSsh { source })?;

    if status.success() {
        Ok(())
    } else {
        Err(AppError::SshAccessDenied {
            user: config.server_user.clone(),
            host: config.server_host.clone(),
            port: config.server_port,
        })
    }
}

fn check_remote_write_access(config: &Config) -> Result<(), AppError> {
    let status = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-p")
        .arg(config.server_port.to_string())
        .arg(format!("{}@{}", config.server_user, config.server_host))
        .arg("test")
        .arg("-w")
        .arg(&config.server_dest)
        .status()
        .map_err(|source| AppError::SpawnSsh { source })?;

    if status.success() {
        Ok(())
    } else {
        Err(AppError::RemoteWriteAccessDenied {
            path: config.server_dest.clone(),
            user: config.server_user.clone(),
            host: config.server_host.clone(),
            port: config.server_port,
        })
    }
}

#[derive(Debug, Error)]
enum AppError {
    #[error("failed to load config from {}: {source}", path.display())]
    LoadConfig {
        path: PathBuf,
        #[source]
        source: ConfigError,
    },
    #[error("failed to execute ssh: {source}")]
    SpawnSsh {
        #[source]
        source: std::io::Error,
    },
    #[error("ssh access check failed for {user}@{host}:{port}")]
    SshAccessDenied {
        user: String,
        host: String,
        port: u16,
    },
    #[error("remote write-access check failed for {} on {user}@{host}:{port}", path.display())]
    RemoteWriteAccessDenied {
        path: PathBuf,
        user: String,
        host: String,
        port: u16,
    },
}
