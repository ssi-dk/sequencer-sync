use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};
use config::{Config, ConfigError, Destination, RemoteDestination};
use fs2::FileExt;
use log::debug;
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
    transfer_incomplete: bool,
    /// Print what would be copied instead of actually copying; disables run-log writes.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    debug!("sequencer-sync starting");
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
    debug!(
        "Canonicalizing config path: Path given from CLI: {}",
        args.config_path.display()
    );
    let config_path = canonicalize_config_path(&args.config_path)?;
    debug!("Loading config path at {}", config_path.display());
    let config = load_config(&args.config_path)?;

    validate_environment(&config, args.skip_ssh_check)?;
    check_lock_is_available(&config.flockdir, &config.lock_file_name)?;
    let _ = TransferLog::load(&config.logdir).map_err(AppError::TransferLog)?;
    transfer_log::initialize_if_absent(&config.logdir).map_err(AppError::TransferLog)?;
    eprintln!("Setup successful!");
    let cron_path = write_cron_file(&config.logdir, &config_path)?;
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
    regex_string: String,
    destination: Destination,
    exclude: Vec<String>,
    completion_file_globs: Vec<glob::Pattern>,
}

#[derive(Default)]
// We only retain the scan counts that are useful to operators when deciding
// whether a run did work or skipped incomplete directories.
struct ScanSummary {
    incomplete_skipped: u32,
    planned: u32,
}

impl ScanSummary {
    fn has_noteworthy_activity(&self) -> bool {
        self.planned > 0 || self.incomplete_skipped > 0
    }
}

// Scanning returns both the work to perform and the operator-facing summary so
// the caller can decide whether to log anything at all before starting rsync.
// I.e. we don't want to log on noop calls since then running this program via
// a cron job would flood the log, making it useless.
struct ScanResult {
    planned_transfers: Vec<(fs::DirEntry, TransferReason, TransferTarget)>,
    incomplete_messages: Vec<String>,
    summary: ScanSummary,
}

fn run_command(args: RunArgs) -> Result<(), AppError> {
    let config = load_config(&args.config_path)?;
    let mut run_log = RunLog::new(&config.logdir);

    let _lock = match acquire_run_lock(&config.flockdir, &config.lock_file_name)? {
        Some(lock) => lock,
        None => {
            if !args.dry_run {
                run_log.info("Run skipped: file lock already held");
                run_log.finish();
                if run_log.had_error() {
                    return Err(AppError::RunLogWriteFailed);
                }
            }
            return Ok(());
        }
    };

    let mut transfer_log = TransferLog::load(&config.logdir).map_err(AppError::TransferLog)?;

    let result = transfer_new_directories(
        &config.source,
        &config.categories,
        &mut transfer_log,
        &mut run_log,
        &args,
    );
    if !args.dry_run {
        run_log.finish();
    }

    if let Err(error) = result {
        if !args.dry_run {
            run_log.error(&format!("Run aborted: {error}"));
            run_log.finish();
        }
        return Err(error);
    }

    if !args.dry_run && run_log.had_error() {
        return Err(AppError::RunLogWriteFailed);
    }

    Ok(())
}

fn validate_environment(config: &Config, skip_ssh_check: bool) -> Result<(), AppError> {
    // Different destinations can share same endpoint (username, port, host).
    // Only check each once
    let mut checked_ssh_endpoints = HashSet::new();

    debug!(
        "Checking that source directory is readable: {}",
        config.source.display()
    );
    check_readable_directory(&config.source, "source")?;
    for (category_index, cat) in config.categories.iter().enumerate() {
        debug!(
            "Checking category {} destination: {}",
            category_index + 1,
            cat.destination.display()
        );
        match &cat.destination {
            Destination::Local { path } => {
                check_writable_directory(path, "category.destination.path")?;
            }
            Destination::Remote(remote) => {
                if skip_ssh_check {
                    debug!("SSH checks skipped due to command line flag.");
                } else {
                    let endpoint = (remote.user.clone(), remote.host.clone(), remote.port);
                    if checked_ssh_endpoints.insert(endpoint) {
                        check_ssh_access(remote)?;
                    }
                    check_remote_writable_directory(remote)?;
                }
            }
        }
    }
    debug!(
        "Checking writability of flockdir: {}",
        config.flockdir.display()
    );
    check_writable_directory(&config.flockdir, "flockdir")?;
    debug!(
        "Checking writability of logdir: {}",
        config.logdir.display()
    );
    check_writable_directory(&config.logdir, "logdir")?;
    Ok(())
}

#[derive(Clone, Copy)]
enum TransferReason {
    /// Directory has never been transferred before.
    New,
    /// Directory was previously transferred but failed; being retried via --retry-failed.
    Retry,
    /// Directory was manually marked for transfer by setting redo=true in the transfer log.
    Redo,
}

