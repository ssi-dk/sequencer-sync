use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

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
    check_readable_directory(&config.source, "source")?;
    check_writable_directory(&config.landing_zone, "landing_zone")?;
    check_writable_directory(&config.flockdir, "flockdir")?;

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

fn check_readable_directory(path: &Path, field: &'static str) -> Result<(), AppError> {
    ensure_directory_exists(path, field)?;
    fs::read_dir(path).map_err(|source| AppError::ReadDirectory {
        field,
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn check_writable_directory(path: &Path, field: &'static str) -> Result<(), AppError> {
    ensure_directory_exists(path, field)?;

    let temp_path = temp_probe_path(path, field);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|source| AppError::WriteDirectory {
            field,
            path: path.to_path_buf(),
            source,
        })?;

    fs::remove_file(&temp_path).map_err(|source| AppError::CleanupProbeFile {
        path: temp_path,
        source,
    })?;

    Ok(())
}

fn ensure_directory_exists(path: &Path, field: &'static str) -> Result<(), AppError> {
    let metadata = fs::metadata(path).map_err(|source| AppError::ReadMetadata {
        field,
        path: path.to_path_buf(),
        source,
    })?;

    if metadata.is_dir() {
        Ok(())
    } else {
        Err(AppError::NotADirectory {
            field,
            path: path.to_path_buf(),
        })
    }
}

fn temp_probe_path(directory: &Path, field: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    let filename = format!(
        ".sequencer-sync-{field}-write-probe-{}-{timestamp}",
        std::process::id()
    );

    directory.join(filename)
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
    #[error("failed to access metadata for `{field}` directory {}: {source}", path.display())]
    ReadMetadata {
        field: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config field `{field}` must point to a directory: {}", path.display())]
    NotADirectory { field: &'static str, path: PathBuf },
    #[error("failed to read `{field}` directory {}: {source}", path.display())]
    ReadDirectory {
        field: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write to `{field}` directory {}: {source}", path.display())]
    WriteDirectory {
        field: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove temporary probe file {}: {source}", path.display())]
    CleanupProbeFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
