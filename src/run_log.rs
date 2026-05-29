use std::fs::OpenOptions;
use std::io::Write;

use chrono::Local;

use crate::{
    UserError,
    paths::{CanonicalChildFileBuf, CanonicalDirBuf, NormalPathSegment},
};

// Human-readable log. Two files: Full log, and latest attempted transfer log.
pub struct RunLog {
    /// Absolute path to the append-only log that accumulates across all runs.
    full_log_path: CanonicalChildFileBuf,
    /// Absolute path to the log that is overwritten only once a real transfer
    /// attempt starts. Runs that do not start a transfer append here instead.
    latest_log_path: CanonicalChildFileBuf,
    /// Lines buffered for the latest log until we know whether this run should
    /// append to the existing latest log or replace it.
    pending_latest_lines: Vec<String>,
    /// Whether this run has claimed the latest log as the latest attempted
    /// transfer. Once set, subsequent lines append directly.
    latest_started: bool,
    /// Set to true if any call to `log()` failed during this run.
    had_error: bool,
}

impl RunLog {
    pub fn new(logdir: &CanonicalDirBuf) -> Result<Self, UserError> {
        Ok(Self {
            full_log_path: logdir
                .join_file_name(NormalPathSegment::new("sequencer-sync.log".as_ref()).unwrap())?,
            latest_log_path: logdir.join_file_name(
                NormalPathSegment::new("sequencer-sync-latest.log".as_ref()).unwrap(),
            )?,
            pending_latest_lines: Vec::new(),
            latest_started: false,
            had_error: false,
        })
    }

    /// Returns true if a non-fatal error was recorded during this run.
    pub fn had_error(&self) -> bool {
        self.had_error
    }

    /// Log an informational message to the run log files and at `info` level.
    pub fn info(&mut self, message: &str) {
        log::info!("{message}");
        self.write(message);
    }

    /// Log an error to the run log files and at `error` level, and set the
    /// had_error flag to cause a non-zero exit code.
    pub fn error(&mut self, message: &str) {
        log::error!("{message}");
        self.write(message);
        self.had_error = true;
    }

    /// Mark this run as the latest attempted transfer. Buffered lines for this
    /// run replace the existing latest log, and subsequent lines append.
    pub fn start_latest_attempt(&mut self) {
        if let Err(error) = self.try_start_latest_attempt() {
            log::warn!("Failed to update latest run log: {error}");
            self.had_error = true;
        }
    }

    fn try_start_latest_attempt(&mut self) -> std::io::Result<()> {
        if self.latest_started {
            return Ok(());
        }

        if self.pending_latest_lines.is_empty() {
            self.latest_started = true;
            return Ok(());
        }

        let mut latest = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.latest_log_path)?;
        for line in &self.pending_latest_lines {
            latest.write_all(line.as_bytes())?;
        }
        latest.sync_all()?;

        self.pending_latest_lines.clear();
        self.latest_started = true;
        Ok(())
    }

    /// Flush any buffered lines by appending them to the existing latest log.
    /// This is used for noteworthy runs that did not actually start a transfer.
    pub fn finish(&mut self) {
        if let Err(error) = self.try_finish() {
            log::warn!("Failed to finalize latest run log: {error}");
            self.had_error = true;
        }
    }

    fn try_finish(&mut self) -> std::io::Result<()> {
        if self.latest_started || self.pending_latest_lines.is_empty() {
            return Ok(());
        }

        let mut latest = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.latest_log_path)?;
        for line in &self.pending_latest_lines {
            latest.write_all(line.as_bytes())?;
        }
        latest.sync_all()?;

        self.pending_latest_lines.clear();
        Ok(())
    }

    /// Write a message to the log files. If writing fails, emit a warning and
    /// mark this RunLog as having encountered an error.
    fn write(&mut self, message: &str) {
        if let Err(error) = self.try_write(message) {
            log::warn!("Failed to write to run log: {error}");
            self.had_error = true;
        }
    }

    fn try_write(&mut self, message: &str) -> std::io::Result<()> {
        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
        let line = format!("{timestamp}: {message}\n");

        self.append_line(&self.full_log_path, &line)?;

        if self.latest_started {
            self.append_line(&self.latest_log_path, &line)?;
        } else {
            self.pending_latest_lines.push(line);
        }

        Ok(())
    }

    fn append_line(&self, path: &CanonicalChildFileBuf, line: &str) -> std::io::Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(line.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }
}