#[derive(Clone, Copy)]
enum SkipReason {
    // Should not be transferred: Previously failed, --retry-failed not set
    FailedNoRetry,

    // Should not be transferred: Already successfully transferred
    AlreadyTranferred,
}

enum TransferAction {
    Tranfer(TransferReason),
    Skip(SkipReason),
}

fn scan_directories(
    source: &Path,
    categories: &[config::Category],
    transfer_log: &TransferLog,
    retry_failed: bool,
    transfer_incomplete: bool,
) -> Result<ScanResult, AppError> {
    debug!("Searching for new directories in {}", source.display());

    let entries = fs::read_dir(source).map_err(|e| AppError::ReadDirectory {
        field: "source",
        path: source.to_path_buf(),
        source: e,
    })?;

    let mut planned_transfers = Vec::new();
    let mut incomplete_messages = Vec::new();
    let mut summary = ScanSummary::default();

    if log::max_level() >= log::LevelFilter::Debug {
        debug!("Checking for following regex");
        for cat in categories {
            debug!("\t{}", cat.regex.as_str());
        }
    }

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

        let action = transfer_log.transfer_action(&key, retry_failed);
        let reason: TransferReason = match action {
            TransferAction::Skip(SkipReason::AlreadyTranferred) => {
                debug!("Skipping already-transferred directory: {}", key.display());
                continue;
            }
            TransferAction::Skip(SkipReason::FailedNoRetry) => {
                debug!(
                    "Skipping directory where transfer previously failed (--retry-failed not set): {}",
                    key.display()
                );
                continue;
            }
            TransferAction::Tranfer(reason) => reason,
        };

        let dir_name = entry.file_name();
        let dir_name = dir_name.to_string_lossy();

        let target = match classify(&dir_name, categories)? {
            Some(t) => {
                debug!(
                    "Match: Run directory {} match regex {} to landing zone {}",
                    dir_name,
                    &t.regex_string,
                    &t.destination.display()
                );
                t
            }
            None => {
                debug!("No category match: {}", dir_name);
                continue;
            }
        };

        let is_complete = run_is_complete(&entry.path(), &target.completion_file_globs)?;
        if !is_complete {
            if transfer_incomplete {
                debug!(
                    "Transferring incomplete run due to --transfer-incomplete flag: {}",
                    &entry.path().display()
                );
            } else {
                summary.incomplete_skipped += 1;
                debug!(
                    "Completion file(s) not found; skipping {}",
                    &entry.path().display()
                );
                incomplete_messages.push(format!("Skipped incomplete directory {dir_name}"));
                continue;
            }
        }

        planned_transfers.push((entry, reason, target));
    }
    summary.planned = planned_transfers.len() as u32;

    Ok(ScanResult {
        planned_transfers,
        incomplete_messages,
        summary,
    })
}

fn run_is_complete(
    run_dir: &Path,
    completion_file_globs: &[glob::Pattern],
) -> Result<bool, AppError> {
    for completion_file_glob in completion_file_globs {
        let pattern = run_dir.join(completion_file_glob.as_str());
        let pattern = pattern.to_string_lossy();
        // The pattern was validated at config load time, so PatternError is not expected here.
        let mut paths = glob::glob(&pattern).expect("completion file glob pattern should be valid");
        match paths.next() {
            None => {
                return {
                    debug!(
                        "\tNot found: Completion glob {}",
                        run_dir.join(completion_file_glob.as_str()).display()
                    );
                    Ok(false)
                };
            }
            Some(Ok(_)) => (),
            Some(Err(source)) => {
                return Err(AppError::CompletionFileScan {
                    run_dir: run_dir.to_path_buf(),
                    source,
                });
            }
        }
    }

    Ok(true)
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
            cat.destination
                .with_appended_relative_path(Path::new(&format!("20{}", &dir_name[..2])))
        } else {
            cat.destination.clone()
        };
        return Ok(Some(TransferTarget {
            regex_string: cat.regex.as_str().to_owned(),
            destination,
            exclude: cat.exclude.clone(),
            completion_file_globs: cat.completion_file_globs.clone(),
        }));
    }
    Ok(None)
}

