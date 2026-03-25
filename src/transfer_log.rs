use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{TransferAction, TransferReason};

const TRANSFER_LOG_FILE_NAME: &str = "transferred-directories.jsonl";

// Machine readable log in JSONL format. Authorative source of which directories
// have been previosly copied and whether the copy was successful
pub struct TransferLog {
    path: PathBuf,
    /// Maps directory key to its latest transfer state.
    transferred_directories: HashMap<PathBuf, TransferState>,
    /// Lazily opened file handle for appending records.
    file: Option<File>,
}

impl std::fmt::Debug for TransferLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferLog")
            .field("path", &self.path)
            .field("transferred_directories", &self.transferred_directories)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum TransferLogError {
    #[error("failed to read transfer log {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse transfer log {} line {line}: {source}", path.display())]
    Parse {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "failed to parse transfer log timestamp in {} line {line}: {source}",
        path.display()
    )]
    ParseTimestamp {
        path: PathBuf,
        line: usize,
        #[source]
        source: chrono::ParseError,
    },
    #[error("failed to append to transfer log {}: {source}", path.display())]
    Append {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "failed to serialize transfer log record for `{directory}` (likely a non-UTF-8 directory name): {source}"
    )]
    Serialize {
        directory: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "transfer directory {} is not under source directory {}",
        directory.display(),
        source_dir.display()
    )]
    DirectoryOutsideSource {
        source_dir: PathBuf,
        directory: PathBuf,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct TransferRecord {
    directory: PathBuf,
    transferred_at: String, // UTC time, but in String format for serialization
    #[serde(default = "default_succeeded")]
    succeeded: bool,
    redo: bool,
}

fn default_succeeded() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TransferState {
    succeeded: bool,
    redo: bool,
    transferred_at: DateTime<Utc>,
}

impl TransferLog {
    pub fn load(logdir: &Path) -> Result<Self, TransferLogError> {
        let path = transfer_log_path(logdir);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    transferred_directories: HashMap::new(),
                    file: None,
                });
            }
            Err(source) => return Err(TransferLogError::Read { path, source }),
        };

        let reader = BufReader::new(file);
        let mut transferred_directories: HashMap<PathBuf, TransferState> = HashMap::new();

        for (index, line_result) in reader.lines().enumerate() {
            let line = line_result.map_err(|source| TransferLogError::Read {
                path: path.clone(),
                source,
            })?;

            if line.trim().is_empty() {
                continue;
            }

            let line_number = index + 1;
            let record: TransferRecord =
                serde_json::from_str(&line).map_err(|source| TransferLogError::Parse {
                    path: path.clone(),
                    line: line_number,
                    source,
                })?;

            let transferred_at = chrono::DateTime::parse_from_rfc3339(&record.transferred_at)
                .map_err(|source| TransferLogError::ParseTimestamp {
                    path: path.clone(),
                    line: line_number,
                    source,
                })?
                .with_timezone(&Utc);

            let new_state = TransferState {
                succeeded: record.succeeded,
                redo: record.redo,
                transferred_at,
            };

            // The newest timestamp wins; equal timestamps keep the existing state
            // to avoid depending on file order.
            match transferred_directories.get_mut(&record.directory) {
                Some(existing) if new_state.transferred_at > existing.transferred_at => {
                    *existing = new_state;
                }
                Some(_) => {}
                None => {
                    transferred_directories.insert(record.directory.clone(), new_state);
                }
            }
        }

        Ok(Self {
            path,
            transferred_directories,
            file: None,
        })
    }

    pub fn transfer_action(&self, directory: &Path, retry_failed: bool) -> TransferAction {
        match self.transferred_directories.get(directory).copied() {
            None => TransferAction::Tranfer(TransferReason::New),
            Some(TransferState { redo: true, .. }) => TransferAction::Tranfer(TransferReason::Redo),
            Some(TransferState {
                succeeded: true,
                redo: false,
                ..
            }) => TransferAction::Skip(crate::SkipReason::AlreadyTranferred),
            Some(TransferState {
                succeeded: false,
                redo: false,
                ..
            }) if retry_failed => TransferAction::Tranfer(TransferReason::Retry),
            Some(TransferState {
                succeeded: false,
                redo: false,
                ..
            }) => TransferAction::Skip(crate::SkipReason::FailedNoRetry),
        }
    }

    pub fn record_transfer(
        &mut self,
        directory: &Path,
        succeeded: bool,
    ) -> Result<(), TransferLogError> {
        let directory = directory.to_path_buf();

        let record = TransferRecord {
            directory: directory.clone(),
            transferred_at: Utc::now().to_rfc3339(),
            succeeded,
            redo: false,
        };

        let line =
            serde_json::to_string(&record).map_err(|source| TransferLogError::Serialize {
                directory: directory.display().to_string(),
                source,
            })?;

        self.open_for_append()?;
        let file = self.file.as_mut().unwrap();
        let path = &self.path;

        writeln!(file, "{line}").map_err(|source| TransferLogError::Append {
            path: path.clone(),
            source,
        })?;

        file.sync_all().map_err(|source| TransferLogError::Append {
            path: path.clone(),
            source,
        })?;

        self.transferred_directories.insert(
            directory,
            TransferState {
                succeeded,
                redo: false,
                transferred_at: DateTime::<Utc>::MAX_UTC,
            },
        );
        Ok(())
    }

    fn open_for_append(&mut self) -> Result<&mut File, TransferLogError> {
        if self.file.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .map_err(|source| TransferLogError::Append {
                    path: self.path.clone(),
                    source,
                })?;
            self.file = Some(file);
        }
        Ok(self.file.as_mut().unwrap())
    }
}

