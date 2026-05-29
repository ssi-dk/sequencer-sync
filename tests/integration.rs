use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use assert_cmd::Command;

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sequencer-sync-integration-{name}-{}-{timestamp}-{id}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("fixture root should be created");
        Self { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn mkdir(&self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(&path).expect("fixture directory should be created");
        path
    }

    fn write_file(&self, relative: &str, contents: &str) {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent should be created");
        }
        fs::write(path, contents).expect("fixture file should be written");
    }

    fn prepare_common_dirs(&self) {
        self.mkdir("flock");
        self.mkdir("log");
        self.mkdir("source");
        self.mkdir("staging");
        self.mkdir("landing");
    }

    fn write_config(&self) -> PathBuf {
        let config = format!(
            r#"version: 3
lock_file: "{lock_file}"
logdir: "{logdir}"
server_user: "test"
server_port: 22
server_host: "localhost"
source: "{source}"
filestructures:
  default:
    ignore_globs:
      - "ignored/**"
    checkout_globs:
      - "report.txt"
    completion_file_globs:
      - "complete.txt"

category:
  - regex: "^run-"
    staging_zone: "{staging}"
    landing_zone: "{landing}"
    filestructure: "default"
"#,
            lock_file = self.path("flock/sequencer-sync.lock").display(),
            logdir = self.path("log").display(),
            source = self.path("source").display(),
            staging = self.path("staging").display(),
            landing = self.path("landing").display(),
        );
        let path = self.path("config.yaml");
        fs::write(&path, config).expect("config should be written");
        path
    }

    fn write_complete_run(&self, name: &str) {
        self.write_file(&format!("source/{name}/complete.txt"), "done");
        self.write_file(&format!("source/{name}/report.txt"), "report");
        self.write_file(&format!("source/{name}/raw/data.bin"), "raw");
        self.write_file(&format!("source/{name}/ignored/secret.txt"), "secret");
    }

    fn transfer_log(&self) -> String {
        fs::read_to_string(self.path("log/transferred-directories.jsonl")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn cmd() -> Command {
    Command::cargo_bin("sequencer-sync").expect("binary should be available")
}

fn read_dir_names(path: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(path)
        .expect("directory should be readable")
        .map(|entry| {
            entry
                .expect("entry should be readable")
                .file_name()
                .into_string()
                .expect("fixture names should be UTF-8")
        })
        .collect();
    names.sort();
    names
}

#[test]
fn setup_accepts_current_config_and_initializes_operational_files() {
    let fixture = Fixture::new("setup-current-config");
    fixture.prepare_common_dirs();
    let config_path = fixture.write_config();

    cmd()
        .args(["setup", "--config-path"])
        .arg(&config_path)
        .args(["--skip-ssh-check", "--skip-tree-check"])
        .assert()
        .success();

    let cron_file = fixture.path("log/sequencer-sync.cron");
    assert!(cron_file.is_file());
    assert!(
        fs::read_to_string(cron_file)
            .expect("cron file should be readable")
            .contains("--config-path")
    );
    assert!(
        fixture
            .transfer_log()
            .contains("\"directory\":\"_sequencer_sync_setup_\"")
    );
    assert!(read_dir_names(&fixture.path("staging")).is_empty());
    assert!(read_dir_names(&fixture.path("landing")).is_empty());
}

#[test]
fn dry_run_prints_plain_landing_destination_and_does_not_copy() {
    let fixture = Fixture::new("dry-run");
    fixture.prepare_common_dirs();
    fixture.write_complete_run("run-001");
    let config_path = fixture.write_config();

    let output = cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .arg("--dry-run")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be UTF-8");

    assert!(stdout.contains("run-001 ->"));
    assert!(stdout.contains("landing/run-001"));
    assert!(stdout.contains("  checkout: report.txt"));
    assert!(stdout.contains("  ignore: ignored/**"));
    assert!(read_dir_names(&fixture.path("landing")).is_empty());
    assert!(read_dir_names(&fixture.path("staging")).is_empty());
    assert!(fixture.transfer_log().is_empty());
}

#[test]
fn run_transfers_complete_directory_through_staging_to_plain_landing_name() {
    let fixture = Fixture::new("transfer-complete");
    fixture.prepare_common_dirs();
    fixture.write_complete_run("run-001");
    let config_path = fixture.write_config();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    assert_eq!(read_dir_names(&fixture.path("landing")), vec!["run-001"]);
    assert_eq!(
        fs::read_to_string(fixture.path("landing/run-001/report.txt"))
            .expect("checked-out file should exist"),
        "report"
    );
    assert!(!fixture.path("landing/run-001/ignored/secret.txt").exists());
    assert!(fixture.path("landing/run-001/archive.tar").is_file());
    assert!(read_dir_names(&fixture.path("staging")).is_empty());

    let transfer_log = fixture.transfer_log();
    assert!(transfer_log.contains("\"directory\":\"run-001\""));
    assert!(transfer_log.contains("\"succeeded\":true"));
}

#[test]
fn run_skips_incomplete_directory_without_writing_transfer_log() {
    let fixture = Fixture::new("skip-incomplete");
    fixture.prepare_common_dirs();
    fixture.write_file("source/run-001/report.txt", "report");
    let config_path = fixture.write_config();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    assert!(read_dir_names(&fixture.path("landing")).is_empty());
    assert!(read_dir_names(&fixture.path("staging")).is_empty());
    assert!(fixture.transfer_log().is_empty());
}

#[test]
fn second_run_is_noop_after_successful_transfer() {
    let fixture = Fixture::new("second-run-noop");
    fixture.prepare_common_dirs();
    fixture.write_complete_run("run-001");
    let config_path = fixture.write_config();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();
    let transfer_log = fixture.transfer_log();

    cmd()
        .args(["run", "--config-path"])
        .arg(&config_path)
        .assert()
        .success();

    assert_eq!(fixture.transfer_log(), transfer_log);
    assert_eq!(read_dir_names(&fixture.path("landing")), vec!["run-001"]);
}
