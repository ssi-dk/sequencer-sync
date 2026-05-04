use std::fs;
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

    // You must have ssh access to this server with this port, user name.
    pub server_user: String,
    pub server_port: u16,
    pub server_host: String,

    /// Canonicalized absolute path.
    pub source: PathBuf,
    pub categories: Vec<Category>,
}

#[derive(Debug)]
pub struct Category {
    pub regex: Regex,
    pub classification_glob: Option<Pattern>,
    /// Canonicalized absolute path.
    pub landing_zone: PathBuf,
    pub exclude: Vec<String>,
    pub completion_file_globs: Vec<Pattern>,
    /// When true, place runs into a year-based subdirectory under the landing
    /// zone. The year is derived from the directory name by prepending "20" to
    /// its first two characters (e.g. "240101_NB123" → "2024/").
    pub year_subdirectory: bool,
}

#[derive(Debug, Deserialize)]
struct UnvalidatedConfig {
    flockdir: PathBuf,
    lock_file_name: String,
    logdir: PathBuf,
    server_user: String,
    server_port: u16,
    server_host: String,
    source: PathBuf,
    #[serde(default)]
    category: Vec<UnvalidatedCategory>,
}

#[derive(Debug, Deserialize)]
struct UnvalidatedCategory {
    regex: String,
    classification_glob: Option<String>,
    landing_zone: PathBuf,
    #[serde(default)]
    exclude: Vec<String>,
    completion_file_globs: Vec<String>,
    #[serde(default)]
    year_subdirectory: bool,
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
    pub(crate) fn from_yaml_str(contents: &str) -> Result<Self, ConfigError> {
        let config: UnvalidatedConfig =
            serde_yaml::from_str(contents).map_err(ConfigError::Parse)?;
        config.validate()
    }

    /// Canonicalize all path fields via the filesystem (resolving symlinks,
    /// `.`, and `..`), then check that no two fields resolve to the same
    /// directory.
    fn canonicalize_paths(&mut self) -> Result<(), ConfigError> {
        self.flockdir = canonicalize_field("flockdir", &self.flockdir)?;
        self.logdir = canonicalize_field("logdir", &self.logdir)?;
        self.source = canonicalize_field("source", &self.source)?;

        for cat in &mut self.categories {
            cat.landing_zone = canonicalize_field("category.landing_zone", &cat.landing_zone)?;
        }

        let mut paths: Vec<(&'static str, &Path)> = vec![
            ("source", &self.source),
            ("flockdir", &self.flockdir),
            ("logdir", &self.logdir),
        ];
        for cat in &self.categories {
            paths.push(("category.landing_zone", &cat.landing_zone));
        }
        validate_all_paths_distinct(&paths)?;

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
        validate_non_empty("server_user", &self.server_user)?;
        validate_non_empty("server_host", &self.server_host)?;

        if self.server_port == 0 {
            return Err(ConfigError::ZeroPort {
                field: "server_port",
            });
        }

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
            server_user: self.server_user,
            server_port: self.server_port,
            server_host: self.server_host,
            source: self.source,
            categories,
        })
    }
}