fn transfer_new_directories(
    source: &Path,
    categories: &[config::Category],
    transfer_log: &mut TransferLog,
    run_log: &mut RunLog,
    args: &RunArgs,
) -> Result<(), AppError> {
    let (mut succeeded, mut failed) = (0u32, 0u32);
    let scan = scan_directories(
        source,
        categories,
        transfer_log,
        args.retry_failed,
        args.transfer_incomplete,
    )?;

    debug!(
        "Total directories to transfer: {}",
        scan.planned_transfers.len()
    );

    if !scan.summary.has_noteworthy_activity() {
        return Ok(());
    }

    let ScanResult {
        planned_transfers,
        incomplete_messages,
        summary,
    } = scan;

    if !args.dry_run {
        run_log.info(&run_started_message(args));
        run_log.info(&scan_summary_message(&summary));
        for message in &incomplete_messages {
            run_log.info(message);
        }
        if !planned_transfers.is_empty() {
            run_log.start_latest_attempt();
        }
    }

    for (entry, reason, target) in planned_transfers {
        let dir_name = entry.file_name();
        let dir_name = dir_name.to_string_lossy();

        let transferred_dir = target
            .destination
            .with_appended_relative_path(Path::new(dir_name.as_ref()));
        let destination_display = transferred_dir.display();

        if args.dry_run {
            print_dry_run(&entry.path(), &transferred_dir, &target.exclude);
            continue;
        }

        prepare_transfer_destination(&transferred_dir)?;
        let rsync_result = rsync_directory(&entry.path(), &transferred_dir, &target.exclude);

        let key = transfer_log::relative_directory_key(source, &entry.path())
            .map_err(AppError::TransferLog)?;
        transfer_log
            .record_transfer(&key, rsync_result.is_ok())
            .map_err(AppError::TransferLog)?;

        match rsync_result {
            Ok(()) => {
                succeeded += 1;
                run_log.info(&format!(
                    "Transferred {} {dir_name} -> {destination_display}",
                    transfer_reason_label(reason)
                ));
                if let Err(error) = touch_transfer_marker(&transferred_dir) {
                    run_log.error(&format!(
                        "Warning: failed to write transfer marker: {error}"
                    ));
                }
            }
            Err(error) => {
                failed += 1;
                run_log.error(&format!(
                    "FAILED transfer {} {dir_name} -> {destination_display}: {error}",
                    transfer_reason_label(reason)
                ));
            }
        }
    }

    if !args.dry_run {
        run_log.info(&run_complete_message(succeeded, failed));
    }

    Ok(())
}

fn run_started_message(args: &RunArgs) -> String {
    format!(
        "Run started: retry_failed={} transfer_incomplete={}",
        args.retry_failed, args.transfer_incomplete
    )
}

fn scan_summary_message(summary: &ScanSummary) -> String {
    format!(
        "Scan summary: planned={} incomplete_skipped={}",
        summary.planned, summary.incomplete_skipped
    )
}

fn run_complete_message(succeeded: u32, failed: u32) -> String {
    format!("Run complete: {succeeded} transferred, {failed} failed")
}

fn transfer_reason_label(reason: TransferReason) -> &'static str {
    match reason {
        TransferReason::New => "new directory",
        TransferReason::Retry => "previously failed transfer",
        TransferReason::Redo => "directory marked for redo",
    }
}

// This marker is useful because once the data has been transferred from the
// landing zone to the remote server, someone on the server can check for this
// file to see if the transfer to the landing zone was complete.
const TRANSFER_MARKER_FILE_NAME: &str = "transfer_successful.txt";

fn touch_transfer_marker(transferred_dir: &Destination) -> Result<(), AppError> {
    match transferred_dir {
        Destination::Local { path } => {
            let marker = path.join(TRANSFER_MARKER_FILE_NAME);
            File::create(&marker).map_err(|source| AppError::WriteTransferMarker {
                path: marker.display().to_string(),
                source,
            })?;
            Ok(())
        }
        Destination::Remote(remote) => {
            build_remote_touch_marker(remote)
                .run_status()
                .map_err(|source| AppError::WriteRemoteTransferMarker {
                    destination: remote.display(),
                    source,
                })
        }
    }
}

fn print_dry_run(source: &Path, destination: &Destination, exclude: &[String]) {
    println!("{} -> {}", source.display(), destination.display());
    for pattern in exclude {
        println!("  exclude: {pattern}");
    }
}

fn rsync_directory(
    source: &Path,
    destination: &Destination,
    exclude: &[String],
) -> Result<(), AppError> {
    debug!(
        "Running rsync from {} to {} with exclude {}",
        source.display(),
        destination.display(),
        exclude.join(" ")
    );
    let cmd = build_rsync_command(source, destination, exclude);
    let output = cmd
        .run_output()
        .map_err(|source| AppError::SpawnRsync { source })?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!(
            "rsync failed: source={} destination={} exit_code={} stderr={}",
            source.display(),
            destination.display(),
            output
                .status
                .code()
                .map_or_else(|| "unknown".to_string(), |code| code.to_string()),
            stderr.trim()
        );
        Err(AppError::RsyncFailed {
            source_path: source.to_path_buf(),
            destination: destination.display(),
            exit_code: output.status.code(),
        })
    }
}

