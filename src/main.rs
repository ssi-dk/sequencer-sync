use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Error};
use bstr::{BStr, BString, ByteSlice};
use clap::{Args, Parser, Subcommand};
use config::Config;
use flate2::Compression;
use flate2::write::GzEncoder;
use fs2::FileExt;
use log::{debug, warn};
use run_log::RunLog;
use thiserror::Error;
use transfer_log::TransferLog;
use walkdir::WalkDir;

use crate::paths::{
    CanonicalChildFileBuf, CanonicalDirBuf, DirEntrySubdirCases, NormalPathSegment,
    NormalPathSegmentBuf, NormalUTF8Segment, RelativePathBuf,
};

mod config;
mod paths;
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
            (Some(_), true) => return Err(UserError::ConflictingTreeCheckArgs.into()),
            (Some(p), false) => {
                let canonicalized = match p.canonicalize() {
                    Ok(p) => p,
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        return Err(UserError::NotFound {
                            description: "Argument to --tree-check-source".to_owned(),
                            path: p,
                        }
                        .into());
                    }
                    Err(source) => {
                        return Err(AppError::Internal(Error::from(source).context(format!(
                            "When canonicalizing --tree-check-source given as {:?}",
                            p
                        ))));
                    }
                };
                if !canonicalized.is_dir() {
                    todo!();
                }
                unsafe { TreeCheck::Source(CanonicalDirBuf::new_unchecked(canonicalized)) }
            }
            (None, false) => return Err(UserError::MissingTreeCheckArg.into()),
            (None, true) => TreeCheck::Skipped,
        };

        Ok(ValidatedSetupArgs {
            config_path: self.config_path,
            skip_ssh_check: self.skip_ssh_check,
            tree_check,
        })
    }
}

enum TreeCheck {
    Source(CanonicalDirBuf),
    Skipped,
}

struct ValidatedSetupArgs {
    // We do not check this path exists, since we load it on startup anyway,
    // loading the file will trigger the error.
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
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn try_main() -> Result<(), AppError> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Setup(args) => setup(args)?,
        Commands::Run(args) => run_command(args)?,
    }

    Ok(())
}

fn setup(raw_args: SetupArgs) -> Result<(), AppError> {
    let args = raw_args.validate()?;
    debug!("Loading config path at {}", args.config_path.display());
    let config = Config::from_path(&args.config_path)?;
    debug!(
        "Loaded {} configured filestructure(s)",
        config.file_structures.len()
    );

    validate_environment(&config, args.skip_ssh_check)?;
    match args.tree_check {
        TreeCheck::Source(p) => check_run_trees(&p, &config.categories)?,
        TreeCheck::Skipped => {}
    }
    check_lock_is_available(&config.lock_file)?;
    let _ = TransferLog::load(&config.logdir).map_err(UserError::TransferLog)?;
    transfer_log::initialize_if_absent(&config.logdir).map_err(UserError::TransferLog)?;
    eprintln!("Setup successful!");
    let cron_path = write_cron_file(&config.logdir, &args.config_path)?;
    eprintln!(
        "Install the generated cron job with your system cron configuration: {}",
        cron_path.display()
    );

    Ok(())
}

enum TransferDestination {
    LandingZone(CanonicalDirBuf),
    YearSubDirectory((CanonicalDirBuf, NormalUTF8Segment)),
}

// We might want to have distinct transfer targets e.g. from operational runs
// versus projects. Perhaps these need different file subsets.
// A TransferTarget stores information about source and destination once we have
// classified a run as e.g. project/operations/something else.
struct TransferTarget {
    // Zero-based index of which category
    category_index: usize,
    destination: TransferDestination,
    staging_zone: CanonicalDirBuf,
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

struct PlannedTransfer {
    run_dir: NormalUTF8Segment,
    reason: TransferReason,
    target: TransferTarget,
}

// Scanning returns both the work to perform and the operator-facing summary so
// the caller can decide whether to log anything at all before starting transfer.
// I.e. we don't want to log on noop calls since then running this program via
// a cron job would flood the log, making it useless.
struct ScanResult {
    planned_transfers: Vec<PlannedTransfer>,
    incomplete_messages: Vec<String>,
    summary: ScanSummary,
}

fn run_command(args: RunArgs) -> Result<(), AppError> {
    let config = Config::from_path(&args.config_path)?;
    let mut run_log = RunLog::new(&config.logdir)?;

    let _lock = match acquire_run_lock(&config.lock_file)? {
        Some(lock) => lock,
        None => {
            run_log.info("Run skipped: file lock already held");
            run_log.finish();
            if run_log.had_error() {
                return Err(UserError::RunLogWriteFailed.into());
            }
            return Ok(());
        }
    };

    let mut transfer_log = TransferLog::load(&config.logdir).map_err(UserError::TransferLog)?;

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
        return Err(UserError::RunLogWriteFailed.into());
    }

    Ok(())
}