impl UnvalidatedCategory {
    fn validate(self) -> Result<Category, ConfigError> {
        validate_absolute_path("category.landing_zone", &self.landing_zone)?;

        let regex = Regex::new(&self.regex).map_err(|source| ConfigError::InvalidRegex {
            field: "category.regex",
            source,
        })?;

        let classification_glob = self
            .classification_glob
            .as_deref()
            .map(|pattern| validate_glob("category.classification_glob", pattern))
            .transpose()?;

        let completion_file_globs = validate_globs(
            "category.completion_file_globs",
            &self.completion_file_globs,
        )?;

        Ok(Category {
            regex,
            classification_glob,
            landing_zone: self.landing_zone,
            exclude: self.exclude,
            completion_file_globs,
            year_subdirectory: self.year_subdirectory,
        })
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
    use std::path::PathBuf;

    use super::{Config, ConfigError};

    const EXAMPLE_CONFIG: &str = include_str!("../examples/config.yaml");
    const NEXTSEQ_EXAMPLE: &str = r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    landing_zone: "/var/lib/sequencer/landing-zone"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#;

    #[test]
    fn parses_example_config() {
        let config = Config::from_yaml_str(EXAMPLE_CONFIG).expect("nanopore config should parse");

        assert_eq!(config.flockdir, PathBuf::from("/var/lib/sequencer/flock"));
        assert_eq!(config.lock_file_name, "sequencer-sync.lock");
        assert_eq!(config.logdir, PathBuf::from("/var/lib/sequencer/log"));
        assert_eq!(config.server_user, "sequencer-sync");
        assert_eq!(config.server_port, 22);
        assert_eq!(config.server_host, "sequencer.example.org");
        assert_eq!(config.source, PathBuf::from("/data/nanopore"));

        assert_eq!(config.categories.len(), 2);
        assert!(config.categories[0].regex.is_match("ONT_WGS_run1"));
        assert!(config.categories[0].regex.is_match("ONT_raw_run2"));
        assert!(
            config.categories[0]
                .classification_glob
                .as_ref()
                .expect("example core category should have classification glob")
                .matches("metadata/core_facility.txt")
        );
        assert_eq!(
            config.categories[0].landing_zone,
            PathBuf::from("/var/lib/sequencer/landing-zone-core")
        );
        assert!(config.categories[1].regex.is_match("ONT_raw_run2"));
        assert_eq!(
            config.categories[1].landing_zone,
            PathBuf::from("/var/lib/sequencer/landing-zone-other")
        );
    }

    #[test]
    fn parses_nextseq_example_config() {
        let config = Config::from_yaml_str(NEXTSEQ_EXAMPLE).expect("nextseq config should parse");

        assert_eq!(config.flockdir, PathBuf::from("/var/lib/sequencer/flock"));
        assert_eq!(config.logdir, PathBuf::from("/var/lib/sequencer/log"));
        assert_eq!(config.source, PathBuf::from("/data/nextseq"));
        assert_eq!(config.categories.len(), 1);
        assert!(config.categories[0].regex.is_match("240101_"));
        assert!(config.categories[0].classification_glob.is_none());
        assert_eq!(
            config.categories[0].landing_zone,
            PathBuf::from("/var/lib/sequencer/landing-zone")
        );
    }

    #[test]
    fn rejects_config_with_no_categories() {
        let error = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
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
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "relative/data"

category:
  - regex: "^\\d{6}_"
    landing_zone: "/var/lib/sequencer/landing-zone"
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
    fn rejects_empty_server_user() {
        let error = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "   "
server_port: 22
server_host: "sequencer.example.org"
source: "/data/sequencer"

category:
  - regex: "^\\d{6}_"
    landing_zone: "/var/lib/sequencer/landing-zone"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        )
        .expect_err("empty server_user should fail validation");

        assert!(matches!(
            error,
            ConfigError::EmptyField {
                field: "server_user"
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
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nanopore"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "/landing/core"
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    landing_zone: "/landing/other"
    completion_file_globs:
      - "report*.html"
"#,
        )
        .unwrap();

        // ONT_WGS_ matches first category
        let matched = config
            .categories
            .iter()
            .find(|c| c.regex.is_match("ONT_WGS_run1"));
        assert_eq!(
            matched.unwrap().landing_zone,
            PathBuf::from("/landing/core")
        );

        // ONT_raw_ matches second category (first doesn't match)
        let matched = config
            .categories
            .iter()
            .find(|c| c.regex.is_match("ONT_raw_run2"));
        assert_eq!(
            matched.unwrap().landing_zone,
            PathBuf::from("/landing/other")
        );
    }

    #[test]
    fn classify_first_match_wins() {
        let config = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nanopore"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "/landing/core"
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    landing_zone: "/landing/other"
    completion_file_globs:
      - "report*.html"
"#,
        )
        .unwrap();

        // ONT_WGS_run1 matches both regexes but first-match wins
        let matched = config
            .categories
            .iter()
            .find(|c| c.regex.is_match("ONT_WGS_run1"))
            .unwrap();
        assert_eq!(matched.landing_zone, PathBuf::from("/landing/core"));
    }

    #[test]
    fn classify_returns_none_for_unmatched() {
        let config = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nanopore"

category:
  - regex: "^ONT_WGS_"
    landing_zone: "/landing/core"
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    landing_zone: "/landing/other"
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
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    landing_zone: "/var/lib/sequencer/landing-zone"
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
    fn parses_classification_glob() {
        let config = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    classification_glob: "Analysis/*/SampleSheet.csv"
    landing_zone: "/var/lib/sequencer/landing-zone"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        )
        .expect("classification_glob should parse");

        let classification_glob = config.categories[0]
            .classification_glob
            .as_ref()
            .expect("classification glob should be present");
        assert!(classification_glob.matches("Analysis/1/SampleSheet.csv"));
        assert!(!classification_glob.matches("InterOp/IndexMetricsOut.bin"));
    }

    #[test]
    fn rejects_invalid_classification_glob() {
        let error = Config::from_yaml_str(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    classification_glob: "["
    landing_zone: "/var/lib/sequencer/landing-zone"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        )
        .expect_err("invalid classification_glob should fail");

        assert!(matches!(
            error,
            ConfigError::InvalidGlob {
                field: "category.classification_glob",
                ..
            }
        ));
    }
}