fn path_with_trailing_separator(path: &Path) -> PathBuf {
    let mut path_with_separator = path.to_path_buf();
    path_with_separator.push("");
    path_with_separator
}

fn rsync_destination(destination: &Destination) -> OsString {
    match destination {
        Destination::Local { path } => path_with_trailing_separator(path).into_os_string(),
        Destination::Remote(remote) => {
            let destination_with_separator = path_with_trailing_separator(&remote.path);
            OsString::from(format!(
                "{}@{}:{}",
                remote.user,
                remote.host,
                destination_with_separator.display()
            ))
        }
    }
}

fn load_config(config_path: &Path) -> Result<Config, AppError> {
    Config::from_path(config_path).map_err(|source| AppError::LoadConfig {
        path: config_path.to_path_buf(),
        source: Box::new(source),
    })
}

fn check_ssh_access(remote: &RemoteDestination) -> Result<(), AppError> {
    debug!(
        "Checking SSH access. User name: {} port: {} domain name: {}",
        remote.user, remote.port, remote.host
    );
    build_ssh_access_check(remote)
        .run_status()
        .map_err(|source| AppError::SshAccessDenied {
            user: remote.user.clone(),
            host: remote.host.clone(),
            port: remote.port,
            source,
        })
}

fn check_remote_writable_directory(remote: &RemoteDestination) -> Result<(), AppError> {
    build_remote_writable_probe(remote, "category.destination.path")
        .run_status()
        .map_err(|source| AppError::RemoteWriteDirectory {
            destination: remote.display(),
            source,
        })
}

// Create the target directory, which we then rsync into.
// This is necessary, because if we transfer the directory itself, the exclude patterns
// would be relative to the source directory's parent, which could work,
// but would be awkward and confusing for the user
fn prepare_transfer_destination(destination: &Destination) -> Result<(), AppError> {
    match destination {
        Destination::Local { path } => {
            fs::create_dir_all(path).map_err(|source| AppError::CreateTransferDir {
                path: path.display().to_string(),
                source,
            })
        }
        Destination::Remote(remote) => build_remote_mkdir(remote).run_status().map_err(|source| {
            AppError::CreateRemoteTransferDir {
                destination: remote.display(),
                source,
            }
        }),
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

fn write_cron_file(logdir: &Path, config_path: &Path) -> Result<PathBuf, AppError> {
    debug!("Determining cron file content");
    let binary_path = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|source| AppError::CurrentExe { source })?;
    let cron_path = cron_file_path(logdir);
    let contents = render_cron_file(config_path, &binary_path);
    debug!(
        "Writing cron content to cron file at {}",
        cron_path.display()
    );
    fs::write(&cron_path, contents).map_err(|source| AppError::WriteCronFile {
        path: cron_path.clone(),
        source,
    })?;
    Ok(cron_path)
}

fn cron_file_path(logdir: &Path) -> PathBuf {
    logdir.join("sequencer-sync.cron")
}

fn lock_file_path(flockdir: &Path, lock_file_name: &str) -> PathBuf {
    flockdir.join(lock_file_name)
}

fn render_cron_file(config_path: &Path, binary_path: &Path) -> String {
    let command = format!(
        "{} run --config-path {}",
        shell_quote(binary_path.to_string_lossy().as_ref()),
        shell_quote(config_path.to_string_lossy().as_ref()),
    );

    format!("# Install this file into cron manually.\n*/15 * * * * {command}\n")
}

fn check_lock_is_available(flockdir: &Path, lock_file_name: &str) -> Result<(), AppError> {
    let path = lock_file_path(flockdir, lock_file_name);
    debug!("Checking availability of lock file at {}", path.display());
    let _lock =
        acquire_run_lock(flockdir, lock_file_name)?.ok_or(AppError::RunLockHeld { path })?;
    Ok(())
}

fn acquire_run_lock(flockdir: &Path, lock_file_name: &str) -> Result<Option<RunLock>, AppError> {
    let path = lock_file_path(flockdir, lock_file_name);
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

/// A command ready to execute, represented as program + args for testability.
struct ExternalCommand {
    program: OsString,
    args: Vec<OsString>,
}

impl ExternalCommand {
    fn run_status(&self) -> Result<(), std::io::Error> {
        let status = Command::new(&self.program).args(&self.args).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "remote command failed with exit code {}",
                status
                    .code()
                    .map_or_else(|| "unknown".to_string(), |code| code.to_string())
            )))
        }
    }

    fn run_output(&self) -> Result<std::process::Output, std::io::Error> {
        Command::new(&self.program).args(&self.args).output()
    }
}

