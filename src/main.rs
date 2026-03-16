use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};
use config::{Config, ConfigError};
use fs2::FileExt;
use run_log::RunLog;
use thiserror::Error;
use transfer_log::TransferLog;

mod config;
mod run_log;
mod transfer_log;

#[derive(Debug, Parser)]
#[command(name = "sequencer-sync")]
#[command(about = "Copy files from sequencing run directory to a target directory")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Validate config file, check directories have correct permissions, and print cron tab
    Setup(SetupArgs),

    /// Synchronize files to the landing zone
    Run(RunArgs),
}

#[derive(Args, Debug)]
struct SetupArgs {
    #[arg(long)]
    config_path: PathBuf,
    /// Skip the SSH access check (useful before SSH keys are deployed).
    #[arg(long, default_value_t = false)]
    skip_ssh_check: bool,
}

#[derive(Args, Debug)]
struct RunArgs {
    #[arg(long)]
    config_path: PathBuf,
    /// Retry directories whose previous transfer failed.
    #[arg(long, default_value_t = false)]
    retry_failed: bool,
    /// Transfer directories even if the completion file is not present.
    #[arg(long, default_value_t = false)]
    ignore_incomplete: bool,
    /// Print what would be copied instead of actually copying.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
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
        Commands::Run(args) => run_command(args),
    }
}

fn setup(args: SetupArgs) -> Result<(), AppError> {
    let config_path = canonicalize_config_path(&args.config_path)?;
    let config = load_config(&args.config_path)?;

    validate_environment(&config, args.skip_ssh_check)?;
    check_lock_is_available(&config.flockdir)?;
    eprintln!("Setup successful!");
    let cron_path = write_cron_file(&config.flockdir, &config_path)?;
    eprintln!(
        "Install the generated cron job with your system cron configuration: {}",
        cron_path.display()
    );

    Ok(())
}

// We might want to have distinct transfer targets e.g. from operational runs
// versus projects. Perhaps these need different file subsets.
// A TransferTarget stores information about source and destination once we have
// classified a run as e.g. project/operations/something else.
struct TransferTarget {
    destination: PathBuf,
    exclude: Vec<String>,
    completion_file_glob: glob::Pattern,
}

fn run_command(args: RunArgs) -> Result<(), AppError> {
    let config = load_config(&args.config_path)?;

    let _lock = match acquire_run_lock(&config.flockdir)? {
        Some(lock) => lock,
        None => {
            eprintln!("Another sequencer-sync run is already in progress; exiting.");
            return Ok(());
        }
    };
    let mut transfer_log = TransferLog::load(&config.flockdir).map_err(AppError::TransferLog)?;
    let mut run_log = RunLog::new(&config.flockdir);

    transfer_new_directories(
        &config.source,
        &config.categories,
        &mut transfer_log,
        &mut run_log,
        args.retry_failed,
        args.ignore_incomplete,
        args.dry_run,
    )?;

    if run_log.had_error() {
        return Err(AppError::RunLogWriteFailed);
    }

    Ok(())
}

fn validate_environment(config: &Config, skip_ssh_check: bool) -> Result<(), AppError> {
    if !skip_ssh_check {
        check_ssh_access(config)?;
    }
    check_readable_directory(&config.source, "source")?;
    for cat in &config.categories {
        check_writable_directory(&cat.landing_zone, "category.landing_zone")?;
    }
    check_writable_directory(&config.flockdir, "flockdir")?;
    Ok(())
}

enum TransferReason {
    /// Directory has never been transferred before.
    New,
    /// Directory was previously transferred but failed; being retried via --retry-failed.
    Retry,
}

fn new_directories(
    source: &Path,
    transfer_log: &TransferLog,
    retry_failed: bool,
) -> Result<Vec<(fs::DirEntry, TransferReason)>, AppError> {
    let entries = fs::read_dir(source).map_err(|e| AppError::ReadDirectory {
        field: "source",
        path: source.to_path_buf(),
        source: e,
    })?;

    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| AppError::ReadDirectory {
            field: "source",
            path: source.to_path_buf(),
            source: e,
        })?;

        let file_type = entry.file_type().map_err(|e| AppError::ReadMetadata {
            field: "source",
            path: entry.path(),
            source: e,
        })?;
        if !file_type.is_dir() {
            continue;
        }

        let key = transfer_log::relative_directory_key(source, &entry.path())
            .map_err(AppError::TransferLog)?;
        if transfer_log.should_skip(&key, retry_failed) {
            continue;
        }

        let reason = if transfer_log.previously_failed(&key) {
            TransferReason::Retry
        } else {
            TransferReason::New
        };

        result.push((entry, reason));
    }

    Ok(result)
}

fn run_is_complete(run_dir: &Path, completion_file_glob: &glob::Pattern) -> bool {
    let pattern = run_dir.join(completion_file_glob.as_str());
    let pattern = pattern.to_string_lossy();
    glob::glob(&pattern)
        .map(|mut paths| paths.next().is_some())
        .unwrap_or(false)
}

fn classify(
    dir_name: &str,
    categories: &[config::Category],
) -> Result<Option<TransferTarget>, AppError> {
    for cat in categories {
        if !cat.regex.is_match(dir_name) {
            continue;
        }
        let destination = if cat.year_subdirectory {
            let bytes = dir_name.as_bytes();
            if bytes.len() < 2 || !bytes[0].is_ascii_digit() || !bytes[1].is_ascii_digit() {
                return Err(AppError::YearSubdirectoryInvalidName {
                    dir_name: dir_name.to_string(),
                    category_regex: cat.regex.to_string(),
                });
            }
            cat.landing_zone.join(format!("20{}", &dir_name[..2]))
        } else {
            cat.landing_zone.clone()
        };
        return Ok(Some(TransferTarget {
            destination,
            exclude: cat.exclude.clone(),
            completion_file_glob: cat.completion_file_glob.clone(),
        }));
    }
    Ok(None)
}