impl Drop for RunLog {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::paths::CanonicalDirBuf;

    use super::RunLog;

    static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    fn make_temp_dir() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let unique_id = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sequencer-sync-run-log-test-{}-{timestamp}-{unique_id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("should create temp dir");
        path
    }

    fn canonical_temp_dir(path: &Path) -> CanonicalDirBuf {
        CanonicalDirBuf::from_absolute(path, "test temp dir").expect("temp dir should resolve")
    }

    fn cleanup_temp_dir(path: &Path) {
        fs::remove_dir_all(path).expect("should remove temp dir");
    }

    #[test]
    fn creates_log_files_on_first_write() {
        let tempdir = make_temp_dir();
        let logdir = canonical_temp_dir(&tempdir);
        let mut log = RunLog::new(&logdir).expect("run log should initialize");

        log.info("test message");
        log.finish();

        let full = fs::read_to_string(tempdir.join("sequencer-sync.log")).unwrap();
        assert!(full.contains("test message"));
        let latest = fs::read_to_string(tempdir.join("sequencer-sync-latest.log")).unwrap();
        assert!(latest.contains("test message"));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn latest_log_is_truncated_on_new_attempted_transfer() {
        let tempdir = make_temp_dir();
        let logdir = canonical_temp_dir(&tempdir);

        {
            let mut log = RunLog::new(&logdir).expect("run log should initialize");
            log.info("first run message");
            log.start_latest_attempt();
        }

        {
            let mut log = RunLog::new(&logdir).expect("run log should initialize");
            log.info("second run message");
            log.start_latest_attempt();
        }

        let full = fs::read_to_string(tempdir.join("sequencer-sync.log")).unwrap();
        assert!(full.contains("first run message"));
        assert!(full.contains("second run message"));
        assert_eq!(full.lines().count(), 2);

        let latest = fs::read_to_string(tempdir.join("sequencer-sync-latest.log")).unwrap();
        assert!(!latest.contains("first run message"));
        assert!(latest.contains("second run message"));
        assert_eq!(latest.lines().count(), 1);

        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn multiple_messages_in_one_run_append_to_latest() {
        let tempdir = make_temp_dir();
        let logdir = canonical_temp_dir(&tempdir);
        let mut log = RunLog::new(&logdir).expect("run log should initialize");

        log.info("message one");
        log.info("message two");
        log.finish();

        let latest = fs::read_to_string(tempdir.join("sequencer-sync-latest.log")).unwrap();
        assert!(latest.contains("message one"));
        assert!(latest.contains("message two"));
        assert_eq!(latest.lines().count(), 2);

        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn no_op_run_does_not_create_files() {
        let tempdir = make_temp_dir();
        let logdir = canonical_temp_dir(&tempdir);
        let _log = RunLog::new(&logdir).expect("run log should initialize");

        assert!(!tempdir.join("sequencer-sync.log").exists());
        assert!(!tempdir.join("sequencer-sync-latest.log").exists());
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn non_transfer_run_appends_to_existing_latest_log() {
        let tempdir = make_temp_dir();
        let logdir = canonical_temp_dir(&tempdir);

        {
            let mut log = RunLog::new(&logdir).expect("run log should initialize");
            log.info("first attempted transfer");
            log.start_latest_attempt();
        }

        {
            let mut log = RunLog::new(&logdir).expect("run log should initialize");
            log.info("file lock already held");
            log.finish();
        }

        let latest = fs::read_to_string(tempdir.join("sequencer-sync-latest.log")).unwrap();
        assert!(latest.contains("first attempted transfer"));
        assert!(latest.contains("file lock already held"));

        cleanup_temp_dir(&tempdir);
    }
}
