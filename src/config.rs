use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use glob::Pattern;
use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

const SUPPORTED_CONFIG_VERSION: u16 = 2;

#[derive(Debug)]
pub struct Config {
    /// Absolute path to the lock file. Its parent directory is canonicalized.
    pub lock_file: PathBuf,
    /// Canonicalized absolute path. Directory for log files (transfer log, run
    /// log, cron file).
    pub logdir: PathBuf,

    // You must have ssh access to this server with this port, user name.
    pub server_user: String,
    pub server_port: u16,
    pub server_host: String,

    /// Canonicalized absolute path.
    pub source: PathBuf,
    // Stored in Arcs, because it's nice that the Category object
    // also has a reference to them
    pub filestructures: HashMap<String, Arc<FileStructure>>,
    pub categories: Vec<Category>,
}

#[derive(Debug)]
pub struct FileStructure {
    pub name: String,
    // We split ignore/checkout into paths and globs because matching a path
    // is much faster, as it's just a hash check.
    // Files matching these relative paths are not transferred
    pub ignore_paths: HashSet<PathBuf>,
    // Files matching these patterns are not transferred
    pub ignore_globs: Vec<Pattern>,
    // Files matching these relative paths are not archived in landing zone
    pub checkout_paths: HashSet<PathBuf>,
    // Files matching these patterns are not archived in landing zone
    pub checkout_globs: Vec<Pattern>,
    pub completion_file_globs: Vec<Pattern>,
}

#[derive(Debug)]
pub struct Category {
    pub regex: Regex,
    pub classification_glob: Option<Pattern>,
    /// Canonicalized absolute path.
    pub landing_zone: PathBuf,
    pub filestructure: Arc<FileStructure>,
    /// When true, place runs into a year-based subdirectory under the landing
    /// zone. The year is derived from the directory name by prepending "20" to
    /// its first two characters (e.g. "240101_NB123" → "2024/").
    pub year_subdirectory: bool,
}

