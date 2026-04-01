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
    /// Canonicalized absolute path.
    pub source: PathBuf,
    pub categories: Vec<Category>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Destination {
    Local { path: PathBuf },
    Remote(RemoteDestination),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteDestination {
    // Transfer will ssh to <user>@<host>:/path
    pub user: String,
    pub host: String,
    pub port: u16,
    // Absolute path, but not canonicalized, because that cannot be
    // done on the local machine.
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct Category {
    pub regex: Regex,
    pub destination: Destination,
    pub exclude: Vec<String>,
    pub completion_file_globs: Vec<Pattern>,
    /// When true, place runs into a year-based subdirectory under the landing
    /// zone. The year is derived from the directory name by prepending "20" to
    /// its first two characters (e.g. "240101_NB123" -> "2024/").
    pub year_subdirectory: bool,
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
    destination: UnvalidatedDestination,
    #[serde(default)]
    exclude: Vec<String>,
    completion_file_globs: Vec<String>,
    #[serde(default)]
    year_subdirectory: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum UnvalidatedDestination {
    Local {
        path: PathBuf,
    },
    Remote {
        user: String,
        host: String,
        port: u16,
        path: PathBuf,
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
        "config fields `{first}` and `{second}` must not point to the same path: {}",
        path.display()
    )]
    DuplicatePath {
        first: &'static str,
        second: &'static str,
        path: PathBuf,
    },
    #[error(
        "config remote destinations `{first}` and `{second}` must not point to the same location: {user}@{host}:{path}",
        path = path.display()
    )]
    DuplicateRemoteDestination {
        first: &'static str,
        second: &'static str,
        user: String,
        host: String,
        port: u16,
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

        Self::from_yaml_str(&contents)
    }

    fn from_yaml_str(contents: &str) -> Result<Self, ConfigError> {
        let mut config = Self::parse_yaml_str_uncanonicalized(contents)?;
        config.canonicalize_paths()?;
        Ok(config)
    }

    fn parse_yaml_str_uncanonicalized(contents: &str) -> Result<Self, ConfigError> {
        let config: UnvalidatedConfig =
            serde_yaml::from_str(contents).map_err(ConfigError::Parse)?;
        config.validate()
    }

    /// Canonicalize all local path fields via the filesystem (resolving
    /// symlinks, `.`, and `..`), then check that no two local fields resolve
    /// to the same directory. Remote destination paths are validated
    /// syntactically and are never canonicalized locally.
    fn canonicalize_paths(&mut self) -> Result<(), ConfigError> {
        self.flockdir = canonicalize_field("flockdir", &self.flockdir)?;
        self.logdir = canonicalize_field("logdir", &self.logdir)?;
        self.source = canonicalize_field("source", &self.source)?;

        for cat in &mut self.categories {
            if let Destination::Local { path } = &mut cat.destination {
                *path = canonicalize_field("category.destination.path", path)?;
            }
        }

        let mut local_paths: Vec<(&'static str, &Path)> = vec![
            ("source", &self.source),
            ("flockdir", &self.flockdir),
            ("logdir", &self.logdir),
        ];
        for cat in &self.categories {
            if let Destination::Local { path } = &cat.destination {
                local_paths.push(("category.destination.path", path));
            }
        }
        validate_all_paths_distinct(&local_paths)?;
        validate_all_remote_destinations_distinct(&self.categories)?;

        Ok(())
    }
}

impl Destination {
    pub fn display(&self) -> String {
        match self {
            Self::Local { path } => path.display().to_string(),
            Self::Remote(remote) => remote.display(),
        }
    }

    pub fn with_appended_relative_path(&self, relative: &Path) -> Self {
        match self {
            Self::Local { path } => Self::Local {
                path: path.join(relative),
            },
            Self::Remote(remote) => Self::Remote(RemoteDestination {
                user: remote.user.clone(),
                host: remote.host.clone(),
                port: remote.port,
                path: remote.path.join(relative),
            }),
        }
    }
}

impl RemoteDestination {
    pub fn display(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.path.display())
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
        let regex = Regex::new(&self.regex).map_err(|source| ConfigError::InvalidRegex {
            field: "category.regex",
            source,
        })?;

        let destination = self.destination.validate()?;

        let completion_file_globs = validate_globs(
            "category.completion_file_globs",
            &self.completion_file_globs,
        )?;

        Ok(Category {
            regex,
            destination,
            exclude: self.exclude,
            completion_file_globs,
            year_subdirectory: self.year_subdirectory,
        })
    }
}

