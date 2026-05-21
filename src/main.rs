use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};
use config::{Config, ConfigError};
use flate2::Compression;
use flate2::write::GzEncoder;
use fs2::FileExt;
use log::{debug, warn};
use run_log::RunLog;
use thiserror::Error;
use transfer_log::TransferLog;
use walkdir::WalkDir;

mod config;
mod run_log;
mod transfer_log;

fn long_version() -> &'static str {
    let mut s = env!("CARGO_PKG_VERSION").to_owned();

    match (
        option_env!("VERGEN_GIT_SHA"),
        option_env!("VERGEN_GIT_COMMIT_DATE"),
    ) {
        (Some(sha), Some(date)) => {
            s.push_str(" commit ");
            s.push_str(sha);
            s.push_str(" at ");
            s.push_str(date);

            if option_env!("VERGEN_GIT_DIRTY").is_some_and(|s| s == "true") {
                s.push_str(" (dirty git repository)");
            }
        }
        _ => s.push_str(" (commit not available at build time)"),
    }
    // Leak because clap demands a &'static str. This occurs once in program for a small string,
    // so the memory usage doesn't matter.
    Box::leak(s.into_boxed_str())
}

#[derive(Debug, Parser)]
#[command(
    name = "sequencer-sync",
    version,
    long_version = long_version(),
    about = "Copy files from sequencing run directory to a target directory"
)]
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
    /// Source-like directory whose child run directories are checked for tree classification conflicts.
    #[arg(long)]
    tree_check_source: Option<PathBuf>,
    /// Skip tree classification conflict checks during setup.
    #[arg(long, default_value_t = false)]
    skip_tree_check: bool,
}

impl SetupArgs {
    fn validate(self) -> Result<ValidatedSetupArgs, AppError> {
        let tree_check = match (self.tree_check_source, self.skip_tree_check) {
            (Some(_), true) => return Err(AppError::ConflictingTreeCheckArgs),
            (Some(p), false) => TreeCheck::Source(p),
            (None, false) => return Err(AppError::MissingTreeCheckArg),
            (None, true) => TreeCheck::Skipped,
        };
        debug!(
            "Canonicalizing config path: Path given from CLI: {}",
            self.config_path.display()
        );
        let config_path = fs::canonicalize(&self.config_path).map_err(|source| {
            AppError::CanonicalizeConfigPath {
                path: self.config_path.to_path_buf(),
                source,
            }
        })?;
        Ok(ValidatedSetupArgs {
            config_path,
            skip_ssh_check: self.skip_ssh_check,
            tree_check,
        })
    }
}

// Config file may specify filestructures which do not match actual sequencing run
// folder structure. Setup subcommand can check existing run folders in some source folder
// against the specified filestructure. This enum stores whether setup should do that
enum TreeCheck {
    Source(PathBuf),
    Skipped,
}

