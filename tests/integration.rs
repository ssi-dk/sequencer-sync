use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;

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
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "{landing_core}"
    exclude: []
    completion_file_glob: "report*.html"

  - regex: "^ONT_"
    landing_zone: "{landing_other}"
    exclude: []
    completion_file_glob: "report*.html"
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
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"

category:
  - regex: "^\\d{{6}}_"
    landing_zone: "{landing}"
    exclude: []
    completion_file_glob: "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
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
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "{landing_core}"
    exclude: []
    completion_file_glob: "report*.html"

  - regex: "^ONT_"
    landing_zone: "{root}/no-such-parent/nanopore-landing-other"
    exclude: []
    completion_file_glob: "report*.html"
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
fn nanopore_transfers_complete_runs() {
    let fixture = TestFixture::new("nanopore-complete");
    let config_path = fixture.setup_nanopore();

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

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .args(["--ignore-incomplete"])
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
fn second_run_is_noop() {
    let fixture = TestFixture::new("second-noop");
    let config_path = fixture.setup_nanopore();

    // First run
    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let log_after_first = read_transfer_log(&fixture);
    assert_eq!(transfer_log_line_count(&fixture), 2);
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

    // Latest log unchanged (no-op doesn't write, so file from first run persists)
    let latest_after_second = read_latest_log(&fixture);
    assert_eq!(latest_after_first, latest_after_second);
}

#[test]
fn retry_failed() {
    let fixture = TestFixture::new("retry-failed");
    let config_path = fixture.setup_nextseq();

    // Write a failed entry to the transfer log
    let log_path = transfer_log_path(&fixture);
    fs::write(
        &log_path,
        "{\"directory\":\"240101_NB001\",\"transferred_at\":\"2026-01-01T00:00:00Z\",\"succeeded\":false}\n",
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

    // Run log should contain "Retrying" line
    let run_log = read_run_log(&fixture);
    assert!(run_log.contains("Retrying previously failed transfer: 240101_NB001"));
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
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "{landing_core}"
    exclude: ["*_skip", "pod5*"]
    completion_file_glob: "report*.html"

  - regex: "^ONT_"
    landing_zone: "{landing_other}"
    exclude: []
    completion_file_glob: "report*.html"
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
    assert!(stdout.contains("exclude: *_skip"));
    assert!(stdout.contains("exclude: pod5*"));
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
fn setup_fails_duplicate_landing_zones() {
    let fixture = TestFixture::new("setup-dup-landing");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    let shared_landing = fixture.mkdir("nanopore-landing-shared");

    let config = format!(
        r#"flockdir: "{flockdir}"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "{landing}"
    exclude: []
    completion_file_glob: "report*.html"

  - regex: "^ONT_"
    landing_zone: "{landing}"
    exclude: []
    completion_file_glob: "report*.html"
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
        .failure();
}

#[test]
fn setup_fails_landing_zone_equals_flockdir() {
    let fixture = TestFixture::new("setup-landing-is-flock");
    let shared = fixture.mkdir("shared");
    fixture.mkdir("logdir");
    fixture.mkdir("nextseq-source");

    let config = format!(
        r#"flockdir: "{shared}"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"

category:
  - regex: "^\\d{{6}}_"
    landing_zone: "{shared}"
    exclude: []
    completion_file_glob: "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
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
