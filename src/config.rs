use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use glob::Pattern;
use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug)]
pub struct Config {
    /// Canonicalized absolute path. Directory for the file lock.
    pub flockdir: PathBuf,
    /// Base name of the lock file inside `flockdir`.
    pub lock_file_name: String,
    /// Canonicalized absolute path. Directory for log files (transfer log, run
    /// log, cron file).
    pub logdir: PathBuf,

    /// Canonicalized absolute path.
    pub source: PathBuf,
    pub categories: Vec<Category>,
}

#[derive(Debug)]
pub struct Category {
    pub regex: Regex,
    pub landing_zone: LandingZone,
    pub exclude: Vec<String>,
    pub completion_file_globs: Vec<Pattern>,
    /// When true, place runs into a year-based subdirectory under the landing
    /// zone. The year is derived from the directory name by prepending "20" to
    /// its first two characters (e.g. "240101_NB123" → "2024/").
    pub year_subdirectory: bool,
}

/// Where a category's files are delivered. Either a local directory on the
/// sequencer, or an SSH-reachable directory on a remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandingZone {
    /// Canonicalized absolute path on the local filesystem.
    Local(PathBuf),
    /// Remote SSH endpoint. `dir` is taken as-is (POSIX-style absolute path on
    /// the remote host); we cannot canonicalize it from this machine.
    Remote {
        user: String,
        host: String,
        port: u16,
        dir: OsString,
    },
}

impl LandingZone {
    /// Human-readable rendering used in logs and error messages. Local paths
    /// render as the path; remotes render as `user@host:port:/dir`.
    pub fn display(&self) -> String {
        match self {
            Self::Local(p) => p.display().to_string(),
            Self::Remote {
                user,
                host,
                port,
                dir,
            } => format!("{user}@{host}:{port}:{}", dir.to_string_lossy()),
        }
    }

    /// Append a year-based subdirectory (e.g. "2024") under this landing zone,
    /// returning a new `LandingZone` of the same kind.
    pub fn with_subdir(&self, subdir: &str) -> Self {
        match self {
            Self::Local(p) => Self::Local(p.join(subdir)),
            Self::Remote {
                user,
                host,
                port,
                dir,
            } => Self::Remote {
                user: user.clone(),
                host: host.clone(),
                port: *port,
                dir: remote_join(dir, OsStr::new(subdir)),
            },
        }
    }

    /// Append a run directory name (the basename of a source run) under this
    /// landing zone.
    pub fn join_run(&self, run_name: &OsStr) -> Self {
        match self {
            Self::Local(p) => Self::Local(p.join(run_name)),
            Self::Remote {
                user,
                host,
                port,
                dir,
            } => Self::Remote {
                user: user.clone(),
                host: host.clone(),
                port: *port,
                dir: remote_join(dir, run_name),
            },
        }
    }
}

fn remote_join(base: &OsStr, child: &OsStr) -> OsString {
    let mut joined = trim_remote_trailing_slashes(base);
    joined.push("/");
    joined.push(child);
    joined
}

fn trim_remote_trailing_slashes(path: &OsStr) -> OsString {
    let mut bytes = path.as_bytes().to_vec();
    while bytes.ends_with(b"/") && bytes.len() > 1 {
        bytes.pop();
    }
    OsString::from_vec(bytes)
}

#[derive(Debug, Deserialize)]
struct UnvalidatedConfig {
    flockdir: PathBuf,
    lock_file_name: String,
    logdir: PathBuf,
    source: PathBuf,
    #[serde(default)]
    category: Vec<UnvalidatedCategory>,
}

