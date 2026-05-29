use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use glob::Pattern;
use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

use crate::paths::{CanonicalChildFileBuf, CanonicalDirBuf, PathError};

const SUPPORTED_CONFIG_VERSION: u16 = 3;

// Fully validated and filesystem-resolved config object
#[derive(Debug)]
pub struct Config {
    pub lock_file: CanonicalChildFileBuf,

    /// Directory for log files (transfer log, run log, cron file).
    pub logdir: CanonicalDirBuf,

    // You must have ssh access to this server with this port, user name.
    pub server_user: String,
    pub server_port: u16,
    pub server_host: String,

    /// Canonicalized absolute path.
    pub source: CanonicalDirBuf,
    // Stored in Arcs, because it's nice that the Category object
    // also has a reference to them
    pub file_structures: HashMap<String, Arc<FileStructure>>,
    pub categories: Vec<Category>,
}

// Validated, but not filesystem-resolved config.
pub struct ConfigSpec {
    lock_file: PathBuf,
    log_dir: PathBuf,
    pub server_user: String,
    pub server_port: u16,
    pub server_host: String,
    pub source: PathBuf,
    pub file_structures: HashMap<String, Arc<FileStructure>>,
    pub categories: Vec<CategorySpec>,
}

// Validated, but not filesysem-resolved config
pub struct CategorySpec {
    pub regex: Regex,
    pub classification_glob: Option<Pattern>,
    // Absolute, only normal path segments
    pub landing_zone: PathBuf,
    // Absolute, only path segments
    pub staging_zone: PathBuf,
    // Arc, because filestructures are shared between categories
    pub file_structure: Arc<FileStructure>,
    pub year_subdirectory: bool,
}

// Validated and filesystem-resolved category
#[derive(Debug)]
pub struct Category {
    pub regex: Regex,
    pub classification_glob: Option<Pattern>,
    pub landing_zone: CanonicalDirBuf,

    /// Here, run directories are created incrementally before final atomic move to landing zone.
    /// Setup command validates this is on the same partition as the landing zone.
    pub staging_zone: CanonicalDirBuf,

    pub file_structure: Arc<FileStructure>,
    /// When true, place runs into a year-based subdirectory under the landing
    /// zone. The year is derived from the directory name by prepending "20" to
    /// its first two characters (e.g. "240101_NB123" → "2024/").
    pub year_subdirectory: bool,
}