// ---------------------------------------------------------------------------
// Command builders — pure functions returning ExternalCommand
// ---------------------------------------------------------------------------

fn build_ssh_command(remote: &RemoteDestination, remote_command: &str) -> ExternalCommand {
    ExternalCommand {
        program: OsString::from("ssh"),
        args: vec![
            OsString::from("-o"),
            OsString::from("BatchMode=yes"),
            OsString::from("-p"),
            OsString::from(remote.port.to_string()),
            OsString::from(format!("{}@{}", remote.user, remote.host)),
            OsString::from("--"),
            OsString::from(remote_command),
        ],
    }
}

fn build_ssh_access_check(remote: &RemoteDestination) -> ExternalCommand {
    build_ssh_command(remote, "true")
}

fn build_remote_writable_probe(remote: &RemoteDestination, field: &str) -> ExternalCommand {
    let probe_path = temp_probe_path(&remote.path, field);
    build_ssh_command(
        remote,
        &format!(
            "mkdir -p -- {dir} && : > {probe} && rm -f -- {probe}",
            dir = shell_quote(remote.path.to_string_lossy().as_ref()),
            probe = shell_quote(probe_path.to_string_lossy().as_ref()),
        ),
    )
}

fn build_remote_mkdir(remote: &RemoteDestination) -> ExternalCommand {
    build_ssh_command(
        remote,
        &format!(
            "mkdir -p -- {}",
            shell_quote(remote.path.to_string_lossy().as_ref())
        ),
    )
}

fn build_remote_touch_marker(remote: &RemoteDestination) -> ExternalCommand {
    let marker = remote.path.join(TRANSFER_MARKER_FILE_NAME);
    build_ssh_command(
        remote,
        &format!(
            "touch -- {}",
            shell_quote(marker.to_string_lossy().as_ref())
        ),
    )
}

fn build_rsync_command(
    source: &Path,
    destination: &Destination,
    exclude: &[String],
) -> ExternalCommand {
    let mut args = Vec::new();
    args.push(OsString::from("-a"));
    if let Destination::Remote(remote) = destination {
        args.push(OsString::from("-e"));
        args.push(OsString::from(format!(
            "ssh -o BatchMode=yes -p {}",
            remote.port
        )));
    }
    for pattern in exclude {
        args.push(OsString::from("--exclude"));
        args.push(OsString::from(pattern));
    }
    args.push(path_with_trailing_separator(source).into_os_string());
    args.push(rsync_destination(destination));
    ExternalCommand {
        program: OsString::from("rsync"),
        args,
    }
}

// ---------------------------------------------------------------------------
// Thin wrappers: builder + execution
// ---------------------------------------------------------------------------

