use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand, ValueEnum};
use config::{Config, ConfigError};
use fs2::FileExt;
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
    let config_path = canonicalize_config_path(&args.config_path)?;
    let config = load_config(&args.config_path)?;

    check_ssh_access(&config)?;
    check_remote_write_access(&config)?;
    check_readable_directory(&config.source, "source")?;
    check_writable_directory(&config.landing_zone, "landing_zone")?;
    check_writable_directory(&config.flockdir, "flockdir")?;
    check_lock_is_available(&config.flockdir)?;
    let cron_path = write_cron_file(&config.flockdir, &config_path, args.platform)?;
    eprintln!(
        "Install the generated cron job with your system cron configuration: {}",
        cron_path.display()
    );

    Ok(())
}

fn test(args: CommandArgs) -> Result<(), AppError> {
    match args.platform {
        Platform::Nanopore => test_nanopore(&args.config_path),
        Platform::NextSeq => test_nextseq(&args.config_path),
    }
}

fn run_command(args: CommandArgs) -> Result<(), AppError> {
    let config = load_config(&args.config_path)?;
    let _lock = match acquire_run_lock(&config.flockdir)? {
        Some(lock) => lock,
        None => {
            eprintln!("Another sequencer-sync run is already in progress; exiting.");
            return Ok(());
        }
    };

    match args.platform {
        Platform::Nanopore => run_nanopore(&config),
        Platform::NextSeq => run_nextseq(&config),
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

fn run_nanopore(_config: &Config) -> Result<(), AppError> {
    todo!()
}

fn run_nextseq(_config: &Config) -> Result<(), AppError> {
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

fn canonicalize_config_path(path: &Path) -> Result<PathBuf, AppError> {
    fs::canonicalize(path).map_err(|source| AppError::CanonicalizeConfigPath {
        path: path.to_path_buf(),
        source,
    })
}

fn write_cron_file(
    flockdir: &Path,
    config_path: &Path,
    platform: Platform,
) -> Result<PathBuf, AppError> {
    let cron_path = cron_file_path(flockdir);
    let contents = render_cron_file(config_path, platform);
    fs::write(&cron_path, contents).map_err(|source| AppError::WriteCronFile {
        path: cron_path.clone(),
        source,
    })?;
    Ok(cron_path)
}

fn cron_file_path(flockdir: &Path) -> PathBuf {
    flockdir.join("sequencer-sync.cron")
}

fn lock_file_path(flockdir: &Path) -> PathBuf {
    flockdir.join(LOCK_FILE_NAME)
}

fn render_cron_file(config_path: &Path, platform: Platform) -> String {
    let command = format!(
        "sequencer-sync run --config-path {} --platform {}",
        shell_quote(config_path.to_string_lossy().as_ref()),
        platform.as_cli_value()
    );

    format!("# Install this file into cron manually.\n*/15 * * * * {command}\n")
}

fn check_lock_is_available(flockdir: &Path) -> Result<(), AppError> {
    let _lock = acquire_run_lock(flockdir)?.ok_or_else(|| AppError::RunLockHeld {
        path: lock_file_path(flockdir),
    })?;
    Ok(())
}

fn acquire_run_lock(flockdir: &Path) -> Result<Option<RunLock>, AppError> {
    let path = lock_file_path(flockdir);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| AppError::OpenRunLockFile {
            path: path.clone(),
            source,
        })?;

    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(RunLock { file })),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(source) => Err(AppError::AcquireRunLock { path, source }),
    }
}

fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', r#"'\''"#);
    format!("'{escaped}'")
}

impl Platform {
    fn as_cli_value(&self) -> &'static str {
        match self {
            Self::Nanopore => "nanopore",
            Self::NextSeq => "next-seq",
        }
    }
}

const LOCK_FILE_NAME: &str = "sequencer-sync.lock";

struct RunLock {
    file: File,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
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
    #[error("failed to resolve config path {}: {source}", path.display())]
    CanonicalizeConfigPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write cron file {}: {source}", path.display())]
    WriteCronFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to open run lock file {}: {source}", path.display())]
    OpenRunLockFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to acquire run lock {}: {source}", path.display())]
    AcquireRunLock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("run lock is currently held: {}", path.display())]
    RunLockHeld { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{LOCK_FILE_NAME, Platform, cron_file_path, lock_file_path, render_cron_file};

    #[test]
    fn renders_cron_file() {
        let block = render_cron_file(
            Path::new("/etc/sequencer-sync/config.toml"),
            Platform::Nanopore,
        );

        assert!(block.contains("# Install this file into cron manually."));
        assert!(block.contains("*/15 * * * * sequencer-sync run --config-path '/etc/sequencer-sync/config.toml' --platform nanopore"));
    }

    #[test]
    fn computes_cron_file_path_in_flockdir() {
        let path = cron_file_path(Path::new("/var/lib/sequencer/flock"));

        assert_eq!(
            path,
            Path::new("/var/lib/sequencer/flock/sequencer-sync.cron")
        );
    }

    #[test]
    fn computes_lock_file_path_in_flockdir() {
        let path = lock_file_path(Path::new("/var/lib/sequencer/flock"));

        assert_eq!(
            path,
            Path::new("/var/lib/sequencer/flock").join(LOCK_FILE_NAME)
        );
    }
}
