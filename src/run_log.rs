use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Local;

pub struct RunLog {
    /// Absolute path to the append-only log that accumulates across all runs.
    full_log_path: PathBuf,
    /// Absolute path to the log that is overwritten on each program invocation.
    latest_log_path: PathBuf,
    /// Whether the latest log has been truncated in this invocation. Reset per
    /// program run, not per directory — the first call to `log()` truncates,
    /// subsequent calls append.
    latest_started: bool,
    /// Set to true if any call to `log()` failed during this run.
    had_error: bool,
}

impl RunLog {
    pub fn new(flockdir: &Path) -> Self {
        Self {
            full_log_path: flockdir.join("sequencer-sync.log"),
            latest_log_path: flockdir.join("sequencer-sync-latest.log"),
            latest_started: false,
            had_error: false,
        }
    }

    /// Returns true if a non-fatal error was recorded during this run.
    pub fn had_error(&self) -> bool {
        self.had_error
    }

    /// Record that a non-fatal error occurred. This will cause the
    /// process to exit with a non-zero status code.
    pub fn record_error(&mut self) {
        self.had_error = true;
    }

    /// Log a message. If writing fails, print a warning to stderr and
    /// mark this RunLog as having encountered an error.
    pub fn log(&mut self, message: &str) {
        if let Err(error) = self.try_write(message) {
            eprintln!("Warning: failed to write to run log: {error}");
            self.had_error = true;
        }
    }

    fn try_write(&mut self, message: &str) -> std::io::Result<()> {
        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
        let line = format!("{timestamp}: {message}\n");

        // Append to full log
        let mut full = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.full_log_path)?;
        full.write_all(line.as_bytes())?;

        // Write to latest log (truncate on first write, append after)
        if self.latest_started {
            let mut latest = OpenOptions::new()
                .append(true)
                .open(&self.latest_log_path)?;
            latest.write_all(line.as_bytes())?;
        } else {
            std::fs::write(&self.latest_log_path, &line)?;
            self.latest_started = true;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::RunLog;

    fn make_temp_dir() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "sequencer-sync-run-log-test-{}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("should create temp dir");
        path
    }

    fn cleanup_temp_dir(path: &Path) {
        fs::remove_dir_all(path).expect("should remove temp dir");
    }

    #[test]
    fn creates_log_files_on_first_write() {
        let tempdir = make_temp_dir();
        let mut log = RunLog::new(&tempdir);

        log.log("test message");

        let full = fs::read_to_string(tempdir.join("sequencer-sync.log")).unwrap();
        assert!(full.contains("test message"));
        let latest = fs::read_to_string(tempdir.join("sequencer-sync-latest.log")).unwrap();
        assert!(latest.contains("test message"));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn latest_log_is_truncated_on_new_run() {
        let tempdir = make_temp_dir();

        // First run
        {
            let mut log = RunLog::new(&tempdir);
            log.log("first run message");
        }

        // Second run
        {
            let mut log = RunLog::new(&tempdir);
            log.log("second run message");
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
        let mut log = RunLog::new(&tempdir);

        log.log("message one");
        log.log("message two");

        let latest = fs::read_to_string(tempdir.join("sequencer-sync-latest.log")).unwrap();
        assert!(latest.contains("message one"));
        assert!(latest.contains("message two"));
        assert_eq!(latest.lines().count(), 2);

        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn no_op_run_does_not_create_files() {
        let tempdir = make_temp_dir();
        let _log = RunLog::new(&tempdir);

        assert!(!tempdir.join("sequencer-sync.log").exists());
        assert!(!tempdir.join("sequencer-sync-latest.log").exists());
        cleanup_temp_dir(&tempdir);
    }
}