#[derive(Debug)]
pub struct FileStructure {
    pub name: String,
    // We split ignore/checkout into paths and globs because matching a path
    // is much faster, as it's just a hash check.
    // Files matching these paths are not transferred
    pub ignore_paths: HashSet<PathBuf>,
    // Files matching these patterns are not transferred
    pub ignore_globs: Vec<Pattern>,
    // Files matching these paths are not archived in landing zone
    pub checkout_paths: HashSet<PathBuf>,
    // Files matching these patterns are not archived in landing zone
    pub checkout_globs: Vec<Pattern>,
    pub completion_file_globs: Vec<Pattern>,
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
    staging_zone: PathBuf,
    landing_zone: PathBuf,
    filestructure: String,
    #[serde(default)]
    year_subdirectory: bool,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Config path {description} must be absolute and should not contain . or ..")]
    NonAbsolutePath { description: String },
    #[error("failed to read config file {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "config field `{field}` must be a relative path/glob inside the run directory: {pattern:?}"
    )]
    GlobOutsideRunDirectory {
        field: &'static str,
        pattern: String,
    },
    #[error("failed to parse YAML config: {0}")]
    Parse(serde_yaml::Error),
    #[error("unsupported config version {found}; this binary supports config version {supported}")]
    UnsupportedVersion { found: u16, supported: u16 },
    #[error("config field `{field}` must not be empty")]
    EmptyField { field: &'static str },
    #[error("config field `{field}` must not be 0")]
    ZeroPort { field: &'static str },
    #[error(transparent)]
    PathError(#[from] Box<PathError>),
    #[error(
        "config fields `{first}` and `{second}` must not point to the same path: {}",
        path.display()
    )]
    DuplicatePath {
        first: String,
        second: String,
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
}

impl From<PathError> for ConfigError {
    fn from(error: PathError) -> Self {
        Self::PathError(Box::new(error))
    }
}

impl ConfigSpec {
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
        config.validate_spec()
    }

    fn into_resolved(self) -> Result<Config, ConfigError> {
        let lock_file =
            CanonicalChildFileBuf::from_absolute(&self.lock_file, "Lock file in config file")?;
        let logdir = CanonicalDirBuf::from_absolute(&self.log_dir, "Log dir in config file")?;
        let source = CanonicalDirBuf::from_absolute(&self.source, "Source in config file")?;

        let mut categories = Vec::with_capacity(self.categories.len());
        for cat in self.categories {
            categories.push(cat.into_resolved()?);
        }

        // Check for distinctness of some paths.
        let mut description_of: HashMap<PathBuf, String> = HashMap::new();
        description_of.insert(
            lock_file.parent().as_ref().to_owned(),
            "lock_file parent".to_owned(),
        );

        // We don't care if lock file parent and log dir clashes, they can be the same,
        // and it does not matter for the validation below.
        description_of.insert(logdir.as_ref().to_owned(), "logdir".to_owned());

        for (category_index, category) in categories.iter().enumerate() {
            // We also do not care if the staging zones are the same as each other,
            // or the log or lock file parent dir. They are all arbitrary writeable
            // directories.
            description_of.insert(
                category.staging_zone.as_ref().to_owned(),
                format!("Staging zone of category {}", category_index),
            );
        }

        // The landing zone cannot clash with either staging zone, log dir, or lock dir.
        let mut lz_paths_descriptions = Vec::new();
        for (category_index, category) in categories.iter().enumerate() {
            lz_paths_descriptions.push((
                category.landing_zone.as_ref().to_owned(),
                format!("Landing zone of category {}", category_index),
            ));
        }

        for (path, description) in lz_paths_descriptions.iter() {
            if let Some(existing) = description_of.get(path) {
                return Err(ConfigError::DuplicatePath {
                    first: existing.clone(),
                    second: description.clone(),
                    path: path.clone(),
                });
            }
        }

        // Now, insert the landing zones, and check if the source dir clashes with
        // anything. It cannot, because the source dir is read-only and we must not write to it.
        for (path, description) in lz_paths_descriptions {
            description_of.insert(path, description);
        }

        if let Some(existing) = description_of.get(source.as_ref()) {
            return Err(ConfigError::DuplicatePath {
                first: existing.clone(),
                second: "Source directory".to_owned(),
                path: source.as_ref().to_owned(),
            });
        }

        Ok(Config {
            lock_file,
            logdir,
            server_user: self.server_user,
            server_port: self.server_port,
            server_host: self.server_host,
            source,
            file_structures: self.file_structures,
            categories,
        })
    }
}

impl Config {
    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        let config = Self::resolve_from_yaml_str(&contents)?;
        Ok(config)
    }

    pub(crate) fn resolve_from_yaml_str(contents: &str) -> Result<Self, ConfigError> {
        let spec = ConfigSpec::from_yaml_str(contents)?;
        spec.into_resolved()
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

fn validate_absolute_normal(path: &Path, description: &str) -> Result<PathBuf, ConfigError> {
    if (!path.is_absolute())
        || path
            .components()
            .skip(1)
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        Err(ConfigError::NonAbsolutePath {
            description: description.to_owned(),
        })
    } else {
        Ok(path.to_owned())
    }
}