fn validate_environment(config: &Config, skip_ssh_check: bool) -> Result<(), AppError> {
    debug!(
        "Checking that source directory is readable: {}",
        config.source.as_ref().display()
    );
    check_readable_directory(&config.source, "source")?;
    for (category_index, cat) in config.categories.iter().enumerate() {
        debug!("Checking category {}", category_index + 1);

        debug!(
            "\tChecking files can be created in staging zone {}",
            cat.staging_zone.as_ref().display()
        );
        let segment: NormalPathSegmentBuf = NormalUTF8Segment::from_timestamp().into();
        let staging_zone_marker_path = cat
            .staging_zone
            .join_file_name(&segment, "Staging zone marker file")?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging_zone_marker_path)
            .with_context(|| {
                format!(
                    "Failure when writing marker file to staging directory to check \
                    writeability of staging directory.\nMarker path: {:?}",
                    staging_zone_marker_path.as_ref()
                )
            })?;

        debug!("\tChecking file can be moved to landing_zone");
        let landing_zone_marker_path = cat
            .landing_zone
            .join_file_name(&segment, "Landing zone marker file")?;
        move_from_staging_zone_to_landing_zone(
            staging_zone_marker_path.as_ref(),
            landing_zone_marker_path.as_ref(),
        )?;

        debug!("Removing marker file from landing zone");

        fs::remove_file(&landing_zone_marker_path).with_context(|| {
            format!(
                "Failed to remove marker file from landing zone at {}",
                landing_zone_marker_path.as_ref().display(),
            )
        })?;

        check_writable_directory(&cat.landing_zone, "category.landing_zone")?;
    }
    debug!(
        "Checking writability of lock file parent directory: {}",
        config.lock_file.parent().as_ref().display()
    );
    check_writable_directory(&config.lock_file.parent(), "lock_file parent")?;
    debug!(
        "Checking writability of logdir: {}",
        config.logdir.as_ref().display()
    );
    check_writable_directory(&config.logdir, "logdir")?;
    if !skip_ssh_check {
        check_ssh_access(config)?;
    } else {
        debug!("SSH check skipped due to command line flag.")
    }
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
    source: &CanonicalDirBuf,
    categories: &[config::Category],
    transfer_log: &TransferLog,
    retry_failed: bool,
    transfer_incomplete: bool,
) -> Result<ScanResult, AppError> {
    debug!(
        "Searching for new directories in {}",
        &source.as_ref().display()
    );

    let entries = fs::read_dir(source)
        .with_context(|| format!("Failure when reading the source directory at {:?}", source))?;

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
        let entry = entry.with_context(|| {
            format!(
                "Failure when reading entry of source directory {:?}",
                source.as_ref()
            )
        })?;

        let (entry_path, segment) = match paths::utf8_subdir(&entry) {
            DirEntrySubdirCases::UTF8SubDir {
                full_path,
                last_segment,
            } => (full_path, last_segment),
            DirEntrySubdirCases::IOError(e) => {
                return Err(AppError::Internal(Error::from(e).context("TODO!")));
            }
            DirEntrySubdirCases::IsSymlink => {
                warn!(
                    "Found symlink when traversing source directory: {:?}, skipping",
                    &entry.file_name()
                );
                continue;
            }
            DirEntrySubdirCases::NotUTF8 => {
                warn!(
                    "Found non-UTF8 entry in source {}: {}. Skipping",
                    source.as_ref().display(),
                    entry.file_name().display()
                );
                continue;
            }
            DirEntrySubdirCases::IsNotDir => continue,
        };

        let relative = segment.as_normal();

        debug!("Classifying {}", entry_path.as_ref().display());

        let Some(dir_name) = relative.as_ref().to_str() else {
            warn!("\tSkipping directory with non-UTF-8 name; glob matching requires UTF-8");
            continue;
        };

        let action = transfer_log.transfer_action(&segment, retry_failed);
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

        let target = match categorize(&entry_path, categories)? {
            Some(t) => {
                let lz = match &t.destination {
                    TransferDestination::LandingZone(lz) => lz,
                    TransferDestination::YearSubDirectory((lz, _)) => lz,
                };
                debug!(
                    "\tClassified: Category {} with filestructure {} and landing zone {}",
                    &t.category_index + 1,
                    &t.filestructure.name,
                    &lz.as_ref().display()
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

        planned_transfers.push(PlannedTransfer {
            run_dir: segment,
            reason,
            target,
        });
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
) -> Result<bool, UserError> {
    for completion_file_glob in completion_file_globs {
        if !glob_has_match(run_dir, completion_file_glob).map_err(|source| {
            UserError::CompletionFileScan {
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

fn categorize(
    run_dir: &CanonicalDirBuf,
    categories: &[config::Category],
) -> Result<Option<TransferTarget>, AppError> {
    let dir_name = run_dir
        .as_ref()
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
            let full_path = run_dir.as_ref().join(classification_glob.as_str());
            let class_glob_display = full_path.display();
            if !glob_has_match(run_dir.as_ref(), classification_glob).with_context(|| {
                format!(
                    "Failed to match potential run dir to classification glob.\n\
                    Run dir: {:?}",
                    run_dir.as_ref()
                )
            })? {
                debug!("\tClassification glob did not match: {class_glob_display}",);
                continue;
            } else {
                debug!("\tMatched classification glob: {class_glob_display}")
            }
        }

        // Landing zone must be PathBuf, because if year_subdirectory, the directory
        // may not exist yet.
        let destination: TransferDestination = if cat.year_subdirectory {
            let bytes = dir_name.as_bytes();
            if bytes.len() < 2 || !bytes[0].is_ascii_digit() || !bytes[1].is_ascii_digit() {
                return Err(UserError::YearSubdirectoryInvalidName {
                    dir_name: dir_name.to_owned(),
                    category_regex: cat.regex.to_string(),
                }
                .into());
            }
            // This can't fail because it's guaranteed to just be four ASCII integers
            let segment = NormalPathSegment::new(Path::new(&format!(
                "20{}{}",
                bytes[0] as char, bytes[1] as char
            )))
            .unwrap()
            .try_into()
            .unwrap();
            TransferDestination::YearSubDirectory((cat.landing_zone.clone(), segment))
        } else {
            TransferDestination::LandingZone(cat.landing_zone.clone())
        };
        return Ok(Some(TransferTarget {
            category_index,
            destination,
            staging_zone: cat.staging_zone.clone(),
            filestructure: cat.file_structure.clone(),
        }));
    }
    Ok(None)
}

fn transfer_new_directories(
    source: &CanonicalDirBuf,
    categories: &[config::Category],
    transfer_log: &mut TransferLog,
    run_log: &mut RunLog,
    args: &RunArgs,
) -> Result<(), AppError> {
    let (mut succeeded, mut failed) = (0u32, 0u32);

    // Scan directories to classify them
    let ScanResult {
        planned_transfers,
        incomplete_messages,
        summary,
    } = scan_directories(
        source,
        categories,
        transfer_log,
        args.retry_failed,
        args.transfer_incomplete,
    )?;

    debug!("Total directories to transfer: {}", planned_transfers.len());

    if !summary.has_noteworthy_activity() {
        return Ok(());
    }

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

    for PlannedTransfer {
        run_dir,
        reason,
        target,
    } in planned_transfers
    {
        let source_run_dir = source
            .try_from_existing_subdirectory(run_dir.as_normal())
            .expect("We obta")
            .expect_normal_dir("Run dir should be directory");
        let classification = classify_run_files(&source_run_dir, &target.filestructure)?;

        let destination_to_create = {
            let lz: PathBuf = match &target.destination {
                TransferDestination::LandingZone(lz) => lz.as_ref().to_owned(),
                TransferDestination::YearSubDirectory((lz, subdir)) => {
                    lz.as_ref().join(subdir.as_normal().as_ref())
                }
            };
            lz.join(run_dir.as_normal().as_ref())
        };

        if destination_to_create.exists() {
            let message = format!(
                "Skipped directory because it is unexpectedly still present in landing zone: at: {}",
                destination_to_create.display()
            );
            warn!("{message}");
            if !args.dry_run {
                run_log.info(&message);
            }
            continue;
        } else {
            if args.dry_run {
                print_dry_run(&run_dir, &destination_to_create, &target.filestructure);
                continue;
            }
        };

        // Create the intermediate year subdirectory if it does not exists
        let destination = match target.destination {
            TransferDestination::LandingZone(lz) => lz.clone(),
            TransferDestination::YearSubDirectory((lz, segment)) => {
                lz.create_if_not_exist(segment.as_normal())?
            }
        };

        let transfer_result = transfer_run_to_landing_zone(
            &run_dir,
            &source_run_dir,
            &target.staging_zone,
            &destination,
            classification,
            args.compress,
        );

        transfer_log
            .record_transfer(&run_dir, transfer_result.is_ok())
            .map_err(UserError::TransferLog)?;

        match transfer_result {
            Ok(()) => {
                succeeded += 1;
                run_log.info(&format!(
                    "Transferred {} {} -> {}",
                    transfer_reason_label(reason),
                    run_dir.into_inner(),
                    destination.as_ref().display()
                ));
            }
            Err(error) => {
                failed += 1;
                run_log.error(&format!(
                    "FAILED transfer {} {} -> {}: {error}",
                    transfer_reason_label(reason),
                    run_dir.into_inner(),
                    destination.as_ref().display()
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
const ARCHIVE_DIR_NAME: &str = "sequencer-sync-archive";

fn print_dry_run(
    segment: &NormalUTF8Segment,
    // This need not exist due to year_subdirectory. The landing zone must exist,
    // but year_subdirectory can make the destination be a non-existing subdir
    destination: &Path,
    filestructure: &config::FileStructure,
) {
    println!(
        "{} -> {}",
        segment.as_normal().as_ref().display(),
        destination.display()
    );
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
    // These are all relative to the run dir when they are discovered
    ignored: Vec<RelativePathBuf>,
    archived: Vec<RelativePathBuf>,
    checkout: Vec<RelativePathBuf>,
}

fn transfer_run_to_landing_zone(
    run_dir: &NormalUTF8Segment,
    directory_to_transfer: &CanonicalDirBuf,
    staging_zone: &CanonicalDirBuf,
    landing_zone: &CanonicalDirBuf,
    classified_files: ClassifiedFiles,
    compress: bool,
) -> Result<(), AppError> {
    ensure_no_archive_dir_checkout_conflict(directory_to_transfer, &classified_files.checkout)?;

    let staging_run_dir = staging_run_dir_segment(run_dir);
    let canonical_staging_run_dir = staging_zone.create_subdir(staging_run_dir.as_normal())?;

    // Create parent directories once, so we can create files without worrying
    // about whether their parent exists
    create_parent_directories(&canonical_staging_run_dir, &classified_files.checkout)?;

    for relative_path in &classified_files.checkout {
        copy_classified_file(
            relative_path,
            directory_to_transfer,
            &canonical_staging_run_dir,
        )?;
    }

    // Create the tarball only if there are files to be archived.
    if !classified_files.archived.is_empty() {
        // Safety: We know that ARCHIVE_DIR_NAME is a normal path segment
        let archive_segment = NormalPathSegment::new(Path::new(ARCHIVE_DIR_NAME)).unwrap();
        let archive_dir = canonical_staging_run_dir.create_subdir(archive_segment)?;

        create_parent_directories(&archive_dir, &classified_files.archived)?;
        for relative_path in &classified_files.archived {
            copy_classified_file(relative_path, directory_to_transfer, &archive_dir)?;
        }

        let archive_segment = if compress {
            NormalPathSegment::new(Path::new("archive.tar.gz")).unwrap()
        } else {
            NormalPathSegment::new(Path::new("archive.tar")).unwrap()
        };
        let archive_path = canonical_staging_run_dir.join_file_name(
            archive_segment,
            "Auto-generated tar archive path in staging dir",
        )?;

        create_archive_tar(&archive_path, &archive_dir, compress)?;
        fs::remove_dir_all(&archive_dir)
            .context("Failed to remove archive directory after creating tar")?;
    }

    move_from_staging_zone_to_landing_zone(
        canonical_staging_run_dir.as_ref(),
        &landing_zone.as_ref().join(run_dir.as_normal().as_ref()),
    )?;
    Ok(())
}

fn move_from_staging_zone_to_landing_zone(
    source: &Path,
    destination: &Path,
) -> Result<(), UserError> {
    match fs::rename(source, destination) {
        Ok(_) => Ok(()),
        Err(error) => match error.kind() {
            std::io::ErrorKind::CrossesDevices => Err(UserError::StagingZoneNotOnRightDevice {
                from: source.to_owned(),
                to: destination.to_owned(),
            }),
            _ => Err(UserError::RenameFailed {
                from: source.to_owned(),
                to: destination.to_owned(),
            }),
        },
    }
}

fn staging_run_dir_segment(run_dir: &NormalUTF8Segment) -> NormalUTF8Segment {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    let segment = format!("{}-{timestamp}", run_dir.clone().into_inner(),);
    NormalPathSegment::new(Path::new(&segment))
        .expect("staging run directory should be a normal path segment")
        .try_into()
        .expect("staging run directory should be valid UTF-8")
}

fn ensure_no_archive_dir_checkout_conflict(
    run_dir: &CanonicalDirBuf,
    checkout_paths: &[RelativePathBuf],
) -> Result<(), UserError> {
    for relative_path in checkout_paths {
        let first_component = relative_path
            .as_ref()
            .components()
            .next()
            .expect("Relative paths must have at least one component");
        if archive_dir_name_conflicts(first_component.as_os_str()) {
            return Err(UserError::ArchiveDirCheckoutConflict {
                run_dir: run_dir.as_ref().to_owned(),
                relative_path: relative_path.as_ref().to_owned(),
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
    target_root: &CanonicalDirBuf,
    relative_paths: &[RelativePathBuf],
) -> Result<(), AppError> {
    // Use HashSet to deduplicate
    let mut directories = HashSet::<RelativePathBuf>::new();

    for relative_path in relative_paths {
        // We need to create all directories. The target root must already exist,
        // so we can skip that
        let parent = if let Some(parent) = relative_path.parent() {
            parent
        } else {
            continue;
        };
        directories.insert(parent);
    }

    for directory in directories {
        let to_create = target_root.as_ref().join(directory.as_ref());
        fs::create_dir_all(&to_create).with_context(|| {
            format!(
                "Failed to create sub-directories of root {:?}.\n
                Failing directory: {:?}",
                target_root.as_ref(),
                to_create
            )
        })?;
    }

    Ok(())
}

fn copy_classified_file(
    relative_path: &RelativePathBuf,
    source: &CanonicalDirBuf,
    target: &CanonicalDirBuf,
) -> Result<(), AppError> {
    let source_path = source.as_ref().join(relative_path.as_ref());
    let target_path = target.as_ref().join(relative_path.as_ref());
    fs::copy(&source_path, &target_path).with_context(|| {
        format!(
            "Failed to copy transferred file to staging area.\nFrom {:?}\nTo {:?}",
            source_path, target_path
        )
    })?;
    Ok(())
}

fn create_archive_tar(
    archive_path: &CanonicalChildFileBuf,
    archive_dir: &CanonicalDirBuf,
    compress: bool,
) -> Result<(), AppError> {
    let file = File::create(archive_path)
        .with_context(|| format!("Archive tar already exists at {:?}", archive_path.as_ref()))?;
    if compress {
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        write_archive_tar(archive_dir, &mut builder)?;
        let encoder = builder
            .into_inner()
            .context("Failed to write archive tar in staging area")?;
        encoder
            .finish()
            .context("Failed to write archive tar in staging area")?;
        Ok(())
    } else {
        let mut builder = tar::Builder::new(file);
        write_archive_tar(archive_dir, &mut builder)
    }
}

fn write_archive_tar<W: std::io::Write>(
    archive_dir: &CanonicalDirBuf,
    builder: &mut tar::Builder<W>,
) -> Result<(), AppError> {
    builder
        .append_dir_all(".", archive_dir)
        .context("Failed to write archive tar in staging area")?;
    builder
        .finish()
        .context("Failed to write archive tar in staging area")?;
    Ok(())
}

fn classify_run_files(
    run_dir: &CanonicalDirBuf,
    filestructure: &config::FileStructure,
) -> Result<ClassifiedFiles, AppError> {
    let mut files = ClassifiedFiles::default();

    for entry in WalkDir::new(run_dir).follow_links(false) {
        let entry = entry
            .with_context(|| format!("Failed to walk run directory at {:?}", run_dir.as_ref()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let relative = RelativePathBuf::new(
            entry
                .path()
                .strip_prefix(run_dir.as_ref())
                .expect("Internal error: Path of entry must start with base path"),
        )
        .expect("Internal error: Expected path of entry to be normalized");
        classify_relative_file(run_dir, relative, filestructure, &mut files)?;
    }

    debug!(
        "Classified {} file(s) for checkout, {} file(s) for archive, and ignored {} file(s)",
        files.checkout.len(),
        files.archived.len(),
        files.ignored.len()
    );

    Ok(files)
}

fn classify_relative_file(
    run_dir: &CanonicalDirBuf,
    path: RelativePathBuf,
    filestructure: &config::FileStructure,
    files: &mut ClassifiedFiles,
) -> Result<(), UserError> {
    let ignored = filestructure.ignore_paths.contains(path.as_ref())
        || filestructure
            .ignore_globs
            .iter()
            .any(|pattern| pattern.matches_path(path.as_ref()));
    let checkout = filestructure.checkout_paths.contains(path.as_ref())
        || filestructure
            .checkout_globs
            .iter()
            .any(|pattern| pattern.matches_path(path.as_ref()));

    match (ignored, checkout) {
        (true, true) => Err(UserError::FileStructureConflict {
            run_dir: run_dir.as_ref().to_owned(),
            relative_path: path.as_ref().to_owned(),
        }),
        (true, false) => {
            files.ignored.push(path);
            Ok(())
        }
        (false, true) => {
            files.checkout.push(path);
            Ok(())
        }
        (false, false) => {
            files.archived.push(path);
            Ok(())
        }
    }
}

fn check_run_trees(
    tree_check_source: &CanonicalDirBuf,
    categories: &[config::Category],
) -> Result<(), AppError> {
    check_readable_directory(tree_check_source, "tree_check_source")?;
    let entries = fs::read_dir(tree_check_source).with_context(|| {
        format!(
            "Error when checking reading directory {:?}",
            tree_check_source.as_ref()
        )
    })?;

    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "Error when checking reading directory {:?}",
                tree_check_source.as_ref()
            )
        })?;
        let (sub_dir, _) = match paths::utf8_subdir(&entry) {
            DirEntrySubdirCases::UTF8SubDir {
                full_path,
                last_segment,
            } => (full_path, last_segment),
            DirEntrySubdirCases::IOError(_) => {
                panic!("TODO");
            }
            DirEntrySubdirCases::NotUTF8 => {
                warn!(
                    "Found non-UTF8 directory entry: {:?}, skipping",
                    &entry.file_name()
                );
                continue;
            }
            DirEntrySubdirCases::IsSymlink => {
                warn!(
                    "Found symlink when traversing tun trees: {:?}, skipping",
                    &entry.file_name()
                );
                continue;
            }
            DirEntrySubdirCases::IsNotDir => {
                continue;
            }
        };
        if let Some(target) = categorize(&sub_dir, categories)? {
            let _ = classify_run_files(&sub_dir, &target.filestructure)?;
        }
    }

    Ok(())
}

fn check_ssh_access(config: &Config) -> Result<(), AppError> {
    debug!(
        "Checking SSH access. User name: {} port: {} domain name: {}",
        config.server_user, config.server_port, config.server_host
    );
    let output = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-p")
        .arg(config.server_port.to_string())
        .arg(format!("{}@{}", config.server_user, config.server_host))
        .arg("--")
        .arg("true")
        .output()
        .context("Failed to execute SSH command")?;

    if output.status.success() {
        Ok(())
    } else {
        Err(UserError::SshAccessDenied {
            user: config.server_user.clone(),
            host: config.server_host.clone(),
            port: config.server_port,
            stderr: BString::new(output.stderr),
        }
        .into())
    }
}

fn check_readable_directory(path: &CanonicalDirBuf, description: &str) -> Result<(), UserError> {
    fs::read_dir(path).map_err(|source| UserError::DirectoryNotReadable {
        description: description.to_owned(),
        path: path.as_ref().to_owned(),
        source,
    })?;
    Ok(())
}

fn check_writable_directory(path: &CanonicalDirBuf, description: &str) -> Result<(), AppError> {
    let segment: NormalPathSegmentBuf = NormalUTF8Segment::from_timestamp().into();
    let temp_path = path.join_file_name(&segment, description)?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|source| UserError::DirectoryNotWriteable {
            description: description.to_owned(),
            path: path.as_ref().to_owned(),
            source,
        })?;

    fs::remove_file(&temp_path).with_context(|| {
        format!(
            "Failure deleting probe file at {temp_path:?}.\n\
        This probe file was just created, so we do have write permissions to the directory."
        )
    })?;

    Ok(())
}

// NB: config_path is an absolute path to existing config file
fn write_cron_file(logdir: &CanonicalDirBuf, config_path: &Path) -> Result<PathBuf, AppError> {
    debug!("Determining cron file content");
    let binary_path = std::env::current_exe()
        .context("Failed to determine path of current executable")
        .and_then(|path| {
            path.canonicalize()
                .context("Failed to canonicalize path of current executable")
        })?;

    let cron_path = logdir.as_ref().join("sequencer-sync.cron");
    let contents = render_cron_file(config_path, &binary_path);
    debug!(
        "Writing cron content to cron file at {}",
        cron_path.display()
    );
    fs::write(&cron_path, contents)
        .with_context(|| format!("Failed to write cron file to path {:?}", cron_path))?;
    Ok(cron_path)
}

fn render_cron_file(config_path: &Path, binary_path: &Path) -> Vec<u8> {
    let binary_path_str = shell_quote(BStr::new(binary_path.as_os_str().as_bytes()));
    let config_path_str = shell_quote(BStr::new(config_path.as_os_str().as_bytes()));

    let mut out = Vec::new();
    out.extend_from_slice(b"# Install this file into cron manually.\n* * * * * ");
    out.extend_from_slice(&binary_path_str);
    out.extend_from_slice(b" run --config-path ");
    out.extend_from_slice(&config_path_str);
    out.push(b'\n');

    out
}

fn check_lock_is_available(lock_file: &CanonicalChildFileBuf) -> Result<(), UserError> {
    debug!(
        "Checking availability of lock file at {}",
        lock_file.as_ref().display()
    );
    let _lock = acquire_run_lock(lock_file)?.ok_or_else(|| UserError::RunLockHeld {
        path: lock_file.as_ref().to_owned(),
    })?;
    Ok(())
}

fn acquire_run_lock(path: &CanonicalChildFileBuf) -> Result<Option<RunLock>, UserError> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|source| UserError::OpenRunLockFile {
            path: path.as_ref().to_owned(),
            source,
        })?;

    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(RunLock { file })),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(source) => Err(UserError::AcquireRunLock {
            path: path.as_ref().to_owned(),
            source,
        }),
    }
}

fn shell_quote(value: &BStr) -> BString {
    let mut s = BString::new(vec![b'\'']);
    s.append(&mut value.replace(b"\'", b"'\\''").to_vec());
    s.push(b'\'');
    s
}

struct RunLock {
    file: File,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[derive(Debug)]
enum AppError {
    // User-facing error which, when displayed, simply prints a message to
    // the user and exists.
    User(UserError),
    // Internal errors which uses Anyhow, and contains context, and displays
    // in a complex, stack-trace-like manner
    Internal(Error),
}

impl AppError {
    fn internal_from<E>(error: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self::Internal(anyhow::Error::from(error))
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::User(error) => write!(f, "Error: {error}"),
            AppError::Internal(error) => write!(f, "Internal error in sequencer-sync: {error:?}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<UserError> for AppError {
    fn from(error: UserError) -> Self {
        Self::User(error)
    }
}

impl From<Error> for AppError {
    fn from(error: Error) -> Self {
        Self::Internal(error)
    }
}

#[derive(Debug, Error)]
enum UserError {
    #[error("Unsupported config version {found}; this binary supports config version {supported}")]
    UnsupportedConfigVersion { found: u16, supported: u16 },

    #[error("ssh access check failed for {user}@{host}:{port}. Stderr:\n{stderr}")]
    SshAccessDenied {
        user: String,
        host: String,
        port: u16,
        stderr: BString,
    },
    #[error("{description} ends in '..', but must be a regular path. Found path: {path:?}")]
    PathEndsInParent { description: String, path: PathBuf },
    #[error("{description} has no parent. It could terminate in '/'. Found path: {path:?}")]
    PathHasNoParent { description: String, path: PathBuf },
    #[error("{description} at {path:?} is a symlink, but must be a regular file.")]
    IsSymlinkNotRegularFile { description: String, path: PathBuf },
    #[error(
        "{description} at {path:?} already exists, and is not a normal file. It could be a directory or a symlink."
    )]
    IsNotFileOrMissing { description: String, path: PathBuf },

    #[error("failed to rename from staging zone to landing zone.\n\
        \tDirectory in staging zone: {}\n\
        \tDirectory in landing zone: {}",
        from.display(),
        to.display())]
    RenameFailed { from: PathBuf, to: PathBuf },
    #[error(
        "failed to move from staging zone to landing zone, because they are on separate devices. \
        Please make sure, for all categories in config file, that their landing zone and \
        staging zones are on the same device (i.e. file system).\n\
        \tDirectory in staging zone: {}\n\
        \tDirectory in landing zone: {}",
        from.display(),
        to.display())]
    StagingZoneNotOnRightDevice { from: PathBuf, to: PathBuf },
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
    #[error("In config file, {description} is not a valid regex: {source}")]
    InvalidConfigRegex {
        description: String,
        #[source]
        source: regex::Error,
    },
    #[error(
        "In config file, {description} must be a relative path/glob inside the run directory: {pattern:?}"
    )]
    ConfigGlobOutsideRunDirectory {
        description: String,
        pattern: String,
    },
    #[error("file {} in run {} matches both ignore_globs and checkout_globs", relative_path.display(), run_dir.display())]
    FileStructureConflict {
        run_dir: PathBuf,
        relative_path: PathBuf,
    },
    #[error(
        "config fields `{first}` and `{second}` must not point to the same path: {}",
        path.display()
    )]
    DuplicateConfigPath {
        first: String,
        second: String,
        path: PathBuf,
    },
    #[error(
        "In config file, file structure \"{name}\" has an empty completion file glob list.\n\
        This is not permitted, because sequencer-sync will not know when the file is ready to transfer."
    )]
    EmptyCompletionFileGlobList { name: String },
    #[error("{description} cannot be the empty string")]
    EmptyString { description: String },
    #[error("In config file, port must not be 0")]
    ZeroPort,
    #[error("Config file must contain at least one category, but got none")]
    NoCategories,
    #[error("Config file must contain at least one file structure, but got none")]
    NoFileStructures,
    #[error(
        "In config, {description} is {glob_string}, which is not a valid glob pattern: {source}"
    )]
    InvalidConfigGlob {
        description: String,
        glob_string: String,
        #[source]
        source: glob::PatternError,
    },
    #[error("In config gile, category references unknown filestructure `{name}`")]
    UnknownConfigFileStructure { name: String },
    #[error("{description} at {path:?} is not a directory, but must be.")]
    NotADirectory { description: String, path: PathBuf },
    #[error("File or directory not found: {description} at {path:?}.")]
    NotFound { description: String, path: PathBuf },
    #[error("{description} must be an absolute path, but isn't. Got {:?}", path)]
    PathNotAbsolute { description: String, path: PathBuf },
    #[error("error when reading a directory {description} at {} \
        This directory must be readable \
        Please make sure that you have permissions to read the directory\n\
        Error: {}", path.display(), source)]
    DirectoryNotReadable {
        description: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("error when writing a probe file to {description}.\n\
        This directory must be writeable. Please make sure you have write permissions.\n\
        Probe file path: {}\n\
        Error: {}", path.display(), source)]
    DirectoryNotWriteable {
        description: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("checked-out file {} in run {} conflicts with internal archive directory name `{archive_dir_name}`", relative_path.display(), run_dir.display())]
    ArchiveDirCheckoutConflict {
        run_dir: PathBuf,
        relative_path: PathBuf,
        archive_dir_name: &'static str,
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
    #[error("setup requires either --tree-check-source PATH or --skip-tree-check")]
    MissingTreeCheckArg,
    #[error("setup cannot use both --tree-check-source and --skip-tree-check")]
    ConflictingTreeCheckArgs,
}