fn transfer_new_directories(
    source: &Path,
    categories: &[config::Category],
    transfer_log: &mut TransferLog,
    run_log: &mut RunLog,
    retry_failed: bool,
    ignore_incomplete: bool,
    dry_run: bool,
) -> Result<(), AppError> {
    for (entry, reason) in new_directories(source, transfer_log, retry_failed)? {
        let dir_name = entry.file_name();
        let dir_name = dir_name.to_string_lossy();

        let target = match classify(&dir_name, categories)? {
            Some(t) => t,
            None => continue,
        };

        if !ignore_incomplete && !run_is_complete(&entry.path(), &target.completion_file_glob) {
            continue;
        }

        if dry_run {
            print_dry_run(&entry.path(), &target.destination, &target.exclude);
            continue;
        }

        if matches!(reason, TransferReason::Retry) {
            run_log.log(&format!("Retrying previously failed transfer: {dir_name}"));
        }

        let destination_display = target.destination.display();
        let succeeded = rsync_directory(&entry.path(), &target.destination, &target.exclude);
        if let Err(ref error) = succeeded {
            eprintln!("{error}");
        }
        let succeeded = succeeded.is_ok();

        let key = transfer_log::relative_directory_key(source, &entry.path())
            .map_err(AppError::TransferLog)?;
        transfer_log
            .record_transfer(&key, succeeded)
            .map_err(AppError::TransferLog)?;

        if succeeded {
            run_log.log(&format!("Transferred {dir_name} -> {destination_display}"));
            if let Err(error) = touch_transfer_marker(&target.destination.join(entry.file_name())) {
                eprintln!("Warning: {error}");
                run_log.log(&format!("Warning: {error}"));
                run_log.record_error();
            }
        } else {
            run_log.log(&format!(
                "FAILED transfer {dir_name} -> {destination_display}"
            ));
        }
    }

    Ok(())
}

// This marker is useful because once the data has been transferred from the
// landing zone to the remote server, someone on the server can check for this
// file to see if the transfer to the landing zone was complete.
const TRANSFER_MARKER_FILE_NAME: &str = "transfer_successful.txt";

fn touch_transfer_marker(transferred_dir: &Path) -> Result<(), AppError> {
    let marker = transferred_dir.join(TRANSFER_MARKER_FILE_NAME);
    File::create(&marker).map_err(|source| AppError::WriteTransferMarker {
        path: marker,
        source,
    })?;
    Ok(())
}

fn print_dry_run(source: &Path, destination: &Path, exclude: &[String]) {
    println!("{} -> {}", source.display(), destination.display());
    for pattern in exclude {
        println!("  exclude: {pattern}");
    }
}

fn rsync_directory(source: &Path, destination: &Path, exclude: &[String]) -> Result<(), AppError> {
    let mut cmd = Command::new("rsync");
    cmd.arg("-a");
    for pattern in exclude {
        cmd.arg("--exclude").arg(pattern);
    }
    let status = cmd
        .arg(source)
        .arg(destination)
        .status()
        .map_err(|source| AppError::SpawnRsync { source })?;

    if status.success() {
        Ok(())
    } else {
        Err(AppError::RsyncFailed {
            source_path: source.to_path_buf(),
            destination: destination.to_path_buf(),
            exit_code: status.code(),
        })
    }
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

fn write_cron_file(flockdir: &Path, config_path: &Path) -> Result<PathBuf, AppError> {
    let cron_path = cron_file_path(flockdir);
    let contents = render_cron_file(config_path);
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

fn render_cron_file(config_path: &Path) -> String {
    let command = format!(
        "sequencer-sync run --config-path {}",
        shell_quote(config_path.to_string_lossy().as_ref()),
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

// Ensure only one instance of this program runs at a time
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
    #[error(transparent)]
    TransferLog(#[from] transfer_log::TransferLogError),
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
    #[error("failed to execute rsync: {source}")]
    SpawnRsync {
        #[source]
        source: std::io::Error,
    },
    #[error("rsync failed copying {} to {}: exit code {}", source_path.display(), destination.display(), exit_code.map_or("unknown".to_string(), |c| c.to_string()))]
    RsyncFailed {
        source_path: PathBuf,
        destination: PathBuf,
        exit_code: Option<i32>,
    },
    #[error("failed to write transfer marker {}: {source}", path.display())]
    WriteTransferMarker {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("one or more run log writes failed (see warnings above)")]
    RunLogWriteFailed,
    #[error(
        "directory `{dir_name}` matched category regex `{category_regex}` with year_subdirectory enabled, but name does not start with two ASCII digits"
    )]
    YearSubdirectoryInvalidName {
        dir_name: String,
        category_regex: String,
    },
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{LOCK_FILE_NAME, cron_file_path, lock_file_path, render_cron_file};

    #[test]
    fn renders_cron_file() {
        let block = render_cron_file(Path::new("/etc/sequencer-sync/config.yaml"));

        assert!(block.contains("# Install this file into cron manually."));
        assert!(block.contains(
            "*/15 * * * * sequencer-sync run --config-path '/etc/sequencer-sync/config.yaml'"
        ));
        assert!(!block.contains("--platform"));
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