struct RunLock {
    file: File,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[derive(Debug, Error)]
enum AppError {
    #[error("failed to load config from {}: {source}", path.display())]
    LoadConfig {
        path: PathBuf,
        // Box in order to make AppError smaller
        #[source]
        source: Box<ConfigError>,
    },
    #[error("ssh access check failed for {user}@{host}:{port}: {source}")]
    SshAccessDenied {
        user: String,
        host: String,
        port: u16,
        #[source]
        source: std::io::Error,
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
    #[error("failed to create transfer directory {path}: {source}")]
    CreateTransferDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create remote transfer directory {destination}: {source}")]
    CreateRemoteTransferDir {
        destination: String,
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
    #[error("rsync failed copying {} to {}: exit code {}", source_path.display(), destination, exit_code.map_or("unknown".to_string(), |c| c.to_string()))]
    RsyncFailed {
        source_path: PathBuf,
        destination: String,
        exit_code: Option<i32>,
    },
    #[error("failed to scan for completion file in {}: {source}", run_dir.display())]
    CompletionFileScan {
        run_dir: PathBuf,
        #[source]
        source: glob::GlobError,
    },
    #[error("failed to write transfer marker {path}: {source}")]
    WriteTransferMarker {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write remote transfer marker at {destination}: {source}")]
    WriteRemoteTransferMarker {
        destination: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to verify remote destination writability for {destination}: {source}")]
    RemoteWriteDirectory {
        destination: String,
        #[source]
        source: std::io::Error,
    },
    #[error("one or more run log writes failed (see warnings above)")]
    RunLogWriteFailed,
    #[error("failed to determine path to current executable: {source}")]
    CurrentExe {
        #[source]
        source: std::io::Error,
    },
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
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, Once};
    use std::time::{SystemTime, UNIX_EPOCH};

    use glob::Pattern;
    use log::{LevelFilter, Log, Metadata, Record};
    use regex::Regex;

    use super::{
        build_remote_mkdir, build_remote_touch_marker, build_remote_writable_probe,
        build_rsync_command, build_ssh_access_check, classify, cron_file_path, lock_file_path,
        path_with_trailing_separator, render_cron_file, rsync_destination, run_is_complete,
        shell_quote,
    };
    use crate::config::{Category, Destination, RemoteDestination};

    struct TestLogger {
        messages: Mutex<Vec<String>>,
    }

    impl TestLogger {
        const fn new() -> Self {
            Self {
                messages: Mutex::new(Vec::new()),
            }
        }

        fn clear(&self) {
            self.messages.lock().unwrap().clear();
        }

        fn lines(&self) -> Vec<String> {
            self.messages.lock().unwrap().clone()
        }
    }

    impl Log for TestLogger {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() <= log::max_level()
        }

        fn log(&self, record: &Record<'_>) {
            if self.enabled(record.metadata()) {
                self.messages
                    .lock()
                    .unwrap()
                    .push(format!("{}", record.args()));
            }
        }

        fn flush(&self) {}
    }

    static TEST_LOGGER: TestLogger = TestLogger::new();
    static INIT_TEST_LOGGER: Once = Once::new();

    fn init_test_logger() {
        INIT_TEST_LOGGER.call_once(|| {
            log::set_logger(&TEST_LOGGER).expect("test logger should install exactly once");
            log::set_max_level(LevelFilter::Debug);
        });
        TEST_LOGGER.clear();
    }

    fn make_temp_dir() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "sequencer-sync-main-test-{}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("should create temp dir");
        path
    }

    fn cleanup_temp_dir(path: &Path) {
        fs::remove_dir_all(path).expect("should remove temp dir");
    }

    #[test]
    fn renders_cron_file() {
        let block = render_cron_file(
            Path::new("/etc/sequencer-sync/config.yaml"),
            Path::new("/usr/local/bin/sequencer-sync"),
        );

        assert!(block.contains("# Install this file into cron manually."));
        assert!(block.contains("*/15 * * * *"));
        assert!(block.contains("'/usr/local/bin/sequencer-sync' run"));
        assert!(block.contains("--config-path '/etc/sequencer-sync/config.yaml'"));
        assert!(!block.contains("--platform"));
    }

    #[test]
    fn computes_cron_file_path_in_logdir() {
        let path = cron_file_path(Path::new("/var/lib/sequencer/log"));

        assert_eq!(
            path,
            Path::new("/var/lib/sequencer/log/sequencer-sync.cron")
        );
    }

    #[test]
    fn computes_lock_file_path_in_flockdir() {
        let path = lock_file_path(Path::new("/var/lib/sequencer/flock"), "my.lock");

        assert_eq!(path, Path::new("/var/lib/sequencer/flock/my.lock"));
    }

    #[test]
    fn run_is_complete_logs_full_missing_glob_path() {
        init_test_logger();
        let tempdir = make_temp_dir();
        let glob = Pattern::new("nested/complete.txt").expect("glob should parse");

        let is_complete = run_is_complete(&tempdir, &[glob]).expect("scan should succeed");

        assert!(!is_complete);
        let expected_path = tempdir.join("nested/complete.txt").display().to_string();
        assert!(TEST_LOGGER.lines().iter().any(|line| {
            line.contains("Not found: Completion glob") && line.contains(&expected_path)
        }));

        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn classify_preserves_matching_regex_string() {
        let categories = vec![Category {
            regex: Regex::new(r"^ONT_WGS_").expect("regex should parse"),
            destination: Destination::Local {
                path: PathBuf::from("/tmp/landing"),
            },
            exclude: Vec::new(),
            completion_file_globs: vec![Pattern::new("report*.html").expect("glob should parse")],
            year_subdirectory: false,
        }];

        let target = classify("ONT_WGS_run1", &categories)
            .expect("classification should succeed")
            .expect("directory should match category");

        assert_eq!(target.regex_string, "^ONT_WGS_");
    }

    #[test]
    fn rsync_destination_formats_remote_target() {
        let remote = Destination::Remote(RemoteDestination {
            user: "alice".to_string(),
            host: "example.org".to_string(),
            port: 2222,
            path: PathBuf::from("/incoming/data"),
        });

        assert_eq!(
            rsync_destination(&remote),
            OsString::from("alice@example.org:/incoming/data/")
        );
    }

    #[test]
    fn path_with_trailing_separator_handles_both_input_shapes() {
        assert_eq!(
            path_with_trailing_separator(Path::new("/tmp/data")),
            PathBuf::from("/tmp/data/")
        );
        assert_eq!(
            path_with_trailing_separator(Path::new("/tmp/data/")),
            PathBuf::from("/tmp/data/")
        );
    }

    fn args_as_strings(cmd: &super::ExternalCommand) -> Vec<String> {
        cmd.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn test_remote() -> RemoteDestination {
        RemoteDestination {
            user: "alice".to_string(),
            host: "example.org".to_string(),
            port: 22,
            path: PathBuf::from("/incoming/data"),
        }
    }

    // --- SSH access check ---

    #[test]
    fn ssh_access_check_command() {
        let remote = test_remote();
        let cmd = build_ssh_access_check(&remote);
        assert_eq!(cmd.program, OsString::from("ssh"));
        assert_eq!(
            args_as_strings(&cmd),
            vec![
                "-o",
                "BatchMode=yes",
                "-p",
                "22",
                "alice@example.org",
                "--",
                "true"
            ]
        );
    }

    #[test]
    fn ssh_access_check_non_default_port() {
        let remote = RemoteDestination {
            port: 2222,
            ..test_remote()
        };
        let cmd = build_ssh_access_check(&remote);
        assert_eq!(args_as_strings(&cmd)[3], "2222");
    }

    #[test]
    fn ssh_access_check_passthrough_host() {
        let remote = RemoteDestination {
            user: "deploy".to_string(),
            host: "jump+internal.host".to_string(),
            ..test_remote()
        };
        let cmd = build_ssh_access_check(&remote);
        assert_eq!(args_as_strings(&cmd)[4], "deploy@jump+internal.host");
    }

    // --- Remote mkdir (prepare_transfer_destination) ---

    #[test]
    fn remote_mkdir_command() {
        let remote = test_remote();
        let cmd = build_remote_mkdir(&remote);
        let args = args_as_strings(&cmd);
        assert_eq!(args[6], "mkdir -p -- '/incoming/data'");
    }

    #[test]
    fn remote_mkdir_path_with_spaces() {
        let remote = RemoteDestination {
            path: PathBuf::from("/incoming/my data"),
            ..test_remote()
        };
        let cmd = build_remote_mkdir(&remote);
        let args = args_as_strings(&cmd);
        assert_eq!(args[6], "mkdir -p -- '/incoming/my data'");
    }

    #[test]
    fn remote_mkdir_path_with_shell_sensitive_chars() {
        let remote = RemoteDestination {
            path: PathBuf::from("/incoming/$HOME;rm -rf /"),
            ..test_remote()
        };
        let cmd = build_remote_mkdir(&remote);
        let args = args_as_strings(&cmd);
        assert_eq!(args[6], "mkdir -p -- '/incoming/$HOME;rm -rf /'");
    }

    #[test]
    fn remote_mkdir_path_with_single_quotes() {
        let remote = RemoteDestination {
            path: PathBuf::from("/incoming/it's data"),
            ..test_remote()
        };
        let cmd = build_remote_mkdir(&remote);
        let args = args_as_strings(&cmd);
        assert_eq!(args[6], r#"mkdir -p -- '/incoming/it'\''s data'"#);
    }

    // --- Remote writable probe ---

    #[test]
    fn remote_writable_probe_command_structure() {
        let remote = test_remote();
        let cmd = build_remote_writable_probe(&remote, "category.destination.path");
        let args = args_as_strings(&cmd);
        let payload = &args[6];
        assert!(payload.starts_with("mkdir -p -- '/incoming/data'"));
        assert!(payload.contains("&& : >"));
        assert!(payload.contains("&& rm -f --"));
        assert!(payload.contains(".sequencer-sync-category.destination.path-write-probe-"));
    }

    #[test]
    fn remote_writable_probe_quotes_path_with_single_quotes() {
        let remote = RemoteDestination {
            path: PathBuf::from("/incoming/it's data"),
            ..test_remote()
        };
        let cmd = build_remote_writable_probe(&remote, "category.destination.path");
        let args = args_as_strings(&cmd);
        let payload = &args[6];
        assert!(payload.starts_with(r#"mkdir -p -- '/incoming/it'\''s data'"#));
    }

    // --- Remote touch marker ---

    #[test]
    fn remote_touch_marker_command() {
        let remote = test_remote();
        let cmd = build_remote_touch_marker(&remote);
        let args = args_as_strings(&cmd);
        assert_eq!(args[6], "touch -- '/incoming/data/transfer_successful.txt'");
    }

    #[test]
    fn remote_touch_marker_path_with_spaces() {
        let remote = RemoteDestination {
            path: PathBuf::from("/incoming/run 001"),
            ..test_remote()
        };
        let cmd = build_remote_touch_marker(&remote);
        let args = args_as_strings(&cmd);
        assert_eq!(
            args[6],
            "touch -- '/incoming/run 001/transfer_successful.txt'"
        );
    }

    #[test]
    fn remote_touch_marker_path_with_single_quotes() {
        let remote = RemoteDestination {
            path: PathBuf::from("/incoming/it's a run"),
            ..test_remote()
        };
        let cmd = build_remote_touch_marker(&remote);
        let args = args_as_strings(&cmd);
        assert_eq!(
            args[6],
            r#"touch -- '/incoming/it'\''s a run/transfer_successful.txt'"#
        );
    }

    // --- Rsync command: local destination ---

    #[test]
    fn rsync_local_command() {
        let source = Path::new("/data/nanopore/run001");
        let dest = Destination::Local {
            path: PathBuf::from("/landing/run001"),
        };
        let cmd = build_rsync_command(source, &dest, &[]);
        assert_eq!(cmd.program, OsString::from("rsync"));
        assert_eq!(
            args_as_strings(&cmd),
            vec!["-a", "/data/nanopore/run001/", "/landing/run001/"]
        );
    }

    #[test]
    fn rsync_local_with_excludes() {
        let source = Path::new("/data/nanopore/run001");
        let dest = Destination::Local {
            path: PathBuf::from("/landing/run001"),
        };
        let excludes = vec!["*.tmp".to_string(), "logs/".to_string()];
        let cmd = build_rsync_command(source, &dest, &excludes);
        assert_eq!(
            args_as_strings(&cmd),
            vec![
                "-a",
                "--exclude",
                "*.tmp",
                "--exclude",
                "logs/",
                "/data/nanopore/run001/",
                "/landing/run001/"
            ]
        );
    }

    // --- Rsync command: remote destination ---

    #[test]
    fn rsync_remote_command() {
        let source = Path::new("/data/nanopore/run001");
        let dest = Destination::Remote(RemoteDestination {
            user: "alice".to_string(),
            host: "example.org".to_string(),
            port: 22,
            path: PathBuf::from("/incoming/run001"),
        });
        let cmd = build_rsync_command(source, &dest, &[]);
        assert_eq!(
            args_as_strings(&cmd),
            vec![
                "-a",
                "-e",
                "ssh -o BatchMode=yes -p 22",
                "/data/nanopore/run001/",
                "alice@example.org:/incoming/run001/"
            ]
        );
    }

    #[test]
    fn rsync_remote_non_default_port() {
        let source = Path::new("/data/run001");
        let dest = Destination::Remote(RemoteDestination {
            user: "bob".to_string(),
            host: "server.local".to_string(),
            port: 2222,
            path: PathBuf::from("/srv/data/run001"),
        });
        let cmd = build_rsync_command(source, &dest, &[]);
        let args = args_as_strings(&cmd);
        assert_eq!(args[2], "ssh -o BatchMode=yes -p 2222");
    }

    #[test]
    fn rsync_remote_with_excludes() {
        let source = Path::new("/data/run001");
        let dest = Destination::Remote(test_remote());
        let excludes = vec!["*.bam".to_string()];
        let cmd = build_rsync_command(source, &dest, &excludes);
        let args = args_as_strings(&cmd);
        assert_eq!(args[3], "--exclude");
        assert_eq!(args[4], "*.bam");
    }

    // --- Rsync with year-subdirectory derived path ---

    #[test]
    fn rsync_remote_year_subdirectory_path() {
        let source = Path::new("/data/nanopore/240101_NB123");
        let base_dest = Destination::Remote(RemoteDestination {
            user: "alice".to_string(),
            host: "example.org".to_string(),
            port: 22,
            path: PathBuf::from("/incoming"),
        });
        // Simulate what classify does with year_subdirectory=true
        let year_dest = base_dest.with_appended_relative_path(Path::new("2024"));
        let final_dest = year_dest.with_appended_relative_path(Path::new("240101_NB123"));
        let cmd = build_rsync_command(source, &final_dest, &[]);
        let args = args_as_strings(&cmd);
        assert_eq!(args[4], "alice@example.org:/incoming/2024/240101_NB123/");
    }

    // --- shell_quote ---

    #[test]
    fn shell_quote_simple_value() {
        assert_eq!(shell_quote("/tmp/data"), "'/tmp/data'");
    }

    #[test]
    fn shell_quote_value_with_spaces() {
        assert_eq!(shell_quote("/tmp/my data"), "'/tmp/my data'");
    }

    #[test]
    fn shell_quote_value_with_single_quotes() {
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
    }

    #[test]
    fn shell_quote_value_with_special_chars() {
        assert_eq!(shell_quote("$HOME;rm -rf /"), "'$HOME;rm -rf /'");
    }
}
