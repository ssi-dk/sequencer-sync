use std::fs;
use std::fs::File;
use std::path::PathBuf;

use assert_cmd::Command;
use flate2::read::GzDecoder;

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
            r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  nanopore:
    ignore_globs: []
    checkout_globs:
      - "report*.html"
      - "data.txt"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "{landing_core}"
    filestructure: "nanopore"

  - regex: "^ONT_"
    landing_zone: "{landing_other}"
    filestructure: "nanopore"
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
            r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  nextseq:
    ignore_globs: []
    checkout_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "data.txt"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"

category:
  - regex: "^\\d{{6}}_"
    landing_zone: "{landing}"
    filestructure: "nextseq"
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

fn tar_entries(path: PathBuf) -> Vec<String> {
    let file = File::open(path).unwrap();
    let mut archive = tar::Archive::new(file);
    archive
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().display().to_string())
        .collect()
}

fn tar_gz_entries(path: PathBuf) -> Vec<String> {
    let file = File::open(path).unwrap();
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().display().to_string())
        .collect()
}

fn tar_contains(entries: &[String], relative_path: &str) -> bool {
    entries
        .iter()
        .any(|entry| entry == relative_path || entry == &format!("./{relative_path}"))
}

#[test]
fn run_fails_missing_lock_file_parent() {
    let fixture = TestFixture::new("missing-lock-parent");
    // Create source and landing zones but NOT lock file parent directory
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
        r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "report*.html"
      - "data.txt"
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "core.marker"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "{landing_core}"
    filestructure: "default"

  - regex: "^ONT_"
    landing_zone: "{root}/no-such-parent/nanopore-landing-other"
    filestructure: "default"
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
fn run_skips_when_landing_zone_run_already_has_transfer_marker() {
    let fixture = TestFixture::new("landing-marker-present");
    let config_path = fixture.setup_nanopore();
    fixture.write_file(
        "nanopore-landing-core/ONT_WGS_run1/transfer_successful.txt",
        "",
    );
    fixture.write_file(
        "nanopore-landing-core/ONT_WGS_run1/existing.txt",
        "existing",
    );

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    assert!(
        !fixture
            .path("nanopore-landing-core/ONT_WGS_run1/data.txt")
            .exists()
    );
    assert!(
        fixture
            .path("nanopore-landing-core/ONT_WGS_run1/existing.txt")
            .exists()
    );
    assert!(
        fixture
            .path("nanopore-landing-core/ONT_WGS_run1/transfer_successful.txt")
            .exists()
    );

    let run_log = read_run_log(&fixture);
    assert!(run_log.contains(
        "Skipped ONT_WGS_run1 because transfer marker is unexpectedly already present in landing zone"
    ));
    assert_eq!(
        read_transfer_log(&fixture)
            .matches("\"directory\":\"ONT_WGS_run1\"")
            .count(),
        0
    );
    assert!(
        fixture
            .path("nanopore-landing-other/ONT_raw_run2/data.txt")
            .exists()
    );
}

#[test]
fn archive_files_are_packed_and_archive_directory_removed() {
    let fixture = TestFixture::new("archive-tar");
    let config_path = fixture.setup_nanopore();
    fixture.write_file("nanopore-source/ONT_WGS_run1/raw/pod5.bin", "raw");

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let transferred = fixture.path("nanopore-landing-core/ONT_WGS_run1");
    assert!(transferred.join("report_final.html").exists());
    assert!(transferred.join("archive.tar").exists());
    assert!(!transferred.join("archive.tar.gz").exists());
    assert!(!transferred.join("sequencer-sync-archive").exists());

    let entries = tar_entries(transferred.join("archive.tar"));
    assert!(tar_contains(&entries, "raw/pod5.bin"));
}

#[test]
fn archive_files_are_compressed_with_compress() {
    let fixture = TestFixture::new("archive-tar-compressed");
    let config_path = fixture.setup_nanopore();
    fixture.write_file("nanopore-source/ONT_WGS_run1/raw/pod5.bin", "raw");

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .args(["--compress"])
        .assert()
        .success();

    let transferred = fixture.path("nanopore-landing-core/ONT_WGS_run1");
    assert!(transferred.join("archive.tar.gz").exists());
    assert!(!transferred.join("archive.tar").exists());
    assert!(!transferred.join("sequencer-sync-archive").exists());

    let entries = tar_gz_entries(transferred.join("archive.tar.gz"));
    assert!(tar_contains(&entries, "raw/pod5.bin"));
}

#[test]
fn archive_tar_is_not_created_when_no_files_are_archived() {
    let fixture = TestFixture::new("no-archive-files");
    let config_path = fixture.setup_nanopore();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let transferred = fixture.path("nanopore-landing-core/ONT_WGS_run1");
    assert!(transferred.join("report_final.html").exists());
    assert!(transferred.join("data.txt").exists());
    assert!(!transferred.join("archive.tar").exists());
    assert!(!transferred.join("archive.tar.gz").exists());
    assert!(!transferred.join("sequencer-sync-archive").exists());
}

#[test]
fn empty_checkout_globs_archives_every_non_ignored_file() {
    let fixture = TestFixture::new("empty-checkout-archives-all");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");
    fixture.mkdir("nanopore-source/ONT_WGS_run1");
    fixture.write_file("nanopore-source/ONT_WGS_run1/report_final.html", "report");
    fixture.write_file("nanopore-source/ONT_WGS_run1/data/data.txt", "data");

    let config = format!(
        r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  archive_only:
    ignore_globs: []
    checkout_globs: []
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^ONT_"
    landing_zone: "{landing}"
    filestructure: "archive_only"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nanopore-source").display(),
        landing = fixture.path("nanopore-landing-core").display(),
    );
    let config_path = fixture.path("nanopore.yaml");
    fs::write(&config_path, config).unwrap();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let transferred = fixture.path("nanopore-landing-core/ONT_WGS_run1");
    assert!(!transferred.join("report_final.html").exists());
    assert!(!transferred.join("data/data.txt").exists());
    let entries = tar_entries(transferred.join("archive.tar"));
    assert!(tar_contains(&entries, "report_final.html"));
    assert!(tar_contains(&entries, "data/data.txt"));
}

#[test]
fn ignored_files_are_absent_from_checkout_and_archive() {
    let fixture = TestFixture::new("ignored-files");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");
    fixture.mkdir("nanopore-landing-other");
    fixture.mkdir("nanopore-source/ONT_WGS_run1");
    fixture.write_file("nanopore-source/ONT_WGS_run1/report_final.html", "report");
    fixture.write_file("nanopore-source/ONT_WGS_run1/ignored/secret.txt", "secret");
    fixture.write_file("nanopore-source/ONT_WGS_run1/raw/pod5.bin", "raw");

    let config = format!(
        r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  nanopore:
    ignore_globs:
      - "ignored/**"
    checkout_globs:
      - "report*.html"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^ONT_"
    landing_zone: "{landing}"
    filestructure: "nanopore"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nanopore-source").display(),
        landing = fixture.path("nanopore-landing-core").display(),
    );
    let config_path = fixture.path("nanopore.yaml");
    fs::write(&config_path, config).unwrap();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    let transferred = fixture.path("nanopore-landing-core/ONT_WGS_run1");
    assert!(!transferred.join("ignored/secret.txt").exists());
    let entries = tar_entries(transferred.join("archive.tar"));
    assert!(!tar_contains(&entries, "ignored/secret.txt"));
    assert!(tar_contains(&entries, "raw/pod5.bin"));
}

#[test]
fn run_fails_before_copy_when_file_matches_ignore_and_checkout() {
    let fixture = TestFixture::new("conflict-run");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");
    fixture.mkdir("nanopore-source/ONT_WGS_run1");
    fixture.write_file("nanopore-source/ONT_WGS_run1/report_final.html", "report");
    fixture.write_file("nanopore-source/ONT_WGS_run1/conflict.txt", "conflict");

    let config = format!(
        r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  nanopore:
    ignore_globs:
      - "conflict.txt"
    checkout_globs:
      - "report*.html"
      - "conflict.txt"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^ONT_"
    landing_zone: "{landing}"
    filestructure: "nanopore"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nanopore-source").display(),
        landing = fixture.path("nanopore-landing-core").display(),
    );
    let config_path = fixture.path("nanopore.yaml");
    fs::write(&config_path, config).unwrap();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .failure();

    assert!(!fixture.path("nanopore-landing-core/ONT_WGS_run1").exists());
    assert_eq!(
        read_transfer_log(&fixture)
            .matches("\"succeeded\":false")
            .count(),
        1
    );
}

#[test]
fn run_fails_when_checkout_file_conflicts_with_internal_archive_dir() {
    let fixture = TestFixture::new("archive-dir-checkout-conflict");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");
    fixture.mkdir("nanopore-source/ONT_WGS_run1");
    fixture.write_file("nanopore-source/ONT_WGS_run1/report_final.html", "report");
    fixture.write_file(
        "nanopore-source/ONT_WGS_run1/sequencer-sync-archive.gz",
        "checkout",
    );

    let config = format!(
        r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  nanopore:
    ignore_globs: []
    checkout_globs:
      - "report*.html"
      - "sequencer-sync-archive.gz"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^ONT_"
    landing_zone: "{landing}"
    filestructure: "nanopore"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nanopore-source").display(),
        landing = fixture.path("nanopore-landing-core").display(),
    );
    let config_path = fixture.path("nanopore.yaml");
    fs::write(&config_path, config).unwrap();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .failure();

    assert!(!fixture.path("nanopore-landing-core/ONT_WGS_run1").exists());
    assert_eq!(
        read_transfer_log(&fixture)
            .matches("\"succeeded\":false")
            .count(),
        1
    );
}

#[test]
fn classification_glob_selects_first_matching_category_and_falls_back() {
    let fixture = TestFixture::new("classification-glob");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");
    fixture.mkdir("nanopore-landing-other");

    fixture.mkdir("nanopore-source/ONT_run_with_marker");
    fixture.write_file("nanopore-source/ONT_run_with_marker/core.marker", "marker");
    fixture.write_file(
        "nanopore-source/ONT_run_with_marker/report_final.html",
        "report",
    );
    fixture.write_file("nanopore-source/ONT_run_with_marker/data.txt", "data");

    fixture.mkdir("nanopore-source/ONT_run_without_marker");
    fixture.write_file(
        "nanopore-source/ONT_run_without_marker/report_final.html",
        "report",
    );
    fixture.write_file("nanopore-source/ONT_run_without_marker/data.txt", "data");

    let config = format!(
        r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "report*.html"
      - "data.txt"
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "core.marker"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^ONT_"
    classification_glob: "core.marker"
    landing_zone: "{landing_core}"
    filestructure: "default"

  - regex: "^ONT_"
    landing_zone: "{landing_other}"
    filestructure: "default"
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
            .path("nanopore-landing-core/ONT_run_with_marker/data.txt")
            .exists()
    );
    assert!(
        !fixture
            .path("nanopore-landing-other/ONT_run_with_marker")
            .exists()
    );
    assert!(
        fixture
            .path("nanopore-landing-other/ONT_run_without_marker/data.txt")
            .exists()
    );
    assert!(
        !fixture
            .path("nanopore-landing-core/ONT_run_without_marker")
            .exists()
    );
    assert_eq!(transfer_log_line_count(&fixture), 2);
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
        r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "data.txt"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "Analysis/*/Data/fastq/Logs/FastqComplete.txt"

category:
  - regex: "^\\d{{6}}_"
    landing_zone: "{landing}"
    filestructure: "default"
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
fn redo_true_skips_succeeded_directory_when_landing_marker_is_present() {
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

    assert!(!transferred_file.exists());

    let log = read_transfer_log(&fixture);
    assert_eq!(
        log.matches("\"directory\":\"240101_NB001\"").count(),
        2,
        "skipped redo should not append a transfer log entry"
    );

    let run_log = read_run_log(&fixture);
    assert!(run_log.contains(
        "Skipped 240101_NB001 because transfer marker is unexpectedly already present in landing zone"
    ));
}

#[test]
fn setup_with_skip_ssh_check() {
    let fixture = TestFixture::new("setup-skip-ssh");
    let config_path = fixture.setup_nanopore();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check", "--skip-tree-check"])
        .assert()
        .success();

    // Cron file should be created
    assert!(fixture.path("logdir/sequencer-sync.cron").exists());
}

#[test]
fn setup_requires_tree_check_decision() {
    let fixture = TestFixture::new("setup-tree-required");
    let config_path = fixture.setup_nanopore();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check"])
        .assert()
        .failure();
}

#[test]
fn setup_rejects_both_tree_check_options() {
    let fixture = TestFixture::new("setup-tree-conflict-args");
    let config_path = fixture.setup_nanopore();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args([
            "--skip-ssh-check",
            "--skip-tree-check",
            "--tree-check-source",
        ])
        .arg(fixture.path("nanopore-source"))
        .assert()
        .failure();
}

#[test]
fn setup_tree_check_detects_ignore_checkout_overlap() {
    let fixture = TestFixture::new("setup-tree-conflict");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");
    fixture.mkdir("nanopore-source/ONT_WGS_run1");
    fixture.write_file("nanopore-source/ONT_WGS_run1/report_final.html", "report");
    fixture.write_file("nanopore-source/ONT_WGS_run1/conflict.txt", "conflict");

    let config = format!(
        r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  nanopore:
    ignore_globs:
      - "conflict.txt"
    checkout_globs:
      - "report*.html"
      - "conflict.txt"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^ONT_"
    landing_zone: "{landing}"
    filestructure: "nanopore"
"#,
        flockdir = fixture.path("flockdir").display(),
        logdir = fixture.path("logdir").display(),
        source = fixture.path("nanopore-source").display(),
        landing = fixture.path("nanopore-landing-core").display(),
    );
    let config_path = fixture.path("nanopore.yaml");
    fs::write(&config_path, config).unwrap();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check", "--tree-check-source"])
        .arg(fixture.path("nanopore-source"))
        .assert()
        .failure();
}

#[test]
fn setup_fails_malformed_existing_transfer_log() {
    let fixture = TestFixture::new("setup-malformed-transfer-log");
    let config_path = fixture.setup_nanopore();
    fs::write(transfer_log_path(&fixture), "{not-json}\n").unwrap();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check", "--skip-tree-check"])
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
fn dry_run_shows_filestructure_patterns() {
    let fixture = TestFixture::new("dry-run-filestructure");
    fixture.mkdir("flockdir");
    fixture.mkdir("logdir");
    fixture.mkdir("nanopore-source");
    fixture.mkdir("nanopore-landing-core");
    fixture.mkdir("nanopore-landing-other");

    fixture.mkdir("nanopore-source/ONT_WGS_run1");
    fixture.write_file("nanopore-source/ONT_WGS_run1/report_final.html", "report");

    let config = format!(
        r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "report*.html"
      - "data.txt"
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "core.marker"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "{landing_core}"
    filestructure: "default"

  - regex: "^ONT_"
    landing_zone: "{landing_other}"
    filestructure: "default"
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
    assert!(stdout.contains("checkout: report*.html"));
    assert!(stdout.contains("checkout: data.txt"));
}

#[test]
fn transfers_checkout_files_preserving_relative_paths() {
    let fixture = TestFixture::new("checkout-relative-paths");
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
        r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "report*.html"
      - "data.txt"
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "core.marker"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "{landing_core}"
    filestructure: "default"

  - regex: "^ONT_"
    landing_zone: "{landing_other}"
    filestructure: "default"
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
        !fixture
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
        .args(["--skip-ssh-check", "--skip-tree-check"])
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
        .args(["--skip-ssh-check", "--skip-tree-check"])
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
        r#"lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "report*.html"
      - "data.txt"
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "core.marker"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "{landing}"
    filestructure: "default"

  - regex: "^ONT_"
    landing_zone: "{landing}"
    filestructure: "default"
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
        .args(["--skip-ssh-check", "--skip-tree-check"])
        .assert()
        .failure();
}

#[test]
fn setup_fails_landing_zone_equals_lock_file_parent() {
    let fixture = TestFixture::new("setup-landing-is-flock");
    let shared = fixture.mkdir("shared");
    fixture.mkdir("logdir");
    fixture.mkdir("nextseq-source");

    let config = format!(
        r#"lock_file: "{shared}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "data.txt"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"

category:
  - regex: "^\\d{{6}}_"
    landing_zone: "{shared}"
    filestructure: "default"
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
        .args(["--skip-ssh-check", "--skip-tree-check"])
        .assert()
        .failure();
}