#[derive(Debug, Deserialize)]
struct UnvalidatedCategory {
    regex: String,
    landing_zone: UnvalidatedLandingZone,
    #[serde(default)]
    exclude: Vec<String>,
    completion_file_globs: Vec<String>,
    #[serde(default)]
    year_subdirectory: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum UnvalidatedLandingZone {
    Local {
        path: PathBuf,
    },
    Remote {
        user: String,
        host: String,
        port: u16,
        dir: String,
    },
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse YAML config: {0}")]
    Parse(serde_yaml::Error),
    #[error("config field `{field}` must not be empty")]
    EmptyField { field: &'static str },
    #[error("config field `{field}` must not be 0")]
    ZeroPort { field: &'static str },
    #[error("config field `{field}` must be an absolute path: {}", path.display())]
    PathNotAbsolute { field: &'static str, path: PathBuf },
    #[error(
        "config field `{field}` must be an absolute POSIX path (start with `/`, no `..` segments): {value:?}"
    )]
    InvalidRemoteDir { field: &'static str, value: String },
    #[error(
        "config fields `{first}` and `{second}` must not point to the same path: {}",
        path.display()
    )]
    DuplicatePath {
        first: &'static str,
        second: &'static str,
        path: PathBuf,
    },
    #[error("config must contain at least one [[category]]")]
    NoCategoriesConfigured,
    #[error("config field `{field}` is not a valid regex: {source}")]
    InvalidRegex {
        field: &'static str,
        #[source]
        source: regex::Error,
    },
    #[error("config field `{field}` is not a valid glob pattern: {source}")]
    InvalidGlob {
        field: &'static str,
        #[source]
        source: glob::PatternError,
    },
    #[error("config field `{field}` must contain at least one glob pattern")]
    EmptyGlobList { field: &'static str },
    #[error(
        "config field `{field}` must be a valid file base name (no slashes, not \".\" or \"..\"): {value:?}"
    )]
    InvalidBaseName { field: &'static str, value: String },
    #[error("failed to canonicalize `{field}` path {}: {source}", path.display())]
    CanonicalizePath {
        field: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Config {
    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        let mut config = Self::from_yaml_str(&contents)?;
        config.canonicalize_paths()?;
        Ok(config)
    }

    // Note: This doesn't canonicalize paths!
    fn from_yaml_str(contents: &str) -> Result<Self, ConfigError> {
        let config: UnvalidatedConfig =
            serde_yaml::from_str(contents).map_err(ConfigError::Parse)?;
        config.validate()
    }

    /// Canonicalize all local path fields via the filesystem (resolving
    /// symlinks, `.`, and `..`). Remote landing zones are left untouched. Then
    /// verify that no local landing zone collides with `source`/`flockdir`/
    /// `logdir`, and that those three are pairwise distinct.
    fn canonicalize_paths(&mut self) -> Result<(), ConfigError> {
        self.flockdir = canonicalize_field("flockdir", &self.flockdir)?;
        self.logdir = canonicalize_field("logdir", &self.logdir)?;
        self.source = canonicalize_field("source", &self.source)?;

        for cat in &mut self.categories {
            if let LandingZone::Local(path) = &cat.landing_zone {
                let canonical = canonicalize_field("category.landing_zone.path", path)?;
                cat.landing_zone = LandingZone::Local(canonical);
            }
        }

        let core_paths: [(&'static str, &Path); 3] = [
            ("source", &self.source),
            ("flockdir", &self.flockdir),
            ("logdir", &self.logdir),
        ];
        validate_all_paths_distinct(&core_paths)?;

        for cat in &self.categories {
            if let LandingZone::Local(path) = &cat.landing_zone {
                for (other_field, other_path) in &core_paths {
                    if path == *other_path {
                        return Err(ConfigError::DuplicatePath {
                            first: "category.landing_zone.path",
                            second: other_field,
                            path: path.clone(),
                        });
                    }
                }
            }
        }

        Ok(())
    }
}

fn canonicalize_field(field: &'static str, path: &Path) -> Result<PathBuf, ConfigError> {
    fs::canonicalize(path).map_err(|source| ConfigError::CanonicalizePath {
        field,
        path: path.to_path_buf(),
        source,
    })
}

impl UnvalidatedConfig {
    fn validate(self) -> Result<Config, ConfigError> {
        validate_absolute_path("flockdir", &self.flockdir)?;
        validate_base_name("lock_file_name", &self.lock_file_name)?;
        validate_absolute_path("logdir", &self.logdir)?;
        validate_absolute_path("source", &self.source)?;

        if self.category.is_empty() {
            return Err(ConfigError::NoCategoriesConfigured);
        }

        let mut categories = Vec::with_capacity(self.category.len());
        for cat in self.category {
            categories.push(cat.validate()?);
        }

        Ok(Config {
            flockdir: self.flockdir,
            lock_file_name: self.lock_file_name,
            logdir: self.logdir,
            source: self.source,
            categories,
        })
    }
}

impl UnvalidatedCategory {
    fn validate(self) -> Result<Category, ConfigError> {
        let landing_zone = self.landing_zone.validate()?;

        let regex = Regex::new(&self.regex).map_err(|source| ConfigError::InvalidRegex {
            field: "category.regex",
            source,
        })?;

        let completion_file_globs = validate_globs(
            "category.completion_file_globs",
            &self.completion_file_globs,
        )?;

        Ok(Category {
            regex,
            landing_zone,
            exclude: self.exclude,
            completion_file_globs,
            year_subdirectory: self.year_subdirectory,
        })
    }
}

impl UnvalidatedLandingZone {
    fn validate(self) -> Result<LandingZone, ConfigError> {
        match self {
            Self::Local { path } => {
                validate_absolute_path("category.landing_zone.path", &path)?;
                Ok(LandingZone::Local(path))
            }
            Self::Remote {
                user,
                host,
                port,
                dir,
            } => {
                validate_non_empty("category.landing_zone.user", &user)?;
                validate_non_empty("category.landing_zone.host", &host)?;
                if port == 0 {
                    return Err(ConfigError::ZeroPort {
                        field: "category.landing_zone.port",
                    });
                }
                validate_remote_dir("category.landing_zone.dir", &dir)?;
                Ok(LandingZone::Remote {
                    user,
                    host,
                    port,
                    dir: OsString::from(dir),
                })
            }
        }
    }
}

fn validate_absolute_path(field: &'static str, path: &Path) -> Result<(), ConfigError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(ConfigError::PathNotAbsolute {
            field,
            path: path.to_path_buf(),
        })
    }
}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        Err(ConfigError::EmptyField { field })
    } else {
        Ok(())
    }
}