#[cfg(any())]
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

#[cfg(test)]
mod current_tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use glob::Pattern;
    use regex::Regex;

    use super::{
        ClassifiedFiles, TransferDestination, archive_dir_name_conflicts, categorize,
        classify_run_files, render_cron_file, run_is_complete, staging_run_dir_segment,
        transfer_run_to_landing_zone,
    };
    use crate::config::{Category, FileStructure};
    use crate::paths::{CanonicalDirBuf, NormalPathSegment, NormalUTF8Segment};

    static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    fn make_temp_dir() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let unique_id = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sequencer-sync-main-test-{}-{timestamp}-{unique_id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("should create temp dir");
        path
    }

    fn cleanup_temp_dir(path: &Path) {
        fs::remove_dir_all(path).expect("should remove temp dir");
    }

    fn canonical_dir(path: &Path) -> CanonicalDirBuf {
        CanonicalDirBuf::from_absolute(path, "test dir").expect("test dir should resolve")
    }

    fn segment(value: &str) -> NormalUTF8Segment {
        NormalPathSegment::new(Path::new(value))
            .expect("test segment should be normal")
            .try_into()
            .expect("test segment should be UTF-8")
    }

    fn file_structure() -> Arc<FileStructure> {
        Arc::new(FileStructure {
            name: "test".to_owned(),
            ignore_paths: HashSet::new(),
            ignore_globs: Vec::new(),
            checkout_paths: HashSet::new(),
            checkout_globs: Vec::new(),
            completion_file_globs: vec![Pattern::new("complete.txt").unwrap()],
        })
    }

    fn category(
        regex: &str,
        classification_glob: Option<&str>,
        landing_zone: &Path,
        staging_zone: &Path,
        year_subdirectory: bool,
    ) -> Category {
        Category {
            regex: Regex::new(regex).expect("regex should parse"),
            classification_glob: classification_glob.map(|pattern| Pattern::new(pattern).unwrap()),
            landing_zone: canonical_dir(landing_zone),
            staging_zone: canonical_dir(staging_zone),
            file_structure: file_structure(),
            year_subdirectory,
        }
    }

    #[test]
    fn renders_cron_file_with_shell_quoted_paths() {
        let block = render_cron_file(
            Path::new("/etc/sequencer sync/config.yaml"),
            Path::new("/usr/local/bin/sequencer-sync"),
        );
        let block = String::from_utf8(block).expect("cron file should be valid UTF-8 here");

        assert!(block.contains("# Install this file into cron manually."));
        assert!(block.contains("'/usr/local/bin/sequencer-sync' run"));
        assert!(block.contains("--config-path '/etc/sequencer sync/config.yaml'"));
    }

    #[test]
    fn run_is_complete_requires_all_completion_globs() {
        let tempdir = make_temp_dir();
        fs::write(tempdir.join("complete.txt"), "").expect("should write completion file");

        assert!(run_is_complete(&tempdir, &[Pattern::new("complete.txt").unwrap()]).unwrap());
        assert!(
            !run_is_complete(
                &tempdir,
                &[
                    Pattern::new("complete.txt").unwrap(),
                    Pattern::new("missing.txt").unwrap(),
                ],
            )
            .unwrap()
        );

        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn categorize_falls_through_when_classification_glob_is_missing() {
        let tempdir = make_temp_dir();
        let run_dir = tempdir.join("run-001");
        let landing_a = tempdir.join("landing-a");
        let landing_b = tempdir.join("landing-b");
        let staging = tempdir.join("staging");
        for dir in [&run_dir, &landing_a, &landing_b, &staging] {
            fs::create_dir(dir).expect("should create fixture dir");
        }

        let categories = vec![
            category("^run", Some("marker.txt"), &landing_a, &staging, false),
            category("^run", None, &landing_b, &staging, false),
        ];
        let target = categorize(&canonical_dir(&run_dir), &categories)
            .expect("categorization should succeed")
            .expect("fallback category should match");

        match target.destination {
            TransferDestination::LandingZone(path) => {
                assert_eq!(path.as_ref(), landing_b.canonicalize().unwrap())
            }
            TransferDestination::YearSubDirectory(_) => panic!("unexpected year destination"),
        }

        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn categorize_creates_year_destination_from_run_name_prefix() {
        let tempdir = make_temp_dir();
        let run_dir = tempdir.join("240101_RUN");
        let landing = tempdir.join("landing");
        let staging = tempdir.join("staging");
        for dir in [&run_dir, &landing, &staging] {
            fs::create_dir(dir).expect("should create fixture dir");
        }

        let categories = vec![category("^24", None, &landing, &staging, true)];
        let target = categorize(&canonical_dir(&run_dir), &categories)
            .expect("categorization should succeed")
            .expect("category should match");

        match target.destination {
            TransferDestination::YearSubDirectory((base, year)) => {
                assert_eq!(base.as_ref(), landing.canonicalize().unwrap());
                assert_eq!(year.into_inner(), "2024");
            }
            TransferDestination::LandingZone(_) => panic!("unexpected plain landing zone"),
        }

        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn classify_run_files_preserves_nested_relative_paths() {
        let tempdir = make_temp_dir();
        let run_dir = tempdir.join("run-001");
        fs::create_dir(&run_dir).expect("should create run dir");
        fs::create_dir(run_dir.join("nested")).expect("should create nested dir");
        fs::write(run_dir.join("nested/report.txt"), "").expect("should write report");
        fs::write(run_dir.join("other.bin"), "").expect("should write archived file");

        let filestructure = FileStructure {
            name: "test".to_owned(),
            ignore_paths: HashSet::new(),
            ignore_globs: Vec::new(),
            checkout_paths: HashSet::from([PathBuf::from("nested/report.txt")]),
            checkout_globs: Vec::new(),
            completion_file_globs: vec![Pattern::new("complete.txt").unwrap()],
        };

        let files = classify_run_files(&canonical_dir(&run_dir), &filestructure)
            .expect("classification should succeed");

        assert!(
            files
                .checkout
                .iter()
                .any(|path| path.as_ref() == Path::new("nested/report.txt"))
        );
        assert!(
            files
                .archived
                .iter()
                .any(|path| path.as_ref() == Path::new("other.bin"))
        );
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn archive_dir_conflict_detects_archive_names_and_tar_variants() {
        assert!(archive_dir_name_conflicts(
            "sequencer-sync-archive".as_ref()
        ));
        assert!(archive_dir_name_conflicts(
            "sequencer-sync-archive.tar".as_ref()
        ));
        assert!(archive_dir_name_conflicts(
            "sequencer-sync-archive.tar.gz".as_ref()
        ));
        assert!(!archive_dir_name_conflicts(
            "sequencer-sync-archive-extra".as_ref()
        ));
    }

    #[test]
    fn staging_run_dir_keeps_original_name_and_adds_suffix() {
        let run_dir = segment("run-001");
        let staging = staging_run_dir_segment(&run_dir).into_inner();

        assert!(staging.starts_with("run-001-"));
        assert_ne!(staging, "run-001");
    }

    #[test]
    fn transfer_renames_timestamped_staging_dir_to_plain_landing_dir() {
        let tempdir = make_temp_dir();
        let source = tempdir.join("source");
        let staging = tempdir.join("staging");
        let landing = tempdir.join("landing");
        let run = source.join("run-001");
        for dir in [&source, &staging, &landing, &run] {
            fs::create_dir(dir).expect("should create fixture dir");
        }

        transfer_run_to_landing_zone(
            &segment("run-001"),
            &canonical_dir(&run),
            &canonical_dir(&staging),
            &canonical_dir(&landing),
            ClassifiedFiles::default(),
            false,
        )
        .expect("transfer should succeed");

        assert!(landing.join("run-001").is_dir());
        assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);
        cleanup_temp_dir(&tempdir);
    }
}