pub fn relative_directory_key(
    source: &Path,
    directory: &Path,
) -> Result<PathBuf, TransferLogError> {
    directory
        .strip_prefix(source)
        .map(Path::to_path_buf)
        .map_err(|_| TransferLogError::DirectoryOutsideSource {
            source_dir: source.to_path_buf(),
            directory: directory.to_path_buf(),
        })
}

pub fn transfer_log_path(logdir: &Path) -> PathBuf {
    logdir.join(TRANSFER_LOG_FILE_NAME)
}

/// Write a sentinel record to the transfer log if the file is absent or blank.
/// This ensures the file is always present after setup, making it easy to
/// inspect the log format even before any real transfers have occurred.
pub fn initialize_if_absent(logdir: &Path) -> Result<(), TransferLogError> {
    let path = transfer_log_path(logdir);

    let needs_init = match fs::read_to_string(&path) {
        Ok(contents) => contents.trim().is_empty(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(source) => return Err(TransferLogError::Read { path, source }),
    };

    if !needs_init {
        return Ok(());
    }

    let record = TransferRecord {
        directory: PathBuf::from("_sequencer_sync_setup_"),
        transferred_at: Utc::now().to_rfc3339(),
        succeeded: true,
        redo: false,
    };

    let line = serde_json::to_string(&record).map_err(|source| TransferLogError::Serialize {
        directory: record.directory.display().to_string(),
        source,
    })?;

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|source| TransferLogError::Append {
            path: path.clone(),
            source,
        })?;

    writeln!(file, "{line}").map_err(|source| TransferLogError::Append {
        path: path.clone(),
        source,
    })?;
    file.sync_all()
        .map_err(|source| TransferLogError::Append { path, source })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::{SkipReason, TransferAction, TransferReason};

    use super::{TransferLog, TransferLogError, relative_directory_key, transfer_log_path};

    static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn missing_log_loads_empty_state() {
        let tempdir = make_temp_dir();

        let log = TransferLog::load(&tempdir).expect("missing log should load");

        assert!(matches!(
            log.transfer_action(Path::new("run-001"), false),
            TransferAction::Tranfer(TransferReason::New)
        ));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn loads_distinct_directory_records() {
        let tempdir = make_temp_dir();
        fs::write(
            transfer_log_path(&tempdir),
            concat!(
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T10:00:00Z\",\"redo\":false}\n",
                "{\"directory\":\"run-002\",\"transferred_at\":\"2026-03-13T11:00:00Z\",\"redo\":false}\n"
            ),
        )
        .expect("should write log fixture");

        let log = TransferLog::load(&tempdir).expect("log should parse");

        assert!(matches!(
            log.transfer_action(Path::new("run-001"), false),
            TransferAction::Skip(SkipReason::AlreadyTranferred)
        ));
        assert!(matches!(
            log.transfer_action(Path::new("run-002"), false),
            TransferAction::Skip(SkipReason::AlreadyTranferred)
        ));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn duplicate_directory_entries_are_ignored() {
        let tempdir = make_temp_dir();
        fs::write(
            transfer_log_path(&tempdir),
            concat!(
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T10:00:00Z\",\"redo\":false}\n",
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T11:00:00Z\",\"redo\":false}\n"
            ),
        )
        .expect("should write log fixture");

        let _log = TransferLog::load(&tempdir).expect("duplicate directory should not error");
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn later_entries_override_earlier_state_for_same_directory() {
        let tempdir = make_temp_dir();
        fs::write(
            transfer_log_path(&tempdir),
            concat!(
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T10:00:00Z\",\"succeeded\":true,\"redo\":true}\n",
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T11:00:00Z\",\"succeeded\":true,\"redo\":false}\n"
            ),
        )
        .expect("should write log fixture");

        let log = TransferLog::load(&tempdir).expect("duplicate directory should not error");

        assert!(matches!(
            log.transfer_action(Path::new("run-001"), false),
            TransferAction::Skip(SkipReason::AlreadyTranferred)
        ));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn line_order_does_not_override_newer_timestamp() {
        let tempdir = make_temp_dir();
        fs::write(
            transfer_log_path(&tempdir),
            concat!(
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T11:00:00Z\",\"succeeded\":true,\"redo\":false}\n",
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T10:00:00Z\",\"succeeded\":true,\"redo\":true}\n"
            ),
        )
        .expect("should write log fixture");

        let log = TransferLog::load(&tempdir).expect("duplicate directory should not error");

        assert!(matches!(
            log.transfer_action(Path::new("run-001"), false),
            TransferAction::Skip(SkipReason::AlreadyTranferred)
        ));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn equal_timestamps_keep_existing_state() {
        let tempdir = make_temp_dir();
        fs::write(
            transfer_log_path(&tempdir),
            concat!(
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T10:00:00Z\",\"succeeded\":true,\"redo\":false}\n",
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T10:00:00Z\",\"succeeded\":true,\"redo\":true}\n"
            ),
        )
        .expect("should write log fixture");

        let log = TransferLog::load(&tempdir).expect("duplicate directory should not error");

        assert!(matches!(
            log.transfer_action(Path::new("run-001"), false),
            TransferAction::Skip(SkipReason::AlreadyTranferred)
        ));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn malformed_json_line_reports_line_number() {
        let tempdir = make_temp_dir();
        fs::write(
            transfer_log_path(&tempdir),
            concat!(
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T10:00:00Z\",\"redo\":false}\n",
                "{not-json}\n"
            ),
        )
        .expect("should write log fixture");

        let error = TransferLog::load(&tempdir).expect_err("malformed json should error");

        assert!(matches!(error, TransferLogError::Parse { line: 2, .. }));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn malformed_timestamp_reports_line_number() {
        let tempdir = make_temp_dir();
        fs::write(
            transfer_log_path(&tempdir),
            "{\"directory\":\"run-001\",\"transferred_at\":\"not-a-timestamp\",\"redo\":false}\n",
        )
        .expect("should write log fixture");

        let error = TransferLog::load(&tempdir).expect_err("bad timestamp should error");

        assert!(matches!(
            error,
            TransferLogError::ParseTimestamp { line: 1, .. }
        ));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn append_writes_valid_json_line_and_updates_state() {
        let tempdir = make_temp_dir();
        let mut log = TransferLog::load(&tempdir).expect("missing log should load");

        log.record_transfer(Path::new("run-001"), true)
            .expect("append should succeed");

        assert!(matches!(
            log.transfer_action(Path::new("run-001"), false),
            TransferAction::Skip(SkipReason::AlreadyTranferred)
        ));

        let reloaded = TransferLog::load(&tempdir).expect("reloaded log should parse");
        assert!(matches!(
            reloaded.transfer_action(Path::new("run-001"), false),
            TransferAction::Skip(SkipReason::AlreadyTranferred)
        ));

        let contents =
            fs::read_to_string(transfer_log_path(&tempdir)).expect("should read transfer log");
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains("\"directory\":\"run-001\""));
        assert!(contents.contains("\"redo\":false"));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn computes_relative_directory_key() {
        let source = Path::new("/var/lib/sequencer/data");
        let directory = Path::new("/var/lib/sequencer/data/run-001");

        let key = relative_directory_key(source, directory).expect("key should be relative");

        assert_eq!(key, PathBuf::from("run-001"));
    }

    #[test]
    fn rejects_directory_outside_source() {
        let source = Path::new("/var/lib/sequencer/data");
        let directory = Path::new("/elsewhere/run-001");

        let error =
            relative_directory_key(source, directory).expect_err("outside source should error");

        assert!(matches!(
            error,
            TransferLogError::DirectoryOutsideSource { .. }
        ));
    }

    #[test]
    fn computes_transfer_log_path() {
        let flockdir = Path::new("/var/lib/sequencer/flock");

        assert_eq!(
            transfer_log_path(flockdir),
            Path::new("/var/lib/sequencer/flock/transferred-directories.jsonl")
        );
    }

    fn make_temp_dir() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let unique_id = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sequencer-sync-transfer-log-test-{}-{timestamp}-{unique_id}",
            std::process::id(),
        ));
        fs::create_dir(&path).expect("should create temp dir");
        path
    }

    fn cleanup_temp_dir(path: &Path) {
        fs::remove_dir_all(path).expect("should remove temp dir");
    }
}