impl UnvalidatedDestination {
    fn validate(self) -> Result<Destination, ConfigError> {
        match self {
            Self::Local { path } => {
                validate_absolute_path("category.destination.path", &path)?;
                Ok(Destination::Local { path })
            }
            Self::Remote {
                user,
                host,
                port,
                path,
            } => {
                validate_non_empty("category.destination.user", &user)?;
                validate_non_empty("category.destination.host", &host)?;
                if port == 0 {
                    return Err(ConfigError::ZeroPort {
                        field: "category.destination.port",
                    });
                }
                validate_absolute_path("category.destination.path", &path)?;
                Ok(Destination::Remote(RemoteDestination {
                    user,
                    host,
                    port,
                    path,
                }))
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

fn validate_all_remote_destinations_distinct(categories: &[Category]) -> Result<(), ConfigError> {
    for i in 0..categories.len() {
        let Destination::Remote(first) = &categories[i].destination else {
            continue;
        };

        for second_category in categories.iter().skip(i + 1) {
            let Destination::Remote(second) = &second_category.destination else {
                continue;
            };

            if first == second {
                return Err(ConfigError::DuplicateRemoteDestination {
                    first: "category.destination",
                    second: "category.destination",
                    user: first.user.clone(),
                    host: first.host.clone(),
                    port: first.port,
                    path: first.path.clone(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Config, ConfigError, Destination, RemoteDestination};

    const EXAMPLE_CONFIG: &str = include_str!("../examples/config.yaml");
    const NEXTSEQ_EXAMPLE: &str = r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    destination:
      type: local
      path: "/var/lib/sequencer/landing-zone"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#;

    #[test]
    fn parses_example_config() {
        let config = Config::parse_yaml_str_uncanonicalized(EXAMPLE_CONFIG)
            .expect("nanopore config should parse");

        assert_eq!(config.flockdir, PathBuf::from("/var/lib/sequencer/flock"));
        assert_eq!(config.lock_file_name, "sequencer-sync.lock");
        assert_eq!(config.logdir, PathBuf::from("/var/lib/sequencer/log"));
        assert_eq!(config.source, PathBuf::from("/data/nanopore"));

        assert_eq!(config.categories.len(), 2);
        assert!(config.categories[0].regex.is_match("ONT_WGS_run1"));
        assert!(!config.categories[0].regex.is_match("ONT_raw_run2"));
        assert_eq!(
            config.categories[0].destination,
            Destination::Local {
                path: PathBuf::from("/var/lib/sequencer/landing-zone-core"),
            }
        );
        assert!(config.categories[1].regex.is_match("ONT_raw_run2"));
        assert_eq!(
            config.categories[1].destination,
            Destination::Remote(RemoteDestination {
                user: "sequencer-sync".to_string(),
                host: "sequencer.example.org".to_string(),
                port: 22,
                path: PathBuf::from("/srv/sequencer/landing-zone-other"),
            })
        );
    }

    #[test]
    fn parses_nextseq_example_config() {
        let config = Config::parse_yaml_str_uncanonicalized(NEXTSEQ_EXAMPLE)
            .expect("nextseq config should parse");

        assert_eq!(config.flockdir, PathBuf::from("/var/lib/sequencer/flock"));
        assert_eq!(config.logdir, PathBuf::from("/var/lib/sequencer/log"));
        assert_eq!(config.source, PathBuf::from("/data/nextseq"));
        assert_eq!(config.categories.len(), 1);
        assert!(config.categories[0].regex.is_match("240101_"));
        assert_eq!(
            config.categories[0].destination,
            Destination::Local {
                path: PathBuf::from("/var/lib/sequencer/landing-zone"),
            }
        );
    }

    #[test]
    fn parses_remote_destination() {
        let config = Config::parse_yaml_str_uncanonicalized(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nanopore"

category:
  - regex: "^ONT_"
    destination:
      type: remote
      user: "alice"
      host: "example.org"
      port: 2222
      path: "/incoming/ont"
    completion_file_globs:
      - "report*.html"
"#,
        )
        .expect("remote config should parse");

        assert_eq!(
            config.categories[0].destination,
            Destination::Remote(RemoteDestination {
                user: "alice".to_string(),
                host: "example.org".to_string(),
                port: 2222,
                path: PathBuf::from("/incoming/ont"),
            })
        );
    }

    #[test]
    fn rejects_config_with_no_categories() {
        let error = Config::parse_yaml_str_uncanonicalized(
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
        let error = Config::parse_yaml_str_uncanonicalized(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "relative/data"

category:
  - regex: "^\\d{6}_"
    destination:
      type: local
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
    fn rejects_empty_remote_user() {
        let error = Config::parse_yaml_str_uncanonicalized(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/sequencer"

category:
  - regex: "^\\d{6}_"
    destination:
      type: remote
      user: "   "
      host: "sequencer.example.org"
      port: 22
      path: "/landing"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        )
        .expect_err("empty remote user should fail validation");

        assert!(matches!(
            error,
            ConfigError::EmptyField {
                field: "category.destination.user"
            }
        ));
    }

    #[test]
    fn rejects_missing_remote_port() {
        let error = Config::parse_yaml_str_uncanonicalized(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/sequencer"

category:
  - regex: "^\\d{6}_"
    destination:
      type: remote
      user: "alice"
      host: "sequencer.example.org"
      path: "/landing"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        )
        .expect_err("missing remote port should fail validation");

        assert!(matches!(error, ConfigError::Parse(_)));
    }

    #[test]
    fn classify_matches_first_regex() {
        let config = Config::parse_yaml_str_uncanonicalized(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nanopore"

category:
  - regex: "^ONT_WGS_"
    destination:
      type: local
      path: "/landing/core"
    completion_file_globs:
      - "report*.html"

  - regex: "^ONT_"
    destination:
      type: local
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
            matched.unwrap().destination,
            Destination::Local {
                path: PathBuf::from("/landing/core"),
            }
        );

        let matched = config
            .categories
            .iter()
            .find(|c| c.regex.is_match("ONT_raw_run2"));
        assert_eq!(
            matched.unwrap().destination,
            Destination::Local {
                path: PathBuf::from("/landing/other"),
            }
        );
    }

    #[test]
    fn rejects_empty_completion_glob_list() {
        let error = Config::parse_yaml_str_uncanonicalized(
            r#"
flockdir: "/var/lib/sequencer/flock"
lock_file_name: "sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
source: "/data/nextseq"

category:
  - regex: "^\\d{6}_"
    destination:
      type: local
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
}
