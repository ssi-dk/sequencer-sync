use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

struct TestFixture {
    root: PathBuf,
}

impl TestFixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "sequencer-sync-integration-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn mkdir(&self, relative: &str) -> PathBuf {
        let p = self.path(relative);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_file(&self, relative: &str, contents: &str) {
        let p = self.path(relative);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, contents).unwrap();
    }

    fn write_nanopore_config(&self) -> PathBuf {
        let config = format!(
            r#"flockdir: "{flockdir}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^ONT_WGS_"
    landing_zone:
      kind: local
      path: "{landing_core}"
    exclude: []
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    landing_zone:
      kind: local
      path: "{landing_other}"
    exclude: []
    completion_file_globs:
      - "report*.html"
"#,
            flockdir = self.path("flockdir").display(),
            logdir = self.path("logdir").display(),
            source = self.path("nanopore-source").display(),
            landing_core = self.path("nanopore-landing-core").display(),
            landing_other = self.path("nanopore-landing-other").display(),
        );
        let path = self.path("nanopore.yaml");
        fs::write(&path, config).unwrap();
        path
    }

    fn write_nextseq_config(&self) -> PathBuf {
        let config = format!(
            r#"flockdir: "{flockdir}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^\\d{{6}}_"
    landing_zone:
      kind: local
      path: "{landing}"
    exclude: []
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
            flockdir = self.path("flockdir").display(),
            logdir = self.path("logdir").display(),
            source = self.path("nextseq-source").display(),
            landing = self.path("nextseq-landing").display(),
        );
        let path = self.path("nextseq.yaml");
        fs::write(&path, config).unwrap();
        path
    }

    /// Set up directories for a nanopore test scenario.
    fn setup_nanopore(&self) -> PathBuf {
        self.mkdir("flockdir");
        self.mkdir("logdir");
        self.mkdir("nanopore-source");
        self.mkdir("nanopore-landing-core");
        self.mkdir("nanopore-landing-other");

        // Complete runs
        self.mkdir("nanopore-source/ONT_WGS_run1");
        self.write_file("nanopore-source/ONT_WGS_run1/report_final.html", "report");
        self.write_file("nanopore-source/ONT_WGS_run1/data.txt", "data");

        self.mkdir("nanopore-source/ONT_raw_run2");
        self.write_file("nanopore-source/ONT_raw_run2/report_final.html", "report");
        self.write_file("nanopore-source/ONT_raw_run2/data.txt", "data");

        self.write_nanopore_config()
    }

    /// Set up directories for a nextseq test scenario.
    fn setup_nextseq(&self) -> PathBuf {
        self.mkdir("flockdir");
        self.mkdir("logdir");
        self.mkdir("nextseq-source");
        self.mkdir("nextseq-landing");

        // Complete run
        self.mkdir("nextseq-source/240101_NB001");
        self.mkdir("nextseq-source/240101_NB001/PrimaryAnalysisMetrics");
        self.write_file(
            "nextseq-source/240101_NB001/PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv",
            "metrics",
        );
        self.write_file("nextseq-source/240101_NB001/data.txt", "data");

        // Incomplete run (no completion file)
        self.mkdir("nextseq-source/240202_NB002");
        self.write_file("nextseq-source/240202_NB002/data.txt", "data");

        self.write_nextseq_config()
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn cmd() -> Command {
    Command::cargo_bin("sequencer-sync").unwrap()
}

fn transfer_log_path(fixture: &TestFixture) -> PathBuf {
    fixture.path("logdir/transferred-directories.jsonl")
}

fn read_transfer_log(fixture: &TestFixture) -> String {
    let path = transfer_log_path(fixture);
    if path.exists() {
        fs::read_to_string(&path).unwrap()
    } else {
        String::new()
    }
}

fn read_run_log(fixture: &TestFixture) -> String {
    let path = fixture.path("logdir/sequencer-sync.log");
    if path.exists() {
        fs::read_to_string(&path).unwrap()
    } else {
        String::new()
    }
}

fn read_latest_log(fixture: &TestFixture) -> String {
    let path = fixture.path("logdir/sequencer-sync-latest.log");
    if path.exists() {
        fs::read_to_string(&path).unwrap()
    } else {
        String::new()
    }
}

fn transfer_log_line_count(fixture: &TestFixture) -> usize {
    read_transfer_log(fixture)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

#[test]
fn run_fails_missing_flockdir() {
    let fixture = TestFixture::new("missing-flockdir");
    // Create source and landing zones but NOT flockdir
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");
    fixture.mkdir("nanopore-landing-other");
    let config_path = fixture.write_nanopore_config();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .failure();
}

#[test]
fn run_fails_nonexistent_landing_zone() {
    let fixture = TestFixture::new("nonexistent-landing");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");

    // landing-other points to a nonexistent path — caught at config load time.
    let config = format!(
        r#"flockdir: "{flockdir}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^ONT_WGS_"
    landing_zone:
      kind: local
      path: "{landing_core}"
    exclude: []
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    landing_zone:
      kind: local
      path: "{root}/no-such-parent/nanopore-landing-other"
    exclude: []
    completion_file_globs:
      - "report*.html"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nanopore-source").display(),
        landing_core = fixture.path("nanopore-landing-core").display(),
        root = fixture.root.display(),
    );
    let config_path = fixture.path("nanopore.yaml");
    fs::write(&config_path, config).unwrap();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .failure();
}

#[test]
fn run_fails_missing_transfer_destination_directory() {
    let fixture = TestFixture::new("missing-transfer-destination");
    let config_path = fixture.setup_nanopore();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .failure();

    assert!(!fixture.path("nanopore-landing-core/ONT_WGS_run1").exists());
    assert_eq!(transfer_log_line_count(&fixture), 0);
}

#[cfg(unix)]
#[test]
fn local_transfer_preserves_non_utf8_run_name() {
    let fixture = TestFixture::new("non-utf8-run-name");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("source");
    fixture.mkdir("landing");

    let run_name = std::ffi::OsStr::from_bytes(b"ONT_\xff_run");
    let run_dir = fixture.path("source").join(run_name);
    if let Err(error) = fs::create_dir_all(&run_dir) {
        eprintln!(
            "skipping non-UTF-8 path transfer test: filesystem/sandbox refused test path: {error}"
        );
        return;
    }
    fs::create_dir_all(fixture.path("landing").join(run_name)).unwrap();
    fs::write(run_dir.join("report_final.html"), "report").unwrap();
    fs::write(run_dir.join("data.txt"), "payload").unwrap();

    let config = format!(
        r#"flockdir: "{flockdir}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^ONT_"
    landing_zone:
      kind: local
      path: "{landing}"
    exclude: []
    completion_file_globs:
      - "report*.html"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("source").display(),
        landing = fixture.path("landing").display(),
    );
    let config_path = fixture.path("config.yaml");
    fs::write(&config_path, config).unwrap();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let landed = fixture.path("landing").join(run_name).join("data.txt");
    assert!(landed.exists(), "expected file at {}", landed.display());
}

#[test]
fn nanopore_transfers_complete_runs() {
    let fixture = TestFixture::new("nanopore-complete");
    let config_path = fixture.setup_nanopore();
    fixture.mkdir("nanopore-landing-core/ONT_WGS_run1");
    fixture.mkdir("nanopore-landing-other/ONT_raw_run2");

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    // ONT_WGS_run1 should be in landing-core
    assert!(
        fixture
            .path("nanopore-landing-core/ONT_WGS_run1/data.txt")
            .exists()
    );

    // ONT_raw_run2 should be in landing-other
    assert!(
        fixture
            .path("nanopore-landing-other/ONT_raw_run2/data.txt")
            .exists()
    );

    // Transfer log should have 2 succeeded entries
    let log = read_transfer_log(&fixture);
    let succeeded_count = log.matches("\"succeeded\":true").count();
    assert_eq!(succeeded_count, 2);

    // Run log should have 2 "Transferred" lines
    let run_log = read_run_log(&fixture);
    let transferred_count = run_log.matches("Transferred").count();
    assert_eq!(transferred_count, 2);

    // Transfer marker files should exist in the transferred directories
    assert!(
        fixture
            .path("nanopore-landing-core/ONT_WGS_run1/transfer_successful.txt")
            .exists()
    );
    assert!(
        fixture
            .path("nanopore-landing-other/ONT_raw_run2/transfer_successful.txt")
            .exists()
    );
}

#[test]
fn nextseq_skips_incomplete_runs() {
    let fixture = TestFixture::new("nextseq-incomplete");
    let config_path = fixture.setup_nextseq();
    fixture.mkdir("nextseq-landing/240101_NB001");

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    // Complete run transferred
    assert!(
        fixture
            .path("nextseq-landing/240101_NB001/data.txt")
            .exists()
    );

    // Incomplete run NOT transferred
    assert!(!fixture.path("nextseq-landing/240202_NB002").exists());

    // Transfer marker present for complete run
    assert!(
        fixture
            .path("nextseq-landing/240101_NB001/transfer_successful.txt")
            .exists()
    );

    // Transfer log has 1 entry
    assert_eq!(transfer_log_line_count(&fixture), 1);
}

#[test]
fn nextseq_ignore_incomplete_transfers_all() {
    let fixture = TestFixture::new("nextseq-ignore-incomplete");
    let config_path = fixture.setup_nextseq();
    fixture.mkdir("nextseq-landing/240101_NB001");
    fixture.mkdir("nextseq-landing/240202_NB002");

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .args(["--transfer-incomplete"])
        .assert()
        .success();

    // Both runs transferred
    assert!(
        fixture
            .path("nextseq-landing/240101_NB001/data.txt")
            .exists()
    );
    assert!(
        fixture
            .path("nextseq-landing/240202_NB002/data.txt")
            .exists()
    );

    assert_eq!(transfer_log_line_count(&fixture), 2);
}

#[test]
fn nextseq_requires_all_completion_globs() {
    let fixture = TestFixture::new("nextseq-all-completion-globs");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nextseq-source");
    fixture.mkdir("nextseq-landing");
    fixture.mkdir("nextseq-source/240101_NB001");
    fixture.mkdir("nextseq-source/240101_NB001/PrimaryAnalysisMetrics");
    fixture.write_file(
        "nextseq-source/240101_NB001/PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv",
        "metrics",
    );
    fixture.write_file("nextseq-source/240101_NB001/data.txt", "data");

    let config = format!(
        r#"flockdir: "{flockdir}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^\\d{{6}}_"
    landing_zone:
      kind: local
      path: "{landing}"
    exclude: []
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "Analysis/*/Data/fastq/Logs/FastqComplete.txt"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nextseq-source").display(),
        landing = fixture.path("nextseq-landing").display(),
    );
    let config_path = fixture.path("nextseq.yaml");
    fs::write(&config_path, config).unwrap();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    assert!(!fixture.path("nextseq-landing/240101_NB001").exists());
    assert_eq!(transfer_log_line_count(&fixture), 0);
}

#[test]
fn second_run_is_noop() {
    let fixture = TestFixture::new("second-noop");
    let config_path = fixture.setup_nanopore();
    fixture.mkdir("nanopore-landing-core/ONT_WGS_run1");
    fixture.mkdir("nanopore-landing-other/ONT_raw_run2");

    // First run
    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let log_after_first = read_transfer_log(&fixture);
    assert_eq!(transfer_log_line_count(&fixture), 2);
    let run_log_after_first = read_run_log(&fixture);
    let latest_after_first = read_latest_log(&fixture);

    // Second run
    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    // Transfer log unchanged
    let log_after_second = read_transfer_log(&fixture);
    assert_eq!(log_after_first, log_after_second);

    // Full run log unchanged (true no-op doesn't log)
    let run_log_after_second = read_run_log(&fixture);
    assert_eq!(run_log_after_first, run_log_after_second);

    // Latest log unchanged (no-op doesn't write, so file from first run persists)
    let latest_after_second = read_latest_log(&fixture);
    assert_eq!(latest_after_first, latest_after_second);
}

#[test]
fn incomplete_only_run_appends_to_latest_log() {
    let fixture = TestFixture::new("incomplete-only-appends-latest");
    let config_path = fixture.setup_nextseq();
    fixture.mkdir("nextseq-landing/240101_NB001");

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let latest_after_first = read_latest_log(&fixture);
    assert!(latest_after_first.contains("Transferred new directory 240101_NB001"));

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let latest_after_second = read_latest_log(&fixture);
    assert!(latest_after_second.contains("Transferred new directory 240101_NB001"));
    assert!(latest_after_second.contains("Skipped incomplete directory 240202_NB002"));
}

#[test]
fn retry_failed() {
    let fixture = TestFixture::new("retry-failed");
    let config_path = fixture.setup_nextseq();
    fixture.mkdir("nextseq-landing/240101_NB001");

    // Write a failed entry to the transfer log
    let log_path = transfer_log_path(&fixture);
    fs::write(
        &log_path,
        "{\"directory\":\"240101_NB001\",\"transferred_at\":\"2026-01-01T00:00:00Z\",\"succeeded\":false,\"redo\":false}\n",
    )
    .unwrap();

    // Run without --retry-failed: should not retry
    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    // Only the incomplete run might be skipped, but the failed one should also be skipped
    // The transfer log should still have the original entry only (no new transfer of 240101)
    assert!(
        !fixture
            .path("nextseq-landing/240101_NB001/data.txt")
            .exists()
    );

    // Now run with --retry-failed
    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .args(["--retry-failed"])
        .assert()
        .success();

    // Directory should now be transferred
    assert!(
        fixture
            .path("nextseq-landing/240101_NB001/data.txt")
            .exists()
    );

    // Transfer log should have a new succeeded entry
    let log = read_transfer_log(&fixture);
    assert!(log.contains("\"succeeded\":true"));

    // Run log should indicate this was a retry
    let run_log = read_run_log(&fixture);
    assert!(run_log.contains("Transferred previously failed transfer 240101_NB001"));
}

#[test]
fn redo_true_retransfers_succeeded_directory() {
    let fixture = TestFixture::new("redo-true");
    let config_path = fixture.setup_nextseq();
    fixture.mkdir("nextseq-landing/240101_NB001");

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let log_path = transfer_log_path(&fixture);
    fs::write(
        &log_path,
        concat!(
            "{\"directory\":\"240101_NB001\",\"transferred_at\":\"2026-01-01T00:00:00Z\",\"succeeded\":true,\"redo\":false}\n",
            "{\"directory\":\"240101_NB001\",\"transferred_at\":\"2026-01-01T00:00:01Z\",\"succeeded\":true,\"redo\":true}\n"
        ),
    )
    .unwrap();

    let transferred_file = fixture.path("nextseq-landing/240101_NB001/data.txt");
    fs::remove_file(&transferred_file).unwrap();
    assert!(!transferred_file.exists());

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    assert!(transferred_file.exists());

    let log = read_transfer_log(&fixture);
    assert!(log.contains("\"redo\":false"));
    assert!(log.contains("\"succeeded\":true"));

    let run_log = read_run_log(&fixture);
    assert!(run_log.contains("Transferred directory marked for redo 240101_NB001"));
}

#[test]
fn setup_with_skip_ssh_check() {
    let fixture = TestFixture::new("setup-skip-ssh");
    let config_path = fixture.setup_nanopore();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check"])
        .assert()
        .success();

    // Cron file should be created
    assert!(fixture.path("logdir/sequencer-sync.cron").exists());
}

#[test]
fn setup_fails_malformed_existing_transfer_log() {
    let fixture = TestFixture::new("setup-malformed-transfer-log");
    let config_path = fixture.setup_nanopore();
    fs::write(transfer_log_path(&fixture), "{not-json}\n").unwrap();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check"])
        .assert()
        .failure();
}

#[test]
fn dry_run_prints_plan_without_copying() {
    let fixture = TestFixture::new("dry-run");
    let config_path = fixture.setup_nanopore();

    let output = cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .args(["--dry-run"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should print both directories with their destinations
    assert!(stdout.contains("ONT_WGS_run1"));
    assert!(stdout.contains("nanopore-landing-core"));
    assert!(stdout.contains("ONT_raw_run2"));
    assert!(stdout.contains("nanopore-landing-other"));

    // Nothing should actually be copied
    assert!(!fixture.path("nanopore-landing-core/ONT_WGS_run1").exists());
    assert!(!fixture.path("nanopore-landing-other/ONT_raw_run2").exists());

    // Transfer log should be empty (no transfers recorded)
    assert_eq!(transfer_log_line_count(&fixture), 0);
    assert_eq!(read_run_log(&fixture), "");
    assert_eq!(read_latest_log(&fixture), "");
}

#[test]
fn dry_run_shows_exclude_patterns() {
    let fixture = TestFixture::new("dry-run-exclude");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");
    fixture.mkdir("nanopore-landing-other");

    fixture.mkdir("nanopore-source/ONT_WGS_run1");
    fixture.write_file("nanopore-source/ONT_WGS_run1/report_final.html", "report");

    // Config with exclude patterns
    let config = format!(
        r#"flockdir: "{flockdir}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^ONT_WGS_"
    landing_zone:
      kind: local
      path: "{landing_core}"
    exclude: ["/Data", "/AutoCenter"]
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    landing_zone:
      kind: local
      path: "{landing_other}"
    exclude: []
    completion_file_globs:
      - "report*.html"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nanopore-source").display(),
        landing_core = fixture.path("nanopore-landing-core").display(),
        landing_other = fixture.path("nanopore-landing-other").display(),
    );
    let config_path = fixture.path("nanopore.yaml");
    fs::write(&config_path, config).unwrap();

    let output = cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .args(["--dry-run"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("exclude: /Data"));
    assert!(stdout.contains("exclude: /AutoCenter"));
}

#[test]
fn rsync_excludes_patterns_relative_to_transferred_directory() {
    let fixture = TestFixture::new("rsync-exclude-patterns");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");
    fixture.mkdir("nanopore-landing-other");
    fixture.mkdir("nanopore-landing-core/ONT_WGS_run1");

    fixture.mkdir("nanopore-source/ONT_WGS_run1");
    fixture.write_file("nanopore-source/ONT_WGS_run1/report_final.html", "report");
    fixture.write_file("nanopore-source/ONT_WGS_run1/data.txt", "keep");
    fixture.write_file(
        "nanopore-source/ONT_WGS_run1/Data/top_level.txt",
        "exclude at root",
    );
    fixture.write_file(
        "nanopore-source/ONT_WGS_run1/other/Data/nested.txt",
        "keep nested data",
    );
    fixture.write_file(
        "nanopore-source/ONT_WGS_run1/AutoCenter/metrics.txt",
        "exclude at root",
    );

    let config = format!(
        r#"flockdir: "{flockdir}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^ONT_WGS_"
    landing_zone:
      kind: local
      path: "{landing_core}"
    exclude: ["/Data", "/AutoCenter"]
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    landing_zone:
      kind: local
      path: "{landing_other}"
    exclude: []
    completion_file_globs:
      - "report*.html"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nanopore-source").display(),
        landing_core = fixture.path("nanopore-landing-core").display(),
        landing_other = fixture.path("nanopore-landing-other").display(),
    );
    let config_path = fixture.path("nanopore.yaml");
    fs::write(&config_path, config).unwrap();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    assert!(
        fixture
            .path("nanopore-landing-core/ONT_WGS_run1/data.txt")
            .exists()
    );
    assert!(
        fixture
            .path("nanopore-landing-core/ONT_WGS_run1/report_final.html")
            .exists()
    );
    assert!(
        !fixture
            .path("nanopore-landing-core/ONT_WGS_run1/Data")
            .exists()
    );
    assert!(
        !fixture
            .path("nanopore-landing-core/ONT_WGS_run1/AutoCenter")
            .exists()
    );
    assert!(
        fixture
            .path("nanopore-landing-core/ONT_WGS_run1/other/Data/nested.txt")
            .exists()
    );
}

#[test]
fn dry_run_respects_completion_check() {
    let fixture = TestFixture::new("dry-run-incomplete");
    let config_path = fixture.setup_nextseq();

    let output = cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .args(["--dry-run"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Complete run should appear
    assert!(stdout.contains("240101_NB001"));
    // Incomplete run should not
    assert!(!stdout.contains("240202_NB002"));
}

#[test]
fn setup_fails_missing_source() {
    let fixture = TestFixture::new("setup-no-source");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    // Do NOT create nanopore-source
    fixture.mkdir("nanopore-landing-core");
    fixture.mkdir("nanopore-landing-other");
    let config_path = fixture.write_nanopore_config();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check"])
        .assert()
        .failure();
}

#[test]
fn setup_fails_missing_landing_zone() {
    let fixture = TestFixture::new("setup-no-landing");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");
    // Do NOT create nanopore-landing-other
    let config_path = fixture.write_nanopore_config();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check"])
        .assert()
        .failure();
}

#[test]
fn setup_allows_duplicate_landing_zones() {
    // Two categories sharing a landing zone is allowed: the flock serializes
    // runs and `classify` is first-match-wins, so it isn't a correctness
    // hazard, just a config style choice.
    let fixture = TestFixture::new("setup-dup-landing");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    let shared_landing = fixture.mkdir("nanopore-landing-shared");

    let config = format!(
        r#"flockdir: "{flockdir}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^ONT_WGS_"
    landing_zone:
      kind: local
      path: "{landing}"
    exclude: []
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    landing_zone:
      kind: local
      path: "{landing}"
    exclude: []
    completion_file_globs:
      - "report*.html"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nanopore-source").display(),
        landing = shared_landing.display(),
    );
    let config_path = fixture.path("nanopore.yaml");
    fs::write(&config_path, config).unwrap();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check"])
        .assert()
        .success();
}

#[test]
fn setup_fails_landing_zone_equals_flockdir() {
    let fixture = TestFixture::new("setup-landing-is-flock");
    let shared = fixture.mkdir("shared");
    fixture.mkdir("logdir");
    fixture.mkdir("nextseq-source");

    let config = format!(
        r#"flockdir: "{shared}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^\\d{{6}}_"
    landing_zone:
      kind: local
      path: "{shared}"
    exclude: []
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        shared = shared.display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nextseq-source").display(),
    );
    let config_path = fixture.path("nextseq.yaml");
    fs::write(&config_path, config).unwrap();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check"])
        .assert()
        .failure();
}

// ---------------------------------------------------------------------------
// Opt-in end-to-end test: rsync over SSH to a private sshd on localhost.
//
// Gated behind `SEQUENCER_SYNC_E2E_REMOTE=1` because it requires `sshd`,
// `ssh-keygen`, and `ssh-keyscan` to be installed and bindable on a high
// port. It runs a private sshd in a temp directory; it does not touch the
// user's real `~/.ssh`. Each child process gets `HOME` set to the fixture's
// temp dir so ssh picks up the test's identity and known_hosts.
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod remote_e2e {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command as StdCommand, Stdio};
    use std::time::{Duration, Instant};
    use std::{fs, net};

    use super::{TestFixture, cmd, read_transfer_log};

    fn e2e_enabled() -> bool {
        std::env::var("SEQUENCER_SYNC_E2E_REMOTE").as_deref() == Ok("1")
    }

    fn find_sshd() -> Option<PathBuf> {
        for candidate in [
            "/usr/sbin/sshd",
            "/usr/local/sbin/sshd",
            "/opt/homebrew/sbin/sshd",
        ] {
            let p = PathBuf::from(candidate);
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    fn pick_high_port() -> u16 {
        let listener = net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        listener.local_addr().expect("local_addr").port()
    }

    fn write_with_mode(path: &Path, contents: &str, mode: u32) {
        fs::write(path, contents).expect("write file");
        let mut perms = fs::metadata(path).expect("stat").permissions();
        perms.set_mode(mode);
        fs::set_permissions(path, perms).expect("chmod");
    }

    fn run_check(cmd: &mut StdCommand) {
        let out = cmd.output().expect("spawn");
        assert!(
            out.status.success(),
            "command {cmd:?} failed: status={:?} stdout={} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    struct PrivateSshd {
        child: Child,
        port: u16,
        /// Directory containing a `ssh` shim that injects the test's identity
        /// file and known_hosts via -o options. Prepend to PATH for child
        /// processes so both `Command::new("ssh")` from sequencer-sync and the
        /// `ssh` rsync spawns via `-e ssh ...` pick it up. macOS ssh ignores
        /// `$HOME` when locating user known_hosts (it uses the passwd-entry
        /// home), so a shim is the only portable way to redirect those paths
        /// without touching the user's real `~/.ssh`.
        shim_bin_dir: PathBuf,
    }

    impl Drop for PrivateSshd {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn start_private_sshd(server_root: &Path, sshd_bin: &Path) -> PrivateSshd {
        let identity_dir = server_root.join("identity");
        fs::create_dir_all(&identity_dir).expect("mkdir identity");
        let mut perms = fs::metadata(&identity_dir).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&identity_dir, perms).unwrap();

        let id_path = identity_dir.join("id_ed25519");
        run_check(
            StdCommand::new("ssh-keygen")
                .args(["-t", "ed25519", "-N", "", "-q", "-f"])
                .arg(&id_path),
        );

        let host_key = server_root.join("ssh_host_ed25519_key");
        run_check(
            StdCommand::new("ssh-keygen")
                .args(["-t", "ed25519", "-N", "", "-q", "-f"])
                .arg(&host_key),
        );

        let pub_key = fs::read_to_string(id_path.with_extension("pub")).expect("read pubkey");
        let authorized_keys = server_root.join("authorized_keys");
        write_with_mode(&authorized_keys, &pub_key, 0o600);

        let pid_file = server_root.join("sshd.pid");
        let port = pick_high_port();
        let sshd_config = format!(
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {host_key}\n\
             PidFile {pid_file}\n\
             AuthorizedKeysFile {authorized_keys}\n\
             PasswordAuthentication no\n\
             PubkeyAuthentication yes\n\
             ChallengeResponseAuthentication no\n\
             KbdInteractiveAuthentication no\n\
             UsePAM no\n\
             StrictModes no\n\
             PrintMotd no\n\
             PrintLastLog no\n\
             PermitUserEnvironment no\n\
             X11Forwarding no\n\
             AllowAgentForwarding no\n\
             AllowTcpForwarding no\n\
             Subsystem sftp internal-sftp\n",
            host_key = host_key.display(),
            pid_file = pid_file.display(),
            authorized_keys = authorized_keys.display(),
        );
        let sshd_config_path = server_root.join("sshd_config");
        fs::write(&sshd_config_path, sshd_config).unwrap();

        let child = StdCommand::new(sshd_bin)
            .arg("-D")
            .arg("-f")
            .arg(&sshd_config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sshd");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "sshd did not start listening on port {port} within 5s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        let known_hosts = identity_dir.join("known_hosts");
        let scan_out = StdCommand::new("ssh-keyscan")
            .args(["-p", &port.to_string(), "-t", "ed25519", "127.0.0.1"])
            .output()
            .expect("ssh-keyscan");
        assert!(
            scan_out.status.success() && !scan_out.stdout.is_empty(),
            "ssh-keyscan failed: status={:?} stderr={}",
            scan_out.status,
            String::from_utf8_lossy(&scan_out.stderr),
        );
        let mut f = fs::File::create(&known_hosts).expect("create known_hosts");
        f.write_all(&scan_out.stdout).expect("write known_hosts");

        let real_ssh = which_ssh();
        let shim_bin_dir = server_root.join("bin");
        fs::create_dir_all(&shim_bin_dir).expect("mkdir shim bin");
        let shim_path = shim_bin_dir.join("ssh");
        let shim = format!(
            "#!/bin/sh\n\
             exec {real_ssh:?} \\\n  \
                 -o UserKnownHostsFile={known_hosts:?} \\\n  \
                 -o GlobalKnownHostsFile=/dev/null \\\n  \
                 -o IdentityFile={identity:?} \\\n  \
                 -o IdentitiesOnly=yes \\\n  \
                 \"$@\"\n",
            real_ssh = real_ssh.display(),
            known_hosts = known_hosts.display(),
            identity = id_path.display(),
        );
        write_with_mode(&shim_path, &shim, 0o755);

        PrivateSshd {
            child,
            port,
            shim_bin_dir,
        }
    }

    fn which_ssh() -> PathBuf {
        for candidate in [
            "/usr/bin/ssh",
            "/usr/local/bin/ssh",
            "/opt/homebrew/bin/ssh",
        ] {
            let p = PathBuf::from(candidate);
            if p.exists() {
                return p;
            }
        }
        panic!("could not locate ssh client binary");
    }

    #[test]
    fn rsync_over_ssh_to_localhost() {
        if !e2e_enabled() {
            eprintln!("skipping remote e2e test (set SEQUENCER_SYNC_E2E_REMOTE=1 to enable)");
            return;
        }
        let Some(sshd_bin) = find_sshd() else {
            eprintln!("skipping remote e2e test: sshd binary not found");
            return;
        };
        let username = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .expect("USER or LOGNAME must be set");

        let fixture = TestFixture::new("remote-e2e");
        fixture.mkdir("flockdir");
        fixture.mkdir("logdir");
        fixture.mkdir("source");
        let remote_landing = fixture.mkdir("remote landing");
        let server_root = fixture.mkdir("server");
        fixture.mkdir("remote landing/240101_NB 001");
        fixture.mkdir("source/240101_NB 001");
        fixture.mkdir("source/240101_NB 001/PrimaryAnalysisMetrics");
        fixture.write_file(
            "source/240101_NB 001/PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv",
            "metrics",
        );
        fixture.write_file("source/240101_NB 001/data.txt", "payload");

        let sshd = start_private_sshd(&server_root, &sshd_bin);

        let config = format!(
            r#"flockdir: "{flockdir}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^\\d{{6}}_"
    landing_zone:
      kind: remote
      user: "{user}"
      host: "127.0.0.1"
      port: {port}
      dir: "{remote_dir}"
    exclude: []
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
            flockdir = fixture.path("flockdir").display(),
            logdir = fixture.path("logdir").display(),
            source = fixture.path("source").display(),
            user = username,
            port = sshd.port,
            remote_dir = remote_landing.display(),
        );
        let config_path = fixture.path("config.yaml");
        fs::write(&config_path, config).unwrap();

        let path_env = match std::env::var_os("PATH") {
            Some(existing) => {
                let mut combined = sshd.shim_bin_dir.clone().into_os_string();
                combined.push(":");
                combined.push(&existing);
                combined
            }
            None => sshd.shim_bin_dir.clone().into_os_string(),
        };

        cmd()
            .env("PATH", &path_env)
            .args(["setup", "--config-path"])
            .arg(&config_path)
            .assert()
            .success();

        cmd()
            .env("PATH", &path_env)
            .args(["run", "--config-path"])
            .arg(&config_path)
            .assert()
            .success();

        let landed = remote_landing.join("240101_NB 001/data.txt");
        assert!(landed.exists(), "expected file at {}", landed.display());
        let marker = remote_landing.join("240101_NB 001/transfer_successful.txt");
        assert!(marker.exists(), "expected marker at {}", marker.display());

        let log = read_transfer_log(&fixture);
        assert!(log.contains("\"succeeded\":true"));
    }
}