struct ValidatedSetupArgs {
    // Canonicalized, verified to exist
    config_path: PathBuf,
    skip_ssh_check: bool,
    tree_check: TreeCheck,
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
    /// Gzip-compress archive.tar as archive.tar.gz.
    #[arg(long, default_value_t = false)]
    compress: bool,
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

fn setup(raw_args: SetupArgs) -> Result<(), AppError> {
    let args = raw_args.validate()?;
    debug!("Loading config path at {}", args.config_path.display());
    let config = load_config(&args.config_path)?;
    debug!(
        "Loaded {} configured filestructure(s)",
        config.filestructures.len()
    );

    validate_environment(&config, args.skip_ssh_check)?;
    match args.tree_check {
        TreeCheck::Source(p) => check_run_trees(&p, &config.categories)?,
        TreeCheck::Skipped => {}
    }
    check_lock_is_available(&config.lock_file)?;
    let _ = TransferLog::load(&config.logdir).map_err(AppError::TransferLog)?;
    transfer_log::initialize_if_absent(&config.logdir).map_err(AppError::TransferLog)?;
    eprintln!("Setup successful!");
    let cron_path = write_cron_file(&config.logdir, &args.config_path)?;
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
    // Zero-based index of which category
    category_index: usize,
    destination: PathBuf,
    filestructure: Arc<config::FileStructure>,
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
// the caller can decide whether to log anything at all before starting transfer.
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

    let _lock = match acquire_run_lock(&config.lock_file)? {
        Some(lock) => lock,
        None => {
            run_log.info("Run skipped: file lock already held");
            run_log.finish();
            if run_log.had_error() {
                return Err(AppError::RunLogWriteFailed);
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
    if !skip_ssh_check {
        check_ssh_access(config)?;
    } else {
        debug!("SSH check skipped due to command line flag.")
    }
    debug!(
        "Checking that source directory is readable: {}",
        config.source.display()
    );
    check_readable_directory(&config.source, "source")?;
    for (category_index, cat) in config.categories.iter().enumerate() {
        debug!(
            "Checking writability of category {} landing zone: {}",
            category_index + 1,
            cat.landing_zone.display()
        );
        check_writable_directory(&cat.landing_zone, "category.landing_zone")?;
    }
    debug!(
        "Checking writability of lock file parent directory: {}",
        lock_file_parent(&config.lock_file)?.display()
    );
    check_writable_directory(lock_file_parent(&config.lock_file)?, "lock_file parent")?;
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
        for (cat_index, cat) in categories.iter().enumerate() {
            debug!("\t{}: {}", cat_index + 1, cat.regex.as_str());
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
        let dir_name = entry.file_name();

        debug!("Classifying {}", entry.path().display());

        let Some(dir_name) = dir_name.to_str() else {
            warn!("\tSkipping directory with non-UTF-8 name; glob matching requires UTF-8");
            continue;
        };

        let key = transfer_log::relative_directory_key(source, &entry.path())
            .map_err(AppError::TransferLog)?;

        let action = transfer_log.transfer_action(&key, retry_failed);
        let reason: TransferReason = match action {
            TransferAction::Skip(SkipReason::AlreadyTranferred) => {
                debug!("\tDirectory already transferred; skipping");
                continue;
            }
            TransferAction::Skip(SkipReason::FailedNoRetry) => {
                debug!("\tTransfer previously failed; skipping (--retry-failed not set)");
                continue;
            }
            TransferAction::Tranfer(reason) => reason,
        };

        let target = match classify(&entry.path(), categories)? {
            Some(t) => {
                debug!(
                    "\tClassified: Category {} with filestructure {} and landing zone {}",
                    &t.category_index + 1,
                    &t.filestructure.name,
                    &t.destination.display()
                );
                t
            }
            None => {
                debug!("\tNo classification");
                continue;
            }
        };

        let is_complete =
            run_is_complete(&entry.path(), &target.filestructure.completion_file_globs)?;
        if !is_complete {
            if transfer_incomplete {
                debug!("\tTransferring incomplete run due to --transfer-incomplete flag");
            } else {
                summary.incomplete_skipped += 1;
                debug!("\tCompletion file(s) not found; skipping");
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
        if !glob_has_match(run_dir, completion_file_glob).map_err(|source| {
            AppError::CompletionFileScan {
                run_dir: run_dir.to_path_buf(),
                source,
            }
        })? {
            debug!(
                "\tNot found: Completion glob {}",
                run_dir.join(completion_file_glob.as_str()).display()
            );
            return Ok(false);
        }
    }

    Ok(true)
}

fn glob_has_match(run_dir: &Path, pattern: &glob::Pattern) -> Result<bool, glob::GlobError> {
    let pattern = run_dir.join(pattern.as_str());
    let pattern = pattern
        .to_str()
        .expect("run directory and config glob should both be valid UTF-8");
    let mut paths = glob::glob(pattern).expect("glob pattern should be valid");
    match paths.next() {
        Some(Ok(_)) => Ok(true),
        Some(Err(source)) => Err(source),
        None => Ok(false),
    }
}

fn classify(
    run_dir: &Path,
    categories: &[config::Category],
) -> Result<Option<TransferTarget>, AppError> {
    let dir_name = run_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("Unreachable: scan_directories should skip non-UTF-8 run directory names");

    for (category_index, cat) in categories.iter().enumerate() {
        if !cat.regex.is_match(dir_name) {
            continue;
        } else {
            debug!("\tRegex match to category {}", category_index + 1,)
        }

        if let Some(classification_glob) = &cat.classification_glob {
            let full_path = run_dir.join(classification_glob.as_str());
            let class_glob_display = full_path.display();
            if !glob_has_match(run_dir, classification_glob).map_err(|source| {
                AppError::ClassificationFileScan {
                    run_dir: run_dir.to_path_buf(),
                    source,
                }
            })? {
                debug!("\tClassification glob did not match: {class_glob_display}",);
                continue;
            } else {
                debug!("\tMatched classification glob: {class_glob_display}")
            }
        }

        let destination = if cat.year_subdirectory {
            let bytes = dir_name.as_bytes();
            if bytes.len() < 2 || !bytes[0].is_ascii_digit() || !bytes[1].is_ascii_digit() {
                return Err(AppError::YearSubdirectoryInvalidName {
                    dir_name: dir_name.to_owned(),
                    category_regex: cat.regex.to_string(),
                });
            }
            cat.landing_zone
                .join(format!("20{}{}", bytes[0] as char, bytes[1] as char))
        } else {
            cat.landing_zone.clone()
        };
        return Ok(Some(TransferTarget {
            category_index,
            destination,
            filestructure: cat.filestructure.clone(),
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
        let dir_name_display = dir_name
            .to_str()
            .expect("scan_directories should skip non-UTF-8 run directory names");

        let transferred_dir = target.destination.join(entry.file_name());

        if landing_zone_marker_exists(&transferred_dir)? {
            let message = format!(
                "Skipped {dir_name_display} because transfer marker is unexpectedly already present in landing zone: {}",
                transferred_dir.display()
            );
            warn!("{message}");
            if !args.dry_run {
                run_log.info(&message);
            }
            continue;
        }

        if args.dry_run {
            print_dry_run(&entry.path(), &transferred_dir, &target.filestructure);
            continue;
        }

        let destination_display = transferred_dir.display();
        let transfer_result = transfer_run_to_landing_zone(
            &entry.path(),
            &transferred_dir,
            &target.filestructure,
            args.compress,
        );

        let key = transfer_log::relative_directory_key(source, &entry.path())
            .map_err(AppError::TransferLog)?;
        transfer_log
            .record_transfer(&key, transfer_result.is_ok())
            .map_err(AppError::TransferLog)?;

        match transfer_result {
            Ok(()) => {
                succeeded += 1;
                run_log.info(&format!(
                    "Transferred {} {dir_name_display} -> {destination_display}",
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
                    "FAILED transfer {} {dir_name_display} -> {destination_display}: {error}",
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
        "Run started: retry_failed={} transfer_incomplete={} compress={}",
        args.retry_failed, args.transfer_incomplete, args.compress
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
const ARCHIVE_DIR_NAME: &str = "sequencer-sync-archive";

fn touch_transfer_marker(transferred_dir: &Path) -> Result<(), AppError> {
    let marker = transferred_dir.join(TRANSFER_MARKER_FILE_NAME);
    File::create(&marker).map_err(|source| AppError::WriteTransferMarker {
        path: marker,
        source,
    })?;
    Ok(())
}

fn landing_zone_marker_exists(transferred_dir: &Path) -> Result<bool, AppError> {
    let marker = transferred_dir.join(TRANSFER_MARKER_FILE_NAME);
    match fs::metadata(&marker) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(AppError::ReadTransferMarker {
            path: marker,
            source,
        }),
    }
}

fn print_dry_run(source: &Path, destination: &Path, filestructure: &config::FileStructure) {
    println!("{} -> {}", source.display(), destination.display());
    for path in &filestructure.ignore_paths {
        println!("  ignore: {}", path.display());
    }
    for pattern in &filestructure.ignore_globs {
        println!("  ignore: {}", pattern.as_str());
    }
    for path in &filestructure.checkout_paths {
        println!("  checkout: {}", path.display());
    }
    for pattern in &filestructure.checkout_globs {
        println!("  checkout: {}", pattern.as_str());
    }
}

#[derive(Default)]
struct ClassifiedFiles {
    ignored: Vec<PathBuf>,
    archived: Vec<PathBuf>,
    checkout: Vec<PathBuf>,
}

fn transfer_run_to_landing_zone(
    source_run_dir: &Path,
    target_run_dir: &Path,
    filestructure: &config::FileStructure,
    compress: bool,
) -> Result<(), AppError> {
    let classified_files = classify_run_files(source_run_dir, filestructure)?;
    ensure_no_archive_dir_checkout_conflict(source_run_dir, &classified_files.checkout)?;

    // create_dir_all because transferred_dir may not exist
    fs::create_dir_all(target_run_dir).map_err(|source| AppError::CreateTransferDir {
        path: target_run_dir.to_path_buf(),
        source,
    })?;
    debug!(
        "Classified {} file(s) for checkout, {} file(s) for archive, and ignored {} file(s)",
        classified_files.checkout.len(),
        classified_files.archived.len(),
        classified_files.ignored.len()
    );

    // Create parent directories once, so we can create files without worrying
    // about whether their parent exists
    create_parent_directories(target_run_dir, &classified_files.checkout)?;

    for relative_path in &classified_files.checkout {
        copy_classified_file(source_run_dir, relative_path, target_run_dir)?;
    }

    // Create the tarball only if there are files to be archived.
    if !classified_files.archived.is_empty() {
        let archive_dir = target_run_dir.join(ARCHIVE_DIR_NAME);
        fs::create_dir(&archive_dir).map_err(|source| AppError::CreateTransferDir {
            path: archive_dir.clone(),
            source,
        })?;
        create_parent_directories(&archive_dir, &classified_files.archived)?;
        for relative_path in &classified_files.archived {
            copy_classified_file(source_run_dir, relative_path, &archive_dir)?;
        }

        let archive_path = if compress {
            target_run_dir.join("archive.tar.gz")
        } else {
            target_run_dir.join("archive.tar")
        };
        create_archive_tar(&archive_dir, &archive_path, compress)?;
        fs::remove_dir_all(&archive_dir).map_err(|source| AppError::RemoveArchiveDir {
            path: archive_dir,
            source,
        })?;
    }

    Ok(())
}

fn ensure_no_archive_dir_checkout_conflict(
    run_dir: &Path,
    checkout_paths: &[PathBuf],
) -> Result<(), AppError> {
    for relative_path in checkout_paths {
        if relative_path
            .components()
            .next()
            .is_some_and(|component| archive_dir_name_conflicts(component.as_os_str()))
        {
            return Err(AppError::ArchiveDirCheckoutConflict {
                run_dir: run_dir.to_path_buf(),
                relative_path: relative_path.clone(),
                archive_dir_name: ARCHIVE_DIR_NAME,
            });
        }
    }

    Ok(())
}

// Check if an existing relative path in the run_dir could conflict with the creation of
// the archive.
// This conservatively ignores extensions, such that we can switch to another compression
// algorithm (with a new extension) in the future and not reject new paths.
fn archive_dir_name_conflicts(name: &OsStr) -> bool {
    // Fast path: If the path doesn't start with the archive bytes, it can't
    // be a match.
    if !name
        .as_encoded_bytes()
        .starts_with(ARCHIVE_DIR_NAME.as_bytes())
    {
        return false;
    }

    // More complex implementation which detects cases like "{ARCHIVE_DIR_NAME}.tar.gz"
    // but rejects cases like "{ARCHIVE_DIR_NAME}_foo".
    // This is difficult to do efficiently because of variable byte-level encoding
    // of paths on different platforms.
    let archive_dir_name = OsStr::new(ARCHIVE_DIR_NAME);
    let mut candidate = Path::new(name);

    loop {
        if candidate.as_os_str() == archive_dir_name {
            return true;
        }

        let Some(stem) = candidate.file_stem() else {
            return false;
        };
        if stem == candidate.as_os_str() {
            return false;
        }
        candidate = Path::new(stem);
    }
}

fn create_parent_directories(
    target_root: &Path,
    relative_paths: &[PathBuf],
) -> Result<(), AppError> {
    let mut directories = HashSet::new();

    for relative_path in relative_paths {
        // Check whether the relative file has a directory
        // (the root already exists)
        let Some(parent) = relative_path.parent() else {
            continue;
        };
        if parent.as_os_str().is_empty() {
            continue;
        }
        directories.insert(target_root.join(parent));
    }

    for directory in directories {
        fs::create_dir_all(&directory).map_err(|source| AppError::CreateTransferDir {
            path: directory,
            source,
        })?;
    }

    Ok(())
}

fn copy_classified_file(
    run_dir: &Path, // source root dir
    relative_path: &Path,
    target_root: &Path,
) -> Result<(), AppError> {
    let source_path = run_dir.join(relative_path);
    let destination = target_root.join(relative_path);
    fs::copy(&source_path, &destination).map_err(|source| AppError::CopyTransferFile {
        source_path,
        destination,
        source,
    })?;
    Ok(())
}

fn create_archive_tar(
    archive_dir: &Path,
    archive_path: &Path,
    compress: bool,
) -> Result<(), AppError> {
    let file = File::create(archive_path).map_err(|source| AppError::CreateArchiveTar {
        path: archive_path.to_path_buf(),
        source,
    })?;
    if compress {
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        write_archive_tar(archive_dir, archive_path, &mut builder)?;
        let encoder = builder
            .into_inner()
            .map_err(|source| AppError::WriteArchiveTar {
                archive_dir: archive_dir.to_path_buf(),
                archive_tar: archive_path.to_path_buf(),
                source,
            })?;
        encoder
            .finish()
            .map_err(|source| AppError::WriteArchiveTar {
                archive_dir: archive_dir.to_path_buf(),
                archive_tar: archive_path.to_path_buf(),
                source,
            })?;
        Ok(())
    } else {
        let mut builder = tar::Builder::new(file);
        write_archive_tar(archive_dir, archive_path, &mut builder)
    }
}

fn write_archive_tar<W: std::io::Write>(
    archive_dir: &Path,
    archive_path: &Path,
    builder: &mut tar::Builder<W>,
) -> Result<(), AppError> {
    builder
        .append_dir_all(".", archive_dir)
        .map_err(|source| AppError::WriteArchiveTar {
            archive_dir: archive_dir.to_path_buf(),
            archive_tar: archive_path.to_path_buf(),
            source,
        })?;
    builder
        .finish()
        .map_err(|source| AppError::WriteArchiveTar {
            archive_dir: archive_dir.to_path_buf(),
            archive_tar: archive_path.to_path_buf(),
            source,
        })
}

fn classify_run_files(
    run_dir: &Path,
    filestructure: &config::FileStructure,
) -> Result<ClassifiedFiles, AppError> {
    let mut files = ClassifiedFiles::default();

    for entry in WalkDir::new(run_dir).follow_links(false) {
        let entry = entry.map_err(|source| AppError::WalkRunDirectory {
            run_dir: run_dir.to_path_buf(),
            source,
        })?;
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let relative_path = path
            .strip_prefix(run_dir)
            .map(Path::to_path_buf)
            .map_err(|_| AppError::RunFileOutsideRunDir {
                run_dir: run_dir.to_path_buf(),
                path: path.to_path_buf(),
            })?;
        classify_relative_file(run_dir, relative_path, filestructure, &mut files)?;
    }

    Ok(files)
}

fn classify_relative_file(
    run_dir: &Path,
    relative_path: PathBuf,
    filestructure: &config::FileStructure,
    files: &mut ClassifiedFiles,
) -> Result<(), AppError> {
    let ignored = filestructure.ignore_paths.contains(&relative_path)
        || filestructure
            .ignore_globs
            .iter()
            .any(|pattern| pattern.matches_path(&relative_path));
    let checkout = filestructure.checkout_paths.contains(&relative_path)
        || filestructure
            .checkout_globs
            .iter()
            .any(|pattern| pattern.matches_path(&relative_path));

    match (ignored, checkout) {
        (true, true) => Err(AppError::FileStructureConflict {
            run_dir: run_dir.to_path_buf(),
            relative_path,
        }),
        (true, false) => {
            files.ignored.push(relative_path);
            Ok(())
        }
        (false, true) => {
            files.checkout.push(relative_path);
            Ok(())
        }
        (false, false) => {
            files.archived.push(relative_path);
            Ok(())
        }
    }
}

fn check_run_trees(
    tree_check_source: &Path,
    categories: &[config::Category],
) -> Result<(), AppError> {
    check_readable_directory(tree_check_source, "tree_check_source")?;
    let entries = fs::read_dir(tree_check_source).map_err(|source| AppError::ReadDirectory {
        field: "tree_check_source",
        path: tree_check_source.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| AppError::ReadDirectory {
            field: "tree_check_source",
            path: tree_check_source.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| AppError::ReadMetadata {
            field: "tree_check_source",
            path: path.clone(),
            source,
        })?;
        if !file_type.is_dir() {
            continue;
        }
        if let Some(target) = classify(&path, categories)? {
            let _ = classify_run_files(&path, &target.filestructure)?;
        }
    }

    Ok(())
}

fn load_config(config_path: &Path) -> Result<Config, AppError> {
    Config::from_path(config_path).map_err(|source| AppError::LoadConfig {
        path: config_path.to_path_buf(),
        source,
    })
}

fn check_ssh_access(config: &Config) -> Result<(), AppError> {
    debug!(
        "Checking SSH access. User name: {} port: {} domain name: {}",
        config.server_user, config.server_port, config.server_host
    );
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

fn write_cron_file(logdir: &Path, config_path: &Path) -> Result<PathBuf, AppError> {
    debug!("Determining cron file content");
    let binary_path = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|source| AppError::CurrentExe { source })?;
    let cron_path = cron_file_path(logdir);
    let contents = render_cron_file(config_path, &binary_path)?;
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

fn render_cron_file(config_path: &Path, binary_path: &Path) -> Result<String, AppError> {
    let binary_path_str = binary_path
        .to_str()
        .ok_or_else(|| AppError::NonUtf8CronPath {
            field: "current executable",
            path: binary_path.to_path_buf(),
        })?;
    let config_path_str = config_path
        .to_str()
        .ok_or_else(|| AppError::NonUtf8CronPath {
            field: "config path",
            path: config_path.to_path_buf(),
        })?;
    let command = format!(
        "{} run --config-path {}",
        shell_quote(binary_path_str),
        shell_quote(config_path_str),
    );

    Ok(format!(
        "# Install this file into cron manually.\n* * * * * {command}\n"
    ))
}

fn check_lock_is_available(lock_file: &Path) -> Result<(), AppError> {
    debug!(
        "Checking availability of lock file at {}",
        lock_file.display()
    );
    let _lock = acquire_run_lock(lock_file)?.ok_or_else(|| AppError::RunLockHeld {
        path: lock_file.to_path_buf(),
    })?;
    Ok(())
}

fn acquire_run_lock(path: &Path) -> Result<Option<RunLock>, AppError> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|source| AppError::OpenRunLockFile {
            path: path.to_path_buf(),
            source,
        })?;

    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(RunLock { file })),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(source) => Err(AppError::AcquireRunLock {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn lock_file_parent(lock_file: &Path) -> Result<&Path, AppError> {
    lock_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| AppError::MissingLockFileParent {
            path: lock_file.to_path_buf(),
        })
}

fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', r#"'\''"#);
    format!("'{escaped}'")
}

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
    #[error("failed to create transfer directory {}: {source}", path.display())]
    CreateTransferDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to copy transfer file {} to {}: {source}", source_path.display(), destination.display())]
    CopyTransferFile {
        source_path: PathBuf,
        destination: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create archive tar file {}: {source}", path.display())]
    CreateArchiveTar {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write archive tar from {} to {}: {source}", archive_dir.display(), archive_tar.display())]
    WriteArchiveTar {
        archive_dir: PathBuf,
        archive_tar: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove archive directory {} after creating archive.tar: {source}", path.display())]
    RemoveArchiveDir {
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
    #[error("failed to scan for completion file in {}: {source}", run_dir.display())]
    CompletionFileScan {
        run_dir: PathBuf,
        #[source]
        source: glob::GlobError,
    },
    #[error("failed to scan for classification file in {}: {source}", run_dir.display())]
    ClassificationFileScan {
        run_dir: PathBuf,
        #[source]
        source: glob::GlobError,
    },
    #[error("failed to walk run directory {}: {source}", run_dir.display())]
    WalkRunDirectory {
        run_dir: PathBuf,
        #[source]
        source: walkdir::Error,
    },
    #[error("file {} in run {} matches both ignore_globs and checkout_globs", relative_path.display(), run_dir.display())]
    FileStructureConflict {
        run_dir: PathBuf,
        relative_path: PathBuf,
    },
    #[error("checked-out file {} in run {} conflicts with internal archive directory name `{archive_dir_name}`", relative_path.display(), run_dir.display())]
    ArchiveDirCheckoutConflict {
        run_dir: PathBuf,
        relative_path: PathBuf,
        archive_dir_name: &'static str,
    },
    #[error("run file {} is not under run directory {}", path.display(), run_dir.display())]
    RunFileOutsideRunDir { run_dir: PathBuf, path: PathBuf },
    #[error("failed to write transfer marker {}: {source}", path.display())]
    WriteTransferMarker {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to inspect transfer marker {}: {source}", path.display())]
    ReadTransferMarker {
        path: PathBuf,
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
    #[error("cannot render cron file because {field} is not valid UTF-8: {}", path.display())]
    NonUtf8CronPath { field: &'static str, path: PathBuf },
    #[error("lock file path has no parent directory: {}", path.display())]
    MissingLockFileParent { path: PathBuf },
    #[error(
        "directory `{dir_name}` matched category regex `{category_regex}` with year_subdirectory enabled, but name does not start with two ASCII digits"
    )]
    YearSubdirectoryInvalidName {
        dir_name: String,
        category_regex: String,
    },
    #[error("setup requires either --tree-check-source PATH or --skip-tree-check")]
    MissingTreeCheckArg,
    #[error("setup cannot use both --tree-check-source and --skip-tree-check")]
    ConflictingTreeCheckArgs,
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::{Mutex, Once};
    use std::time::{SystemTime, UNIX_EPOCH};

    use glob::Pattern;
    use log::{LevelFilter, Log, Metadata, Record};
    use regex::Regex;

    use super::{classify, cron_file_path, render_cron_file, run_is_complete, scan_directories};
    use crate::config::{Category, FileStructure};
    use crate::transfer_log::TransferLog;

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

    fn test_category(
        regex: &str,
        classification_glob: Option<&str>,
        landing_zone: &str,
    ) -> Category {
        Category {
            regex: Regex::new(regex).expect("regex should parse"),
            classification_glob: classification_glob
                .map(|pattern| Pattern::new(pattern).expect("glob should parse")),
            landing_zone: PathBuf::from(landing_zone),
            filestructure: Arc::new(FileStructure {
                name: "test".to_string(),
                ignore_paths: HashSet::new(),
                ignore_globs: Vec::new(),
                checkout_paths: HashSet::new(),
                checkout_globs: vec![Pattern::new("**").expect("glob should parse")],
                completion_file_globs: vec![
                    Pattern::new("report*.html").expect("glob should parse"),
                ],
            }),
            year_subdirectory: false,
        }
    }

    #[test]
    fn renders_cron_file() {
        let block = render_cron_file(
            Path::new("/etc/sequencer-sync/config.yaml"),
            Path::new("/usr/local/bin/sequencer-sync"),
        )
        .expect("cron file should render");

        assert!(block.contains("# Install this file into cron manually."));
        assert!(
            regex::Regex::new(r#"(\*/\d+|\*) \* \* \* \*"#)
                .unwrap()
                .is_match(&block)
        );
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
    fn classify_uses_first_regex_match_without_classification_glob() {
        let run_dir = Path::new("/tmp/ONT_WGS_run1");
        let categories = vec![
            test_category(r"^ONT_", None, "/tmp/first"),
            test_category(r"^ONT_", None, "/tmp/second"),
        ];

        let target = classify(run_dir, &categories)
            .expect("classification should succeed")
            .expect("directory should match category");

        assert_eq!(target.destination, PathBuf::from("/tmp/first"));
    }

    #[test]
    fn classify_falls_through_when_classification_glob_is_missing() {
        let tempdir = make_temp_dir();
        let run_dir = tempdir.join("ONT_WGS_run1");
        fs::create_dir(&run_dir).expect("should create run dir");
        let categories = vec![
            test_category(r"^ONT_", Some("core.marker"), "/tmp/core"),
            test_category(r"^ONT_", None, "/tmp/fallback"),
        ];

        let target = classify(&run_dir, &categories)
            .expect("classification should succeed")
            .expect("directory should match fallback category");

        assert_eq!(target.destination, PathBuf::from("/tmp/fallback"));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn classify_uses_first_regex_match_with_matching_classification_glob() {
        let tempdir = make_temp_dir();
        let run_dir = tempdir.join("ONT_WGS_run1");
        fs::create_dir(&run_dir).expect("should create run dir");
        fs::write(run_dir.join("core.marker"), "").expect("should write classification marker");
        let categories = vec![
            test_category(r"^ONT_", Some("core.marker"), "/tmp/core"),
            test_category(r"^ONT_", None, "/tmp/fallback"),
        ];

        let target = classify(&run_dir, &categories)
            .expect("classification should succeed")
            .expect("directory should match glob-qualified category");

        assert_eq!(target.destination, PathBuf::from("/tmp/core"));
        cleanup_temp_dir(&tempdir);
    }

    #[cfg(unix)]
    #[test]
    fn scan_directories_warns_and_skips_non_utf8_run_dir() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        init_test_logger();
        let tempdir = make_temp_dir();
        let source = tempdir.join("source");
        let logdir = tempdir.join("log");
        fs::create_dir(&source).expect("should create source dir");
        fs::create_dir(&logdir).expect("should create log dir");
        let run_dir = source.join(OsString::from_vec(b"run_\xff".to_vec()));
        if let Err(error) = fs::create_dir(&run_dir) {
            if matches!(error.raw_os_error(), Some(1 | 92)) {
                cleanup_temp_dir(&tempdir);
                return;
            }
            panic!("should create run dir: {error}");
        }
        let transfer_log = TransferLog::load(&logdir).expect("missing transfer log should load");
        let categories = vec![test_category(r"^run_", None, "/tmp/landing")];

        let result = scan_directories(&source, &categories, &transfer_log, false, false)
            .expect("scan should succeed");

        assert!(result.planned_transfers.is_empty());
        assert!(TEST_LOGGER.lines().iter().any(|line| {
            line.contains("Skipping directory with non-UTF-8 name")
                && line.contains("glob matching requires UTF-8")
        }));
        cleanup_temp_dir(&tempdir);
    }
}