#[derive(Debug, Deserialize)]
struct ConfigHeader {
    version: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnvalidatedConfig {
    version: u16,
    lock_file: PathBuf,
    logdir: PathBuf,
    server_user: String,
    server_port: u16,
    server_host: String,
    source: PathBuf,
    filestructures: HashMap<String, UnvalidatedFileStructure>,
    #[serde(default)]
    category: Vec<UnvalidatedCategory>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnvalidatedFileStructure {
    #[serde(default)]
    ignore_globs: Vec<String>,
    checkout_globs: Vec<String>,
    completion_file_globs: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnvalidatedCategory {
    regex: String,
    classification_glob: Option<String>,
    landing_zone: PathBuf,
    filestructure: String,
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
    #[error("unsupported config version {found}; this binary supports config version {supported}")]
    UnsupportedVersion { found: u16, supported: u16 },
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
    #[error("config must contain at least one filestructure")]
    NoFileStructuresConfigured,
    #[error("config category references unknown filestructure `{name}`")]
    UnknownFileStructure { name: String },
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
    #[error("Expected {label} directory to exist: '{}'", path.display())]
    MissingDirectory { label: &'static str, path: PathBuf },
    #[error("Expected {label} to be a directory: '{}'", path.display())]
    NotDirectory { label: &'static str, path: PathBuf },
    #[error("failed to inspect {label} directory '{}': {source}", path.display())]
    ReadDirectoryMetadata {
        label: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
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
        let config: UnvalidatedConfig = match serde_yaml::from_str(contents) {
            Ok(config) => config,
            Err(parse_error) => {
                if let Ok(header) = serde_yaml::from_str::<ConfigHeader>(contents) {
                    validate_config_version(header.version)?;
                }
                return Err(ConfigError::Parse(parse_error));
            }
        };

        validate_config_version(config.version)?;
        config.validate()
    }

    /// Canonicalize all path fields via the filesystem (resolving symlinks,
    /// `.`, and `..`), then check that no two fields resolve to the same
    /// directory.
    fn canonicalize_paths(&mut self) -> Result<(), ConfigError> {
        self.lock_file = canonicalize_lock_file(&self.lock_file)?;
        self.logdir = canonicalize_field("logdir", &self.logdir)?;
        self.source = canonicalize_field("source", &self.source)?;

        for cat in &mut self.categories {
            cat.landing_zone = canonicalize_field("category.landing_zone", &cat.landing_zone)?;
        }

        let mut paths: Vec<(&'static str, &Path)> = vec![
            ("source", &self.source),
            ("lock_file parent", lock_file_parent(&self.lock_file)?),
            ("logdir", &self.logdir),
        ];
        for cat in &self.categories {
            paths.push(("category.landing_zone", &cat.landing_zone));
        }
        validate_all_paths_distinct(&paths)?;

        Ok(())
    }
}

fn validate_config_version(version: u16) -> Result<(), ConfigError> {
    if version == SUPPORTED_CONFIG_VERSION {
        Ok(())
    } else {
        Err(ConfigError::UnsupportedVersion {
            found: version,
            supported: SUPPORTED_CONFIG_VERSION,
        })
    }
}

fn canonicalize_field(field: &'static str, path: &Path) -> Result<PathBuf, ConfigError> {
    validate_existing_directory(field, path)?;
    fs::canonicalize(path).map_err(|source| ConfigError::CanonicalizePath {
        field,
        path: path.to_path_buf(),
        source,
    })
}

fn canonicalize_lock_file(path: &Path) -> Result<PathBuf, ConfigError> {
    let parent = lock_file_parent(path)?;
    validate_existing_directory("lock_file parent", parent)?;
    let canonical_parent =
        fs::canonicalize(parent).map_err(|source| ConfigError::CanonicalizePath {
            field: "lock_file parent",
            path: parent.to_path_buf(),
            source,
        })?;
    let file_name = path
        .file_name()
        .expect("lock_file_parent rejects paths without a file name");
    Ok(canonical_parent.join(file_name))
}

fn lock_file_parent(path: &Path) -> Result<&Path, ConfigError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    validate_base_name("lock_file", file_name)?;
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| ConfigError::InvalidBaseName {
            field: "lock_file",
            value: path.display().to_string(),
        })
}

fn validate_existing_directory(field: &'static str, path: &Path) -> Result<(), ConfigError> {
    let label = directory_label(field);
    let metadata = fs::metadata(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ConfigError::MissingDirectory {
                label,
                path: path.to_path_buf(),
            }
        } else {
            ConfigError::ReadDirectoryMetadata {
                label,
                path: path.to_path_buf(),
                source,
            }
        }
    })?;

    if metadata.is_dir() {
        Ok(())
    } else {
        Err(ConfigError::NotDirectory {
            label,
            path: path.to_path_buf(),
        })
    }
}

fn directory_label(field: &'static str) -> &'static str {
    match field {
        "lock_file parent" => "lock",
        "logdir" => "log",
        "source" => "source",
        "category.landing_zone" => "landing zone",
        _ => field,
    }
}

impl UnvalidatedConfig {
    fn validate(self) -> Result<Config, ConfigError> {
        validate_absolute_path("lock_file", &self.lock_file)?;
        let _ = lock_file_parent(&self.lock_file)?;
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
        if self.filestructures.is_empty() {
            return Err(ConfigError::NoFileStructuresConfigured);
        }

        let mut filestructures = HashMap::with_capacity(self.filestructures.len());
        for (name, filestructure) in self.filestructures {
            validate_non_empty("filestructures key", &name)?;
            filestructures.insert(name.clone(), Arc::new(filestructure.validate(name)?));
        }

        let mut categories = Vec::with_capacity(self.category.len());
        for cat in self.category {
            categories.push(cat.validate(&filestructures)?);
        }

        Ok(Config {
            lock_file: self.lock_file,
            logdir: self.logdir,
            server_user: self.server_user,
            server_port: self.server_port,
            server_host: self.server_host,
            source: self.source,
            filestructures,
            categories,
        })
    }
}

impl UnvalidatedFileStructure {
    fn validate(self, name: String) -> Result<FileStructure, ConfigError> {
        let (ignore_paths, ignore_globs) =
            validate_file_patterns("filestructures.*.ignore_globs", &self.ignore_globs)?;
        let (checkout_paths, checkout_globs) =
            validate_file_patterns("filestructures.*.checkout_globs", &self.checkout_globs)?;
        let completion_file_globs = validate_globs(
            "filestructures.*.completion_file_globs",
            &self.completion_file_globs,
        )?;
        Ok(FileStructure {
            name,
            ignore_paths,
            ignore_globs,
            checkout_paths,
            checkout_globs,
            completion_file_globs,
        })
    }
}

impl UnvalidatedCategory {
    fn validate(
        self,
        filestructures: &HashMap<String, Arc<FileStructure>>,
    ) -> Result<Category, ConfigError> {
        validate_absolute_path("category.landing_zone", &self.landing_zone)?;
        validate_non_empty("category.filestructure", &self.filestructure)?;

        let regex = Regex::new(&self.regex).map_err(|source| ConfigError::InvalidRegex {
            field: "category.regex",
            source,
        })?;

        let classification_glob = self
            .classification_glob
            .as_deref()
            .map(|pattern| validate_glob("category.classification_glob", pattern))
            .transpose()?;

        let filestructure = filestructures
            .get(&self.filestructure)
            .cloned()
            .ok_or_else(|| ConfigError::UnknownFileStructure {
                name: self.filestructure.clone(),
            })?;

        Ok(Category {
            regex,
            classification_glob,
            landing_zone: self.landing_zone,
            filestructure,
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

fn validate_file_patterns(
    field: &'static str,
    patterns: &[String],
) -> Result<(HashSet<PathBuf>, Vec<Pattern>), ConfigError> {
    let mut literal_paths = HashSet::new();
    let mut glob_patterns = Vec::new();

    for pattern in patterns {
        let glob = validate_glob(field, pattern)?;
        if is_literal_glob(pattern) {
            literal_paths.insert(PathBuf::from(pattern));
        } else {
            glob_patterns.push(glob);
        }
    }

    Ok((literal_paths, glob_patterns))
}

fn is_literal_glob(pattern: &str) -> bool {
    !pattern.contains(['*', '?', '[', ']'])
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{Config, ConfigError};

    const EXAMPLE_CONFIG: &str = include_str!("../examples/config.yaml");
    const NEXTSEQ_EXAMPLE: &str = r#"
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nextseq"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "data.txt"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"

category:
  - regex: "^\\d{6}_"
    landing_zone: "/var/lib/sequencer/landing-zone"
    filestructure: "default"
"#;
    static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn parses_example_config() {
        let config = Config::from_yaml_str(EXAMPLE_CONFIG).expect("nanopore config should parse");

        assert_eq!(
            config.lock_file,
            PathBuf::from("/var/lib/sequencer/flock/sequencer-sync.lock")
        );
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

        assert_eq!(
            config.lock_file,
            PathBuf::from("/var/lib/sequencer/flock/sequencer-sync.lock")
        );
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
    fn rejects_unsupported_config_version() {
        let contents = NEXTSEQ_EXAMPLE.replace("version: 2", "version: 3");

        let error =
            Config::from_yaml_str(&contents).expect_err("unsupported config version should fail");

        assert!(matches!(
            error,
            ConfigError::UnsupportedVersion {
                found: 3,
                supported: 2
            }
        ));
    }

    #[test]
    fn rejects_unsupported_config_version_before_reporting_incompatible_shape() {
        let contents =
            NEXTSEQ_EXAMPLE.replacen("version: 2", "version: 3\nfuture_required_field: true", 1);

        let error = Config::from_yaml_str(&contents)
            .expect_err("unsupported future config shape should report version first");

        assert!(matches!(
            error,
            ConfigError::UnsupportedVersion {
                found: 3,
                supported: 2
            }
        ));
    }

    #[test]
    fn rejects_missing_config_version() {
        let contents = NEXTSEQ_EXAMPLE.replace("version: 2\n", "");

        let error =
            Config::from_yaml_str(&contents).expect_err("missing config version should fail");

        assert!(matches!(error, ConfigError::Parse(_)));
    }

    #[test]
    fn rejects_config_with_no_categories() {
        let error = Config::from_yaml_str(
            r#"
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/sequencer"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "report*.html"
    completion_file_globs:
      - "report*.html"
"#,
        )
        .expect_err("config with no categories should fail");

        assert!(matches!(error, ConfigError::NoCategoriesConfigured));
    }

    #[test]
    fn rejects_relative_source_path() {
        let error = Config::from_yaml_str(
            r#"
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "relative/data"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "data.txt"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"

category:
  - regex: "^\\d{6}_"
    landing_zone: "/var/lib/sequencer/landing-zone"
    filestructure: "default"
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
    fn rejects_unknown_filestructure_reference() {
        let error = Config::from_yaml_str(
            r#"
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/sequencer"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "report*.html"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^run"
    landing_zone: "/var/lib/sequencer/landing-zone"
    filestructure: "missing"
"#,
        )
        .expect_err("unknown filestructure reference should fail");

        assert!(matches!(
            error,
            ConfigError::UnknownFileStructure { name } if name == "missing"
        ));
    }

    #[test]
    fn accepts_empty_checkout_globs() {
        let config = Config::from_yaml_str(
            r#"
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/sequencer"
filestructures:
  default:
    ignore_globs: []
    checkout_globs: []
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^run"
    landing_zone: "/var/lib/sequencer/landing-zone"
    filestructure: "default"
"#,
        )
        .expect("empty checkout_globs should be valid");

        assert!(config.categories[0].filestructure.checkout_globs.is_empty());
    }

    #[test]
    fn stores_literal_filestructure_patterns_as_paths() {
        let config = Config::from_yaml_str(
            r#"
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/sequencer"
filestructures:
  default:
    ignore_globs:
      - "skip/file.txt"
    checkout_globs:
      - "report.txt"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^run"
    landing_zone: "/var/lib/sequencer/landing-zone"
    filestructure: "default"
"#,
        )
        .expect("literal filestructure patterns should be valid");
        let filestructure = &config.categories[0].filestructure;

        assert!(
            filestructure
                .ignore_paths
                .contains(Path::new("skip/file.txt"))
        );
        assert!(filestructure.ignore_globs.is_empty());
        assert!(
            filestructure
                .checkout_paths
                .contains(Path::new("report.txt"))
        );
        assert!(filestructure.checkout_globs.is_empty());
    }

    #[test]
    fn stores_wildcard_filestructure_patterns_as_globs() {
        let config = Config::from_yaml_str(
            r#"
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/sequencer"
filestructures:
  default:
    ignore_globs:
      - "skip/*.txt"
    checkout_globs:
      - "report?.txt"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^run"
    landing_zone: "/var/lib/sequencer/landing-zone"
    filestructure: "default"
"#,
        )
        .expect("wildcard filestructure patterns should be valid");
        let filestructure = &config.categories[0].filestructure;

        assert!(filestructure.ignore_paths.is_empty());
        assert_eq!(filestructure.ignore_globs.len(), 1);
        assert!(filestructure.checkout_paths.is_empty());
        assert_eq!(filestructure.checkout_globs.len(), 1);
    }

    #[test]
    fn rejects_invalid_filestructure_glob() {
        let error = Config::from_yaml_str(
            r#"
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/sequencer"
filestructures:
  default:
    ignore_globs:
      - "["
    checkout_globs:
      - "report*.html"
    completion_file_globs:
      - "report*.html"

category:
  - regex: "^run"
    landing_zone: "/var/lib/sequencer/landing-zone"
    filestructure: "default"
"#,
        )
        .expect_err("invalid ignore_globs pattern should fail");

        assert!(matches!(
            error,
            ConfigError::InvalidGlob {
                field: "filestructures.*.ignore_globs",
                ..
            }
        ));
    }

    #[test]
    fn rejects_empty_server_user() {
        let error = Config::from_yaml_str(
            r#"
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "   "
server_port: 22
server_host: "sequencer.example.org"
source: "/data/sequencer"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "data.txt"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"

category:
  - regex: "^\\d{6}_"
    landing_zone: "/var/lib/sequencer/landing-zone"
    filestructure: "default"
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
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nanopore"
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
    landing_zone: "/landing/core"
    filestructure: "default"

  - regex: "^ONT_"
    landing_zone: "/landing/other"
    filestructure: "default"
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
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nanopore"
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
    landing_zone: "/landing/core"
    filestructure: "default"

  - regex: "^ONT_"
    landing_zone: "/landing/other"
    filestructure: "default"
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
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nanopore"
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
    landing_zone: "/landing/core"
    filestructure: "default"

  - regex: "^ONT_"
    landing_zone: "/landing/other"
    filestructure: "default"
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
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nextseq"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "data.txt"
    completion_file_globs: []

category:
  - regex: "^\\d{6}_"
    landing_zone: "/var/lib/sequencer/landing-zone"
    filestructure: "default"
"#,
        )
        .expect_err("empty completion glob list should fail");

        assert!(matches!(
            error,
            ConfigError::EmptyGlobList {
                field: "filestructures.*.completion_file_globs"
            }
        ));
    }

    #[test]
    fn parses_classification_glob() {
        let config = Config::from_yaml_str(
            r#"
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nextseq"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "data.txt"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"

category:
  - regex: "^\\d{6}_"
    classification_glob: "Analysis/*/SampleSheet.csv"
    landing_zone: "/var/lib/sequencer/landing-zone"
    filestructure: "default"
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
version: 2
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/nextseq"
filestructures:
  default:
    ignore_globs: []
    checkout_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
      - "data.txt"
    completion_file_globs:
      - "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"

category:
  - regex: "^\\d{6}_"
    classification_glob: "["
    landing_zone: "/var/lib/sequencer/landing-zone"
    filestructure: "default"
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

    #[test]
    fn from_path_reports_missing_landing_zone_before_canonicalizing() {
        let tempdir = make_temp_dir();
        fs::create_dir(tempdir.join("flockdir")).expect("should create flockdir");
        fs::create_dir(tempdir.join("logdir")).expect("should create logdir");
        fs::create_dir(tempdir.join("source")).expect("should create source");
        let missing_landing = tempdir.join("missing-landing");
        let config_path = tempdir.join("config.yaml");
        fs::write(
            &config_path,
            format!(
                r#"
version: 2
lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
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
  - regex: "^run"
    landing_zone: "{landing_zone}"
    filestructure: "default"
"#,
                flockdir = tempdir.join("flockdir").display(),
                logdir = tempdir.join("logdir").display(),
                source = tempdir.join("source").display(),
                landing_zone = missing_landing.display(),
            ),
        )
        .expect("should write config");

        let error = Config::from_path(&config_path).expect_err("missing landing zone should fail");

        assert!(matches!(
            error,
            ConfigError::MissingDirectory {
                label: "landing zone",
                ..
            }
        ));
        assert_eq!(
            error.to_string(),
            format!(
                "Expected landing zone directory to exist: '{}'",
                missing_landing.display()
            )
        );
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn from_path_reports_file_where_directory_expected_before_canonicalizing() {
        let tempdir = make_temp_dir();
        fs::create_dir(tempdir.join("flockdir")).expect("should create flockdir");
        fs::create_dir(tempdir.join("logdir")).expect("should create logdir");
        fs::create_dir(tempdir.join("source")).expect("should create source");
        let landing_file = tempdir.join("landing-file");
        fs::write(&landing_file, "").expect("should create landing file");
        let config_path = tempdir.join("config.yaml");
        fs::write(
            &config_path,
            format!(
                r#"
version: 2
lock_file: "{flockdir}/sequencer-sync.lock"
logdir: "{logdir}"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
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
  - regex: "^run"
    landing_zone: "{landing_zone}"
    filestructure: "default"
"#,
                flockdir = tempdir.join("flockdir").display(),
                logdir = tempdir.join("logdir").display(),
                source = tempdir.join("source").display(),
                landing_zone = landing_file.display(),
            ),
        )
        .expect("should write config");

        let error = Config::from_path(&config_path).expect_err("landing file should fail");

        assert!(matches!(
            error,
            ConfigError::NotDirectory {
                label: "landing zone",
                ..
            }
        ));
        cleanup_temp_dir(&tempdir);
    }

    fn make_temp_dir() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let unique_id = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sequencer-sync-config-test-{}-{timestamp}-{unique_id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("should create temp dir");
        path
    }

    fn cleanup_temp_dir(path: &Path) {
        fs::remove_dir_all(path).expect("should remove temp dir");
    }
}
