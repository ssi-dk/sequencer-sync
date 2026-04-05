use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use assert_cmd::Command;

struct TestFixture {
    root: PathBuf,
}

// ---------------------------------------------------------------------------
// Fake ssh/rsync stub scripts
// ---------------------------------------------------------------------------

const FAKE_SSH_SCRIPT: &str = r#"#!/bin/sh
echo "$@" >> "$FAKE_SSH_LOG"
if [ -n "$FAKE_SSH_FAIL_PATTERN" ]; then
    case "$*" in
        *"$FAKE_SSH_FAIL_PATTERN"*) exit 1 ;;
    esac
fi
exit "${FAKE_SSH_EXIT_CODE:-0}"
"#;

const FAKE_RSYNC_SCRIPT: &str = r#"#!/bin/sh
echo "$@" >> "$FAKE_RSYNC_LOG"
if [ "${FAKE_RSYNC_EXIT_CODE+set}" = set ]; then
    exit "$FAKE_RSYNC_EXIT_CODE"
fi
# Last two args are source and destination
eval "src=\${$(($#-1))}"
eval "dst=\${$#}"
# Map remote destination (user@host:path) to local fake root
case "$dst" in
    *:*) dst="$FAKE_REMOTE_ROOT${dst#*:}" ;;
esac
mkdir -p "$dst"
(cd "$src" && cp -a . "$dst/")
exit 0
"#;

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
    destination:
      type: local
      path: "{landing_core}"
    exclude: []
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    destination:
      type: local
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
    destination:
      type: local
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

    // --- Fake binary support ---

    /// Create a bin/ directory with fake ssh and rsync scripts, returning the
    /// path to prepend to PATH.
    fn setup_fake_bin(&self) -> PathBuf {
        let bin = self.mkdir("bin");
        let ssh_path = bin.join("ssh");
        fs::write(&ssh_path, FAKE_SSH_SCRIPT).unwrap();
        fs::set_permissions(&ssh_path, fs::Permissions::from_mode(0o755)).unwrap();
        let rsync_path = bin.join("rsync");
        fs::write(&rsync_path, FAKE_RSYNC_SCRIPT).unwrap();
        fs::set_permissions(&rsync_path, fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    fn ssh_log_path(&self) -> PathBuf {
        self.path("ssh.log")
    }

    fn rsync_log_path(&self) -> PathBuf {
        self.path("rsync.log")
    }

    fn fake_remote_root(&self) -> PathBuf {
        self.path("fake-remote")
    }

    fn read_log_file(path: &PathBuf) -> Vec<String> {
        if path.exists() {
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(String::from)
                .collect()
        } else {
            Vec::new()
        }
    }

    fn ssh_log_lines(&self) -> Vec<String> {
        Self::read_log_file(&self.ssh_log_path())
    }

    fn rsync_log_lines(&self) -> Vec<String> {
        Self::read_log_file(&self.rsync_log_path())
    }

    fn write_remote_config_two_categories(&self) -> PathBuf {
        let config = format!(
            r#"flockdir: "{flockdir}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^ONT_WGS_"
    destination:
      type: remote
      user: "seqbot"
      host: "storage.local"
      port: 22
      path: "/srv/data/core"
    exclude: []
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    destination:
      type: remote
      user: "seqbot"
      host: "storage.local"
      port: 22
      path: "/srv/data/other"
    exclude: []
    completion_file_globs:
      - "report*.html"
"#,
            flockdir = self.path("flockdir").display(),
            logdir = self.path("logdir").display(),
            source = self.path("source").display(),
        );
        let path = self.path("remote.yaml");
        fs::write(&path, config).unwrap();
        path
    }

    fn write_remote_config_single_category(&self) -> PathBuf {
        let config = format!(
            r#"flockdir: "{flockdir}"
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^ONT_"
    destination:
      type: remote
      user: "seqbot"
      host: "storage.local"
      port: 22
      path: "/srv/data/landing"
    exclude: []
    completion_file_globs:
      - "report*.html"
"#,
            flockdir = self.path("flockdir").display(),
            logdir = self.path("logdir").display(),
            source = self.path("source").display(),
        );
        let path = self.path("remote-single.yaml");
        fs::write(&path, config).unwrap();
        path
    }

    /// Set up common directories and a complete run for remote tests.
    fn setup_remote_source(&self) {
        self.mkdir("flockdir");
        self.mkdir("logdir");
        self.mkdir("source");
        self.mkdir("source/ONT_WGS_run1");
        self.write_file("source/ONT_WGS_run1/report_final.html", "report");
        self.write_file("source/ONT_WGS_run1/data.txt", "data");
        self.mkdir("source/ONT_raw_run2");
        self.write_file("source/ONT_raw_run2/report_final.html", "report");
        self.write_file("source/ONT_raw_run2/data.txt", "other data");
    }

    /// Build a Command with fake bin on PATH and standard env vars set.
    fn remote_cmd(&self, bin_dir: &std::path::Path) -> Command {
        let mut c = cmd();
        let path = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        c.env("PATH", path);
        c.env("FAKE_SSH_LOG", self.ssh_log_path());
        c.env("FAKE_RSYNC_LOG", self.rsync_log_path());
        c.env("FAKE_REMOTE_ROOT", self.fake_remote_root());
        c
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
    destination:
      type: local
      path: "{landing_core}"
    exclude: []
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    destination:
      type: local
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
    destination:
      type: local
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
    destination:
      type: local
      path: "{landing_core}"
    exclude: ["/Data", "/AutoCenter"]
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    destination:
      type: local
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
    destination:
      type: local
      path: "{landing_core}"
    exclude: ["/Data", "/AutoCenter"]
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    destination:
      type: local
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
fn dry_run_prints_remote_destination_without_copying() {
    let fixture = TestFixture::new("dry-run-remote");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nextseq-source");
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
    destination:
      type: remote
      user: "alice"
      host: "example.org"
      port: 2222
      path: "/incoming/nextseq"
    exclude: []
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nextseq-source").display(),
    );
    let config_path = fixture.path("nextseq-remote.yaml");
    fs::write(&config_path, config).unwrap();

    let output = cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .args(["--dry-run"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("240101_NB001"));
    assert!(stdout.contains("alice@example.org:/incoming/nextseq/240101_NB001"));
    assert_eq!(transfer_log_line_count(&fixture), 0);
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
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^ONT_WGS_"
    destination:
      type: local
      path: "{landing}"
    exclude: []
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    destination:
      type: local
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
lock_file_name: "sequencer-sync.lock"
logdir: "{logdir}"
source: "{source}"

category:
  - regex: "^\\d{{6}}_"
    destination:
      type: local
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

// ===========================================================================
// Remote destination integration tests (fake ssh/rsync stubs)
// ===========================================================================

#[test]
fn remote_setup_shared_endpoint_checks_ssh_once_and_probes_each_path() {
    let fixture = TestFixture::new("remote-setup-shared");
    fixture.setup_remote_source();
    let bin_dir = fixture.setup_fake_bin();
    let config_path = fixture.write_remote_config_two_categories();

    fixture
        .remote_cmd(&bin_dir)
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let ssh_lines = fixture.ssh_log_lines();

    // One SSH access-check call ("true" payload)
    let access_checks: Vec<_> = ssh_lines
        .iter()
        .filter(|l| l.ends_with("-- true"))
        .collect();
    assert_eq!(
        access_checks.len(),
        1,
        "expected exactly one SSH access check, got: {access_checks:?}"
    );

    // One writability probe per distinct remote path
    let probes: Vec<_> = ssh_lines
        .iter()
        .filter(|l| l.contains("write-probe"))
        .collect();
    assert_eq!(
        probes.len(),
        2,
        "expected two writability probes (one per remote path), got: {probes:?}"
    );

    // Verify both paths are probed
    assert!(probes.iter().any(|l| l.contains("/srv/data/core")));
    assert!(probes.iter().any(|l| l.contains("/srv/data/other")));
}

#[test]
fn remote_setup_skip_ssh_check_makes_no_ssh_calls() {
    let fixture = TestFixture::new("remote-setup-skip-ssh");
    fixture.setup_remote_source();
    let bin_dir = fixture.setup_fake_bin();
    let config_path = fixture.write_remote_config_two_categories();

    fixture
        .remote_cmd(&bin_dir)
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check"])
        .assert()
        .success();

    assert!(
        fixture.ssh_log_lines().is_empty(),
        "expected no SSH calls with --skip-ssh-check"
    );
}

#[test]
fn remote_run_success() {
    let fixture = TestFixture::new("remote-run-success");
    fixture.setup_remote_source();
    let bin_dir = fixture.setup_fake_bin();
    let config_path = fixture.write_remote_config_single_category();

    fixture
        .remote_cmd(&bin_dir)
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let ssh_lines = fixture.ssh_log_lines();
    let rsync_lines = fixture.rsync_log_lines();

    // mkdir -p calls (one per directory to transfer)
    let mkdirs: Vec<_> = ssh_lines
        .iter()
        .filter(|l| l.contains("mkdir -p"))
        .collect();
    assert_eq!(mkdirs.len(), 2, "expected two mkdir calls, got: {mkdirs:?}");

    // rsync calls
    assert_eq!(
        rsync_lines.len(),
        2,
        "expected two rsync calls, got: {rsync_lines:?}"
    );
    assert!(rsync_lines.iter().any(|l| l.contains("ONT_WGS_run1")));
    assert!(rsync_lines.iter().any(|l| l.contains("ONT_raw_run2")));

    // touch marker calls
    let touches: Vec<_> = ssh_lines
        .iter()
        .filter(|l| l.contains("touch -- "))
        .collect();
    assert_eq!(
        touches.len(),
        2,
        "expected two touch-marker calls, got: {touches:?}"
    );
    assert!(
        touches
            .iter()
            .any(|l| l.contains("transfer_successful.txt"))
    );

    // Transfer log records success
    let log = read_transfer_log(&fixture);
    let succeeded_count = log.matches("\"succeeded\":true").count();
    assert_eq!(succeeded_count, 2);

    // Files were copied to the fake remote root
    let remote_root = fixture.fake_remote_root();
    assert!(
        remote_root
            .join("srv/data/landing/ONT_WGS_run1/data.txt")
            .exists()
    );
    assert!(
        remote_root
            .join("srv/data/landing/ONT_raw_run2/data.txt")
            .exists()
    );
}

#[test]
fn remote_run_rsync_failure_records_failure_and_skips_marker() {
    let fixture = TestFixture::new("remote-run-rsync-fail");
    fixture.setup_remote_source();
    let bin_dir = fixture.setup_fake_bin();
    let config_path = fixture.write_remote_config_single_category();

    fixture
        .remote_cmd(&bin_dir)
        .env("FAKE_RSYNC_EXIT_CODE", "1")
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .failure();

    // Transfer log records failure
    let log = read_transfer_log(&fixture);
    let failed_count = log.matches("\"succeeded\":false").count();
    assert_eq!(
        failed_count, 2,
        "both transfers should be recorded as failed"
    );

    // No touch-marker calls (marker is skipped on rsync failure)
    let ssh_lines = fixture.ssh_log_lines();
    let touches: Vec<_> = ssh_lines
        .iter()
        .filter(|l| l.contains("touch -- "))
        .collect();
    assert!(
        touches.is_empty(),
        "expected no touch-marker calls after rsync failure, got: {touches:?}"
    );

    // Run log records FAILED
    let run_log = read_run_log(&fixture);
    assert_eq!(
        run_log.matches("FAILED transfer").count(),
        2,
        "run log should record both failures"
    );
}

#[test]
fn remote_run_marker_failure_records_success_and_warns() {
    let fixture = TestFixture::new("remote-run-marker-fail");
    fixture.setup_remote_source();
    let bin_dir = fixture.setup_fake_bin();
    let config_path = fixture.write_remote_config_single_category();

    // Make ssh fail only for touch commands (marker writes), succeed for mkdir.
    // The binary exits with failure because run_log.had_error() is true after
    // logging the marker-write warnings.
    fixture
        .remote_cmd(&bin_dir)
        .env("FAKE_SSH_FAIL_PATTERN", "touch -- ")
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .failure();

    // Transfer log records success (rsync succeeded)
    let log = read_transfer_log(&fixture);
    let succeeded_count = log.matches("\"succeeded\":true").count();
    assert_eq!(
        succeeded_count, 2,
        "transfers should be recorded as successful despite marker failure"
    );

    // Run log warns about marker failure
    let run_log = read_run_log(&fixture);
    assert!(
        run_log.contains("failed to write transfer marker"),
        "run log should warn about marker failure, got: {run_log}"
    );
}

#[test]
fn remote_dry_run_makes_no_ssh_or_rsync_calls() {
    let fixture = TestFixture::new("remote-dry-run");
    fixture.setup_remote_source();
    let bin_dir = fixture.setup_fake_bin();
    let config_path = fixture.write_remote_config_single_category();

    let output = fixture
        .remote_cmd(&bin_dir)
        .args(["run", "--config-path"])
        .arg(&config_path)
        .args(["--dry-run"])
        .output()
        .unwrap();

    assert!(output.status.success());

    // No ssh or rsync calls
    assert!(
        fixture.ssh_log_lines().is_empty(),
        "expected no SSH calls in dry-run"
    );
    assert!(
        fixture.rsync_log_lines().is_empty(),
        "expected no rsync calls in dry-run"
    );

    // Destination is printed correctly
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("seqbot@storage.local:/srv/data/landing/ONT_WGS_run1"));
    assert!(stdout.contains("seqbot@storage.local:/srv/data/landing/ONT_raw_run2"));

    // No transfer log entries
    assert_eq!(transfer_log_line_count(&fixture), 0);
}

#[test]
fn remote_run_multi_category_command_ordering() {
    let fixture = TestFixture::new("remote-run-multi-cat");
    fixture.setup_remote_source();
    let bin_dir = fixture.setup_fake_bin();
    let config_path = fixture.write_remote_config_two_categories();

    fixture
        .remote_cmd(&bin_dir)
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let ssh_lines = fixture.ssh_log_lines();
    let rsync_lines = fixture.rsync_log_lines();

    // Two rsync calls, one per directory
    assert_eq!(rsync_lines.len(), 2);

    // Each directory gets: mkdir, rsync, touch (in that order per directory)
    // Total SSH calls: 2 mkdir + 2 touch = 4
    let mkdirs: Vec<_> = ssh_lines
        .iter()
        .filter(|l| l.contains("mkdir -p"))
        .collect();
    let touches: Vec<_> = ssh_lines
        .iter()
        .filter(|l| l.contains("touch -- "))
        .collect();
    assert_eq!(mkdirs.len(), 2);
    assert_eq!(touches.len(), 2);

    // WGS goes to /srv/data/core, raw goes to /srv/data/other
    assert!(
        rsync_lines
            .iter()
            .any(|l| l.contains("ONT_WGS_run1") && l.contains("/srv/data/core"))
    );
    assert!(
        rsync_lines
            .iter()
            .any(|l| l.contains("ONT_raw_run2") && l.contains("/srv/data/other"))
    );

    // Transfer log has 2 success entries
    let log = read_transfer_log(&fixture);
    assert_eq!(log.matches("\"succeeded\":true").count(), 2);
}