fn validate_base_name(field: &'static str, value: &str) -> Result<(), ConfigError> {
    if value.is_empty() || value.contains('/') || value == "." || value == ".." {
        Err(ConfigError::InvalidBaseName {
            field,
            value: value.to_string(),
        })
    } else {
        Ok(())
    }
}

fn validate_remote_dir(field: &'static str, value: &str) -> Result<(), ConfigError> {
    if value.is_empty() || !value.starts_with('/') {
        return Err(ConfigError::InvalidRemoteDir {
            field,
            value: value.to_string(),
        });
    }
    for segment in value.split('/') {
        if segment == ".." {
            return Err(ConfigError::InvalidRemoteDir {
                field,
                value: value.to_string(),
            });
        }
    }
    Ok(())
}

fn validate_glob(field: &'static str, pattern: &str) -> Result<Pattern, ConfigError> {
    Pattern::new(pattern).map_err(|source| ConfigError::InvalidGlob { field, source })
}

fn validate_globs(field: &'static str, patterns: &[String]) -> Result<Vec<Pattern>, ConfigError> {
    if patterns.is_empty() {
        return Err(ConfigError::EmptyGlobList { field });
    }

    patterns
        .iter()
        .map(|pattern| validate_glob(field, pattern))
        .collect()
}

fn validate_all_paths_distinct(paths: &[(&'static str, &Path)]) -> Result<(), ConfigError> {
    for i in 0..paths.len() {
        for j in (i + 1)..paths.len() {
            if paths[i].1 == paths[j].1 {
                return Err(ConfigError::DuplicatePath {
                    first: paths[i].0,
                    second: paths[j].0,
                    path: paths[i].1.to_path_buf(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    use super::{Config, ConfigError, LandingZone};

    const EXAMPLE_CONFIG: &str = include_str!("../examples/config.yaml");
    const NEXTSEQ_EXAMPLE: &str = r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    landing_zone:
      kind: local
      path: "/var/lib/sequencer/landing-zone"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#;

    fn local_path(zone: &LandingZone) -> &PathBuf {
        match zone {
            LandingZone::Local(p) => p,
            LandingZone::Remote { .. } => panic!("expected local landing zone"),
        }
    }

    #[test]
    fn parses_example_config() {
        let config = Config::from_yaml_str(EXAMPLE_CONFIG).expect("nanopore config should parse");

        assert_eq!(config.flockdir, PathBuf::from("/var/lib/sequencer/flock"));
        assert_eq!(config.lock_file_name, "sequencer-sync.lock");
        assert_eq!(config.logdir, PathBuf::from("/var/lib/sequencer/log"));
        assert_eq!(config.source, PathBuf::from("/data/nanopore"));

        assert_eq!(config.categories.len(), 2);
        assert!(config.categories[0].regex.is_match("ONT_WGS_run1"));
        assert!(!config.categories[0].regex.is_match("ONT_raw_run2"));
        assert_eq!(
            local_path(&config.categories[0].landing_zone),
            &PathBuf::from("/var/lib/sequencer/landing-zone-core")
        );
        assert!(config.categories[1].regex.is_match("ONT_raw_run2"));
        assert!(matches!(
            config.categories[1].landing_zone,
            LandingZone::Remote { .. }
        ));
    }

    #[test]
    fn parses_nextseq_example_config() {
        let config = Config::from_yaml_str(NEXTSEQ_EXAMPLE).expect("nextseq config should parse");

        assert_eq!(config.flockdir, PathBuf::from("/var/lib/sequencer/flock"));
        assert_eq!(config.logdir, PathBuf::from("/var/lib/sequencer/log"));
        assert_eq!(config.source, PathBuf::from("/data/nextseq"));
        assert_eq!(config.categories.len(), 1);
        assert!(config.categories[0].regex.is_match("240101_"));
        assert_eq!(
            local_path(&config.categories[0].landing_zone),
            &PathBuf::from("/var/lib/sequencer/landing-zone")
        );
    }

    #[test]
    fn parses_remote_landing_zone() {
        let config = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    landing_zone:
      kind: remote
      user: "syncer"
      host: "remote.example.org"
      port: 2222
      dir: "/data/landing"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        )
        .expect("remote config should parse");

        match &config.categories[0].landing_zone {
            LandingZone::Remote {
                user,
                host,
                port,
                dir,
            } => {
                assert_eq!(user, "syncer");
                assert_eq!(host, "remote.example.org");
                assert_eq!(*port, 2222);
                assert_eq!(dir, "/data/landing");
            }
            other => panic!("expected remote landing zone, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_landing_zone_kind() {
        let error = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    landing_zone:
      kind: cloud
      path: "/data/landing"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        )
        .expect_err("unknown kind should fail");
        assert!(matches!(error, ConfigError::Parse(_)));
    }

    #[test]
    fn rejects_relative_remote_dir() {
        let error = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    landing_zone:
      kind: remote
      user: "syncer"
      host: "remote.example.org"
      port: 22
      dir: "data/landing"
    completion_file_globs:
      - "x"
"#,
        )
        .expect_err("relative remote dir should fail");
        assert!(matches!(
            error,
            ConfigError::InvalidRemoteDir {
                field: "category.landing_zone.dir",
                ..
            }
        ));
    }

    #[test]
    fn rejects_remote_dir_with_dotdot_segment() {
        let error = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    landing_zone:
      kind: remote
      user: "syncer"
      host: "remote.example.org"
      port: 22
      dir: "/data/../landing"
    completion_file_globs:
      - "x"
"#,
        )
        .expect_err("remote dir with .. should fail");
        assert!(matches!(
            error,
            ConfigError::InvalidRemoteDir {
                field: "category.landing_zone.dir",
                ..
            }
        ));
    }

    #[test]
    fn rejects_empty_remote_user() {
        let error = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    landing_zone:
      kind: remote
      user: "  "
      host: "remote.example.org"
      port: 22
      dir: "/data/landing"
    completion_file_globs:
      - "x"
"#,
        )
        .expect_err("empty remote user should fail");
        assert!(matches!(
            error,
            ConfigError::EmptyField {
                field: "category.landing_zone.user"
            }
        ));
    }

    #[test]
    fn rejects_zero_remote_port() {
        let error = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    landing_zone:
      kind: remote
      user: "syncer"
      host: "remote.example.org"
      port: 0
      dir: "/data/landing"
    completion_file_globs:
      - "x"
"#,
        )
        .expect_err("zero remote port should fail");
        assert!(matches!(
            error,
            ConfigError::ZeroPort {
                field: "category.landing_zone.port"
            }
        ));
    }

    #[test]
    fn rejects_config_with_no_categories() {
        let error = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/sequencer"
"#,
        )
        .expect_err("config with no categories should fail");

        assert!(matches!(error, ConfigError::NoCategoriesConfigured));
    }

    #[test]
    fn rejects_relative_source_path() {
        let error = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "relative/data"

category:
  - regex: "^\\d{6}_"
    landing_zone:
      kind: local
      path: "/var/lib/sequencer/landing-zone"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        )
        .expect_err("relative source path should fail validation");

        assert!(matches!(
            error,
            ConfigError::PathNotAbsolute {
                field: "source",
                ..
            }
        ));
    }

    #[test]
    fn classify_matches_first_regex() {
        let config = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nanopore"

category:
  - regex: "^ONT_WGS_"
    landing_zone:
      kind: local
      path: "/landing/core"
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    landing_zone:
      kind: local
      path: "/landing/other"
    completion_file_globs:
      - "report*.html"
"#,
        )
        .unwrap();

        let matched = config
            .categories
            .iter()
            .find(|c| c.regex.is_match("ONT_WGS_run1"));
        assert_eq!(
            local_path(&matched.unwrap().landing_zone),
            &PathBuf::from("/landing/core")
        );

        let matched = config
            .categories
            .iter()
            .find(|c| c.regex.is_match("ONT_raw_run2"));
        assert_eq!(
            local_path(&matched.unwrap().landing_zone),
            &PathBuf::from("/landing/other")
        );
    }

    #[test]
    fn classify_returns_none_for_unmatched() {
        let config = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nanopore"

category:
  - regex: "^ONT_WGS_"
    landing_zone:
      kind: local
      path: "/landing/core"
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    landing_zone:
      kind: local
      path: "/landing/other"
    completion_file_globs:
      - "report*.html"
"#,
        )
        .unwrap();

        let matched = config
            .categories
            .iter()
            .find(|c| c.regex.is_match("ILLUMINA_run1"));
        assert!(matched.is_none());
    }

    #[test]
    fn rejects_empty_completion_glob_list() {
        let error = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    landing_zone:
      kind: local
      path: "/var/lib/sequencer/landing-zone"
    completion_file_globs: []
"#,
        )
        .expect_err("empty completion glob list should fail");

        assert!(matches!(
            error,
            ConfigError::EmptyGlobList {
                field: "category.completion_file_globs"
            }
        ));
    }

    #[test]
    fn landing_zone_with_subdir_local() {
        let zone = LandingZone::Local(PathBuf::from("/landing/core"));
        assert_eq!(
            zone.with_subdir("2024"),
            LandingZone::Local(PathBuf::from("/landing/core/2024"))
        );
    }

    #[test]
    fn landing_zone_with_subdir_remote_strips_trailing_slash() {
        let zone = LandingZone::Remote {
            user: "u".to_string(),
            host: "h".to_string(),
            port: 22,
            dir: OsString::from("/data/landing/"),
        };
        match zone.with_subdir("2024") {
            LandingZone::Remote { dir, .. } => {
                assert_eq!(dir, OsString::from("/data/landing/2024"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn landing_zone_join_run_remote() {
        let zone = LandingZone::Remote {
            user: "u".to_string(),
            host: "h".to_string(),
            port: 22,
            dir: OsString::from("/data/landing"),
        };
        match zone.join_run(OsStr::new("240101_NB001")) {
            LandingZone::Remote { dir, .. } => {
                assert_eq!(dir, OsString::from("/data/landing/240101_NB001"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn landing_zone_join_run_local_preserves_non_utf8_name() {
        let zone = LandingZone::Local(PathBuf::from("/landing"));
        let run_name = OsStr::from_bytes(b"ONT_\xff_run");
        assert_eq!(
            zone.join_run(run_name),
            LandingZone::Local(
                PathBuf::from("/landing").join(OsString::from_vec(b"ONT_\xff_run".to_vec()))
            )
        );
    }

    #[test]
    fn landing_zone_display_remote() {
        let zone = LandingZone::Remote {
            user: "u".to_string(),
            host: "h".to_string(),
            port: 2222,
            dir: OsString::from("/data/landing"),
        };
        assert_eq!(zone.display(), "u@h:2222:/data/landing");
    }
}