impl UnvalidatedConfig {
    fn validate_spec(self) -> Result<ConfigSpec, ConfigError> {
        validate_absolute_normal(&self.lock_file, "Lock file")?;
        validate_absolute_normal(&self.logdir, "Log dir")?;
        validate_absolute_normal(&self.source, "Source")?;

        validate_non_empty("server_user", &self.server_user)?;
        validate_non_empty("server_host", &self.server_host)?;

        if self.server_port == 0 {
            return Err(ConfigError::ZeroPort {
                field: "server_port",
            });
        }

        if self.category.is_empty() {
            return Err(ConfigError::NoCategoriesConfigured);
        }
        if self.filestructures.is_empty() {
            return Err(ConfigError::NoFileStructuresConfigured);
        }

        let mut file_structures = HashMap::with_capacity(self.filestructures.len());
        for (name, filestructure) in self.filestructures {
            validate_non_empty("filestructures key", &name)?;
            file_structures.insert(name.clone(), Arc::new(filestructure.validate(name)?));
        }

        let mut categories = Vec::with_capacity(self.category.len());
        for cat in self.category {
            categories.push(cat.validate_spec(&file_structures)?);
        }

        Ok(ConfigSpec {
            lock_file: self.lock_file,
            log_dir: self.logdir,
            server_user: self.server_user,
            server_port: self.server_port,
            server_host: self.server_host,
            source: self.source,
            file_structures,
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
    fn validate_spec(
        self,
        filestructures: &HashMap<String, Arc<FileStructure>>,
    ) -> Result<CategorySpec, ConfigError> {
        let landing_zone =
            validate_absolute_normal(&self.landing_zone, "Landing zone of category")?;
        let staging_zone =
            validate_absolute_normal(&self.staging_zone, "Staging zone of category")?;

        let regex = Regex::new(&self.regex).map_err(|source| ConfigError::InvalidRegex {
            field: "category.regex",
            source,
        })?;

        let classification_glob = self
            .classification_glob
            .as_deref()
            .map(|pattern| validate_glob("category.classification_glob", pattern))
            .transpose()?;

        // Now, map file structures to categories
        let file_structure = if let Some(file_structure) = filestructures.get(&self.filestructure) {
            file_structure.clone()
        } else {
            return Err(ConfigError::UnknownFileStructure {
                name: self.filestructure,
            });
        };

        Ok(CategorySpec {
            regex,
            classification_glob,
            landing_zone,
            staging_zone,
            file_structure,
            year_subdirectory: self.year_subdirectory,
        })
    }
}

impl CategorySpec {
    fn into_resolved(self) -> Result<Category, ConfigError> {
        // Check absolute paths
        let landing_zone =
            CanonicalDirBuf::from_absolute(&self.landing_zone, "Landing zone of a category")?;
        let staging_zone =
            CanonicalDirBuf::from_absolute(&self.staging_zone, "Staging zone of a category")?;

        Ok(Category {
            regex: self.regex,
            classification_glob: self.classification_glob,
            landing_zone,
            staging_zone,
            file_structure: self.file_structure,
            year_subdirectory: self.year_subdirectory,
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

fn validate_glob(field: &'static str, pattern: &str) -> Result<Pattern, ConfigError> {
    let path = Path::new(pattern);

    if pattern.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
                    | std::path::Component::CurDir
                    | std::path::Component::ParentDir
            )
        })
    {
        return Err(ConfigError::GlobOutsideRunDirectory {
            field,
            pattern: pattern.to_owned(),
        });
    }

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

#[cfg(any())]
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

#[cfg(test)]
mod current_tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{Config, ConfigError, ConfigSpec};
    use crate::paths::PathError;

    static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    fn base_config(categories: &str) -> String {
        format!(
            r#"
version: 3
lock_file: "/var/lib/sequencer/flock/sequencer-sync.lock"
logdir: "/var/lib/sequencer/log"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "/data/sequencer"
filestructures:
  default:
    ignore_globs:
      - "ignore/literal.txt"
      - "skip/*.tmp"
    checkout_globs:
      - "report.txt"
      - "results/*.csv"
    completion_file_globs:
      - "complete.txt"

category:
{categories}
"#
        )
    }

    fn one_category() -> &'static str {
        r#"  - regex: "^run"
    classification_glob: "metadata/*.txt"
    staging_zone: "/var/lib/sequencer/staging"
    landing_zone: "/var/lib/sequencer/landing"
    filestructure: "default"
"#
    }

    fn spec(contents: &str) -> Result<ConfigSpec, ConfigError> {
        ConfigSpec::from_yaml_str(contents)
    }

    fn expect_spec_err(contents: &str) -> ConfigError {
        match spec(contents) {
            Ok(_) => panic!("config spec should fail"),
            Err(error) => error,
        }
    }

    #[test]
    fn parses_valid_config_without_touching_filesystem() {
        let config = spec(&base_config(one_category())).expect("config spec should parse");

        assert_eq!(config.server_user, "sequencer-sync");
        assert_eq!(config.server_port, 22);
        assert_eq!(config.server_host, "sequencer.example.org");
        assert_eq!(config.source, PathBuf::from("/data/sequencer"));
        assert_eq!(config.file_structures.len(), 1);
        assert_eq!(config.categories.len(), 1);

        let category = &config.categories[0];
        assert!(category.regex.is_match("run-001"));
        assert!(!category.regex.is_match("other-001"));
        assert!(
            category
                .classification_glob
                .as_ref()
                .expect("classification glob should parse")
                .matches("metadata/core.txt")
        );
        assert_eq!(
            category.staging_zone,
            PathBuf::from("/var/lib/sequencer/staging")
        );
        assert_eq!(
            category.landing_zone,
            PathBuf::from("/var/lib/sequencer/landing")
        );

        let filestructure = config.file_structures.get("default").unwrap();
        assert!(
            filestructure
                .ignore_paths
                .contains(Path::new("ignore/literal.txt"))
        );
        assert_eq!(filestructure.ignore_globs.len(), 1);
        assert!(
            filestructure
                .checkout_paths
                .contains(Path::new("report.txt"))
        );
        assert_eq!(filestructure.checkout_globs.len(), 1);
        assert_eq!(filestructure.completion_file_globs.len(), 1);
    }

    #[test]
    fn rejects_unsupported_config_version_before_shape_errors() {
        let contents = base_config(one_category()).replacen(
            "version: 3",
            "version: 4\nfuture_required_field: true",
            1,
        );

        let error = expect_spec_err(&contents);

        assert!(matches!(
            error,
            ConfigError::UnsupportedVersion {
                found: 4,
                supported: 3
            }
        ));
    }

    #[test]
    fn rejects_missing_required_version_as_parse_error() {
        let contents = base_config(one_category()).replace("version: 3\n", "");

        let error = expect_spec_err(&contents);

        assert!(matches!(error, ConfigError::Parse(_)));
    }

    #[test]
    fn rejects_relative_or_non_normal_absolute_paths() {
        let relative_source = base_config(one_category())
            .replace(r#"source: "/data/sequencer""#, r#"source: "relative/data""#);
        assert!(matches!(
            expect_spec_err(&relative_source),
            ConfigError::NonAbsolutePath { .. }
        ));

        let parent_source = base_config(one_category()).replace(
            r#"source: "/data/sequencer""#,
            r#"source: "/data/../sequencer""#,
        );
        assert!(matches!(
            expect_spec_err(&parent_source),
            ConfigError::NonAbsolutePath { .. }
        ));
    }

    #[test]
    fn rejects_empty_fields_and_missing_references() {
        let empty_user = base_config(one_category())
            .replace(r#"server_user: "sequencer-sync""#, r#"server_user: "   ""#);
        assert!(matches!(
            expect_spec_err(&empty_user),
            ConfigError::EmptyField {
                field: "server_user"
            }
        ));

        let unknown_filestructure = base_config(
            &one_category().replace(r#"filestructure: "default""#, r#"filestructure: "missing""#),
        );
        assert!(matches!(
            expect_spec_err(&unknown_filestructure),
            ConfigError::UnknownFileStructure { name } if name == "missing"
        ));
    }

    #[test]
    fn rejects_bad_globs_and_globs_outside_run_directory() {
        let invalid_glob = base_config(one_category()).replace(r#"- "skip/*.tmp""#, r#"- "[""#);
        assert!(matches!(
            expect_spec_err(&invalid_glob),
            ConfigError::InvalidGlob {
                field: "filestructures.*.ignore_globs",
                ..
            }
        ));

        for pattern in [
            "/absolute.txt",
            "../outside.txt",
            "./same-dir.txt",
            "nested/../x.txt",
        ] {
            let contents = base_config(one_category())
                .replace(r#"- "complete.txt""#, &format!(r#"- "{pattern}""#));
            assert!(matches!(
                expect_spec_err(&contents),
                ConfigError::GlobOutsideRunDirectory {
                    field: "filestructures.*.completion_file_globs",
                    ..
                }
            ));
        }
    }

    #[test]
    fn rejects_empty_completion_glob_list() {
        let contents = base_config(one_category()).replace(
            "    completion_file_globs:\n      - \"complete.txt\"",
            "    completion_file_globs: []",
        );

        let error = expect_spec_err(&contents);

        assert!(matches!(
            error,
            ConfigError::EmptyGlobList {
                field: "filestructures.*.completion_file_globs"
            }
        ));
    }

    #[test]
    fn duplicate_landing_zones_are_allowed() {
        let categories = r#"  - regex: "^run-a"
    staging_zone: "/var/lib/sequencer/staging-a"
    landing_zone: "/var/lib/sequencer/landing"
    filestructure: "default"
  - regex: "^run-b"
    staging_zone: "/var/lib/sequencer/staging-b"
    landing_zone: "/var/lib/sequencer/landing"
    filestructure: "default"
"#;

        let config = spec(&base_config(categories)).expect("duplicate landing zones should parse");

        assert_eq!(config.categories.len(), 2);
        assert_eq!(
            config.categories[0].landing_zone,
            config.categories[1].landing_zone
        );
    }

    #[test]
    fn from_path_resolves_existing_directories() {
        let tempdir = make_temp_dir();
        let flockdir = tempdir.join("flock");
        let logdir = tempdir.join("log");
        let source = tempdir.join("source");
        let staging = tempdir.join("staging");
        let landing = tempdir.join("landing");
        for dir in [&flockdir, &logdir, &source, &staging, &landing] {
            fs::create_dir(dir).expect("should create fixture dir");
        }
        let config_path = tempdir.join("config.yaml");
        fs::write(
            &config_path,
            format!(
                r#"
version: 3
lock_file: "{}/sequencer-sync.lock"
logdir: "{}"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "{}"
filestructures:
  default:
    ignore_globs: []
    checkout_globs: []
    completion_file_globs:
      - "complete.txt"

category:
  - regex: "^run"
    staging_zone: "{}"
    landing_zone: "{}"
    filestructure: "default"
"#,
                flockdir.display(),
                logdir.display(),
                source.display(),
                staging.display(),
                landing.display(),
            ),
        )
        .expect("should write config");

        let config = Config::from_path(&config_path).expect("config should resolve");

        assert_eq!(config.logdir.as_ref(), logdir.canonicalize().unwrap());
        assert_eq!(config.source.as_ref(), source.canonicalize().unwrap());
        assert_eq!(
            config.categories[0].staging_zone.as_ref(),
            staging.canonicalize().unwrap()
        );
        assert_eq!(
            config.categories[0].landing_zone.as_ref(),
            landing.canonicalize().unwrap()
        );
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn from_path_reports_missing_landing_zone() {
        let tempdir = make_temp_dir();
        let flockdir = tempdir.join("flock");
        let logdir = tempdir.join("log");
        let source = tempdir.join("source");
        let staging = tempdir.join("staging");
        let landing = tempdir.join("missing-landing");
        for dir in [&flockdir, &logdir, &source, &staging] {
            fs::create_dir(dir).expect("should create fixture dir");
        }
        let config_path = tempdir.join("config.yaml");
        fs::write(
            &config_path,
            format!(
                r#"
version: 3
lock_file: "{}/sequencer-sync.lock"
logdir: "{}"
server_user: "sequencer-sync"
server_port: 22
server_host: "sequencer.example.org"
source: "{}"
filestructures:
  default:
    ignore_globs: []
    checkout_globs: []
    completion_file_globs:
      - "complete.txt"

category:
  - regex: "^run"
    staging_zone: "{}"
    landing_zone: "{}"
    filestructure: "default"
"#,
                flockdir.display(),
                logdir.display(),
                source.display(),
                staging.display(),
                landing.display(),
            ),
        )
        .expect("should write config");

        let error = Config::from_path(&config_path).expect_err("missing landing zone should fail");

        assert!(matches!(
            error,
            ConfigError::PathError(inner) if matches!(*inner, PathError::NotFound { .. })
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
