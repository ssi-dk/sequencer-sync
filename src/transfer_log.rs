use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const TRANSFER_LOG_FILE_NAME: &str = "transferred-directories.jsonl";

pub struct TransferLog {
    path: PathBuf,
    /// Maps directory key to whether the transfer succeeded.
    transferred_directories: HashMap<PathBuf, bool>,
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
}

fn default_succeeded() -> bool {
    true
}

impl TransferLog {
    pub fn load(flockdir: &Path) -> Result<Self, TransferLogError> {
        let path = transfer_log_path(flockdir);
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
        let mut transferred_directories = HashMap::new();

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

            // Later entries override earlier ones (e.g. a retry after failure)
            transferred_directories.insert(record.directory.clone(), record.succeeded);
        }

        Ok(Self {
            path,
            transferred_directories,
            file: None,
        })
    }

    /// Returns whether this directory should be skipped.
    /// A directory is skipped if it was previously transferred successfully,
    /// or if it failed and `retry_failed` is false.
    pub fn should_skip(&self, directory: &Path, retry_failed: bool) -> bool {
        self.transferred_directories
            .get(directory)
            .is_some_and(|&succeeded| succeeded || !retry_failed)
    }

    /// Returns true if the directory is in the log with a failed transfer.
    pub fn previously_failed(&self, directory: &Path) -> bool {
        self.transferred_directories
            .get(directory)
            .is_some_and(|&succeeded| !succeeded)
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

        self.transferred_directories.insert(directory, succeeded);
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

pub fn transfer_log_path(flockdir: &Path) -> PathBuf {
    flockdir.join(TRANSFER_LOG_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{TransferLog, TransferLogError, relative_directory_key, transfer_log_path};

    #[test]
    fn missing_log_loads_empty_state() {
        let tempdir = make_temp_dir();

        let log = TransferLog::load(&tempdir).expect("missing log should load");

        assert!(!log.should_skip(Path::new("run-001"), false));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn loads_distinct_directory_records() {
        let tempdir = make_temp_dir();
        fs::write(
            transfer_log_path(&tempdir),
            concat!(
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T10:00:00Z\"}\n",
                "{\"directory\":\"run-002\",\"transferred_at\":\"2026-03-13T11:00:00Z\"}\n"
            ),
        )
        .expect("should write log fixture");

        let log = TransferLog::load(&tempdir).expect("log should parse");

        assert!(log.should_skip(Path::new("run-001"), false));
        assert!(log.should_skip(Path::new("run-002"), false));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn duplicate_directory_entries_are_ignored() {
        let tempdir = make_temp_dir();
        fs::write(
            transfer_log_path(&tempdir),
            concat!(
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T10:00:00Z\"}\n",
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T11:00:00Z\"}\n"
            ),
        )
        .expect("should write log fixture");

        let _log = TransferLog::load(&tempdir).expect("duplicate directory should not error");
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn malformed_json_line_reports_line_number() {
        let tempdir = make_temp_dir();
        fs::write(
            transfer_log_path(&tempdir),
            concat!(
                "{\"directory\":\"run-001\",\"transferred_at\":\"2026-03-13T10:00:00Z\"}\n",
                "{not-json}\n"
            ),
        )
        .expect("should write log fixture");

        let error = TransferLog::load(&tempdir).expect_err("malformed json should error");

        assert!(matches!(error, TransferLogError::Parse { line: 2, .. }));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn append_writes_valid_json_line_and_updates_state() {
        let tempdir = make_temp_dir();
        let mut log = TransferLog::load(&tempdir).expect("missing log should load");

        log.record_transfer(Path::new("run-001"), true)
            .expect("append should succeed");

        assert!(log.should_skip(Path::new("run-001"), false));

        let reloaded = TransferLog::load(&tempdir).expect("reloaded log should parse");
        assert!(reloaded.should_skip(Path::new("run-001"), false));

        let contents =
            fs::read_to_string(transfer_log_path(&tempdir)).expect("should read transfer log");
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains("\"directory\":\"run-001\""));
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
        let path = std::env::temp_dir().join(format!(
            "sequencer-sync-transfer-log-test-{}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("should create temp dir");
        path
    }

    fn cleanup_temp_dir(path: &Path) {
        fs::remove_dir_all(path).expect("should remove temp dir");
    }
}
