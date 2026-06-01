use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use glob::Pattern;
use regex::Regex;
use serde::Deserialize;

use crate::paths::{CanonicalChildFileBuf, CanonicalDirBuf};
use crate::{AppError, UserError};

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

impl ConfigSpec {
    pub(crate) fn from_yaml_str(contents: &str) -> Result<Self, AppError> {
        let config: UnvalidatedConfig = match serde_yaml::from_str(contents) {
            Ok(config) => config,
            Err(parse_error) => {
                if let Ok(header) = serde_yaml::from_str::<ConfigHeader>(contents) {
                    validate_config_version(header.version)?;
                }
                return Err(UserError::InvalidConfigFormat { error: parse_error }.into());
            }
        };

        validate_config_version(config.version)?;
        config.validate_spec()
    }

    fn into_resolved(self) -> Result<Config, AppError> {
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
                return Err(UserError::DuplicateConfigPath {
                    first: existing.clone(),
                    second: description.clone(),
                    path: path.clone(),
                }
                .into());
            }
        }

        // Now, insert the landing zones, and check if the source dir clashes with
        // anything. It cannot, because the source dir is read-only and we must not write to it.
        for (path, description) in lz_paths_descriptions {
            description_of.insert(path, description);
        }

        if let Some(existing) = description_of.get(source.as_ref()) {
            return Err(UserError::DuplicateConfigPath {
                first: existing.clone(),
                second: "Source directory".to_owned(),
                path: source.as_ref().to_owned(),
            }
            .into());
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
    pub fn from_path(path: &Path) -> Result<Self, AppError> {
        let contents = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(UserError::NotFound {
                    description: "Config file".to_owned(),
                    path: path.to_owned(),
                }
                .into());
            }

            Err(source) => {
                return Err(AppError::Internal(
                    anyhow::Error::from(source).context("When reading config file"),
                ));
            }
        };
        let config = Self::resolve_from_yaml_str(&contents)?;
        Ok(config)
    }

    pub(crate) fn resolve_from_yaml_str(contents: &str) -> Result<Self, AppError> {
        let spec = ConfigSpec::from_yaml_str(contents)?;
        spec.into_resolved()
    }
}

fn validate_config_version(version: u16) -> Result<(), UserError> {
    if version == SUPPORTED_CONFIG_VERSION {
        Ok(())
    } else {
        Err(UserError::UnsupportedConfigVersion {
            found: version,
            supported: SUPPORTED_CONFIG_VERSION,
        })
    }
}

/// Config paths must begin with `/` or `~`, and every later component must be normal.
fn validate_non_relative_normal(path: &Path, description: &str) -> Result<PathBuf, AppError> {
    let err = || -> Result<PathBuf, AppError> {
        Err(UserError::UnacceptableConfigPath {
            description: description.to_owned(),
            path: path.to_owned(),
        }
        .into())
    };

    let mut components = path.components();

    let mut result: Option<PathBuf> = match components.next() {
        Some(Component::RootDir) => None,
        Some(Component::Normal(s)) if s == "~" => match std::env::var_os("HOME") {
            Some(s) => Some(PathBuf::from(s)),
            None => {
                return Err(UserError::HomeNotSetForTildePath {
                    description: description.to_owned(),
                    path: path.to_owned(),
                }
                .into());
            }
        },
        Some(Component::Normal(_)) => return err(),
        _ => return err(),
    };

    for component in components {
        match component {
            Component::Normal(s) => {
                if s == "~" {
                    return err();
                }
                if let Some(p) = &mut result {
                    p.push(s);
                }
            }
            _ => return err(),
        };
    }

    match result {
        Some(p) => Ok(p),
        None => Ok(path.to_owned()),
    }
}

impl UnvalidatedConfig {
    fn validate_spec(self) -> Result<ConfigSpec, AppError> {
        let lock_file = validate_non_relative_normal(&self.lock_file, "Lock file")?;
        let log_dir = validate_non_relative_normal(&self.logdir, "Log dir")?;
        let source = validate_non_relative_normal(&self.source, "Source")?;

        validate_string_not_empty(&self.server_user, "In config file, field `server_user`")?;
        validate_string_not_empty(&self.server_host, "In config file, field `server_host`")?;

        if self.server_port == 0 {
            return Err(UserError::ZeroPort.into());
        }

        if self.category.is_empty() {
            return Err(UserError::NoCategories.into());
        }
        if self.filestructures.is_empty() {
            return Err(UserError::NoFileStructures.into());
        }

        let mut file_structures = HashMap::with_capacity(self.filestructures.len());
        for (name, filestructure) in self.filestructures {
            validate_string_not_empty(&name, "In config file, name of file structure")?;
            file_structures.insert(name.clone(), Arc::new(filestructure.validate(name)?));
        }

        let mut categories = Vec::with_capacity(self.category.len());
        for cat in self.category {
            categories.push(cat.validate_spec(&file_structures)?);
        }

        Ok(ConfigSpec {
            lock_file,
            log_dir,
            server_user: self.server_user,
            server_port: self.server_port,
            server_host: self.server_host,
            source,
            file_structures,
            categories,
        })
    }
}

fn validate_string_not_empty(x: &str, description: &str) -> Result<(), AppError> {
    if x.trim().is_empty() {
        Err(UserError::EmptyString {
            description: description.to_owned(),
        }
        .into())
    } else {
        Ok(())
    }
}

impl UnvalidatedFileStructure {
    fn validate(self, name: String) -> Result<FileStructure, AppError> {
        let (ignore_paths, ignore_globs) =
            validate_file_patterns("filestructures.*.ignore_globs", &self.ignore_globs)?;
        let (checkout_paths, checkout_globs) =
            validate_file_patterns("filestructures.*.checkout_globs", &self.checkout_globs)?;

        if self.completion_file_globs.is_empty() {
            return Err(UserError::EmptyCompletionFileGlobList { name }.into());
        }

        let description = format!("completion glob of file structure {name}");
        let patterns: Result<_, AppError> = self
            .completion_file_globs
            .iter()
            .map(|pattern| validate_glob(&description, pattern))
            .collect();

        Ok(FileStructure {
            name,
            ignore_paths,
            ignore_globs,
            checkout_paths,
            checkout_globs,
            completion_file_globs: patterns?,
        })
    }
}

impl UnvalidatedCategory {
    fn validate_spec(
        self,
        filestructures: &HashMap<String, Arc<FileStructure>>,
    ) -> Result<CategorySpec, AppError> {
        let landing_zone =
            validate_non_relative_normal(&self.landing_zone, "Landing zone of category")?;
        let staging_zone =
            validate_non_relative_normal(&self.staging_zone, "Staging zone of category")?;

        let regex = Regex::new(&self.regex).map_err(|source| UserError::InvalidConfigRegex {
            description: "File structure regex".into(),
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
            return Err(UserError::UnknownConfigFileStructure {
                name: self.filestructure,
            }
            .into());
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
    fn into_resolved(self) -> Result<Category, AppError> {
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

fn validate_glob(description: &str, pattern: &str) -> Result<Pattern, AppError> {
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
        return Err(UserError::ConfigGlobOutsideRunDirectory {
            description: description.to_owned(),
            pattern: pattern.to_owned(),
        }
        .into());
    }

    Pattern::new(pattern).map_err(|source| {
        UserError::InvalidConfigGlob {
            description: description.to_owned(),
            glob_string: pattern.to_owned(),
            source,
        }
        .into()
    })
}

fn validate_file_patterns(
    description: &str,
    patterns: &[String],
) -> Result<(HashSet<PathBuf>, Vec<Pattern>), AppError> {
    let mut literal_paths = HashSet::new();
    let mut glob_patterns = Vec::new();

    for pattern in patterns {
        let glob = validate_glob(description, pattern)?;
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

#[cfg(test)]
mod current_tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{Config, ConfigSpec};
    use crate::{AppError, UserError};

    const EXAMPLE_CONFIG: &str = include_str!("../examples/config.yaml");
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

    fn spec(contents: &str) -> Result<ConfigSpec, AppError> {
        ConfigSpec::from_yaml_str(contents)
    }

    fn expect_spec_err(contents: &str) -> AppError {
        match spec(contents) {
            Ok(_) => panic!("config spec should fail"),
            Err(error) => error,
        }
    }

    #[test]
    fn example_config_matches_current_schema() {
        spec(EXAMPLE_CONFIG).expect("example config should match current schema");
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
            AppError::User(UserError::UnsupportedConfigVersion {
                found: 4,
                supported: 3
            })
        ));
    }

    #[test]
    fn rejects_missing_required_version_as_parse_error() {
        let contents = base_config(one_category()).replace("version: 3\n", "");

        let error = expect_spec_err(&contents);

        assert!(matches!(
            error,
            AppError::User(UserError::InvalidConfigFormat { .. })
        ));
    }

    #[test]
    fn rejects_config_with_no_categories() {
        let contents = base_config("").replace("category:\n", "");

        let error = expect_spec_err(&contents);

        assert!(matches!(error, AppError::User(UserError::NoCategories)));
    }

    #[test]
    fn rejects_relative_or_non_normal_absolute_paths() {
        let relative_source = base_config(one_category())
            .replace(r#"source: "/data/sequencer""#, r#"source: "relative/data""#);
        assert!(matches!(
            expect_spec_err(&relative_source),
            AppError::User(UserError::UnacceptableConfigPath { description, path })
                if description == "Source" && path == PathBuf::from("relative/data")
        ));

        let parent_source = base_config(one_category()).replace(
            r#"source: "/data/sequencer""#,
            r#"source: "/data/../sequencer""#,
        );
        assert!(matches!(
            expect_spec_err(&parent_source),
            AppError::User(UserError::UnacceptableConfigPath { description, path })
                if description == "Source" && path == PathBuf::from("/data/../sequencer")
        ));

        let non_starting_tilde = base_config(one_category()).replace(
            r#"source: "/data/sequencer""#,
            r#"source: "/data/~/sequencer""#,
        );
        assert!(matches!(
            expect_spec_err(&non_starting_tilde),
            AppError::User(UserError::UnacceptableConfigPath { description, path })
                if description == "Source" && path == PathBuf::from("/data/~/sequencer")
        ));
    }

    #[test]
    fn expands_leading_tilde_in_config_paths() {
        let home = std::env::var_os("HOME").expect("HOME should exist in test environment");
        let contents = base_config(one_category())
            .replace(r#"source: "/data/sequencer""#, r#"source: "~/sequencer""#);

        let config = spec(&contents).expect("leading tilde should be accepted");

        assert_eq!(config.source, PathBuf::from(home).join("sequencer"));
    }

    #[test]
    fn rejects_empty_fields_and_missing_references() {
        let empty_user = base_config(one_category())
            .replace(r#"server_user: "sequencer-sync""#, r#"server_user: "   ""#);
        assert!(matches!(
            expect_spec_err(&empty_user),
            AppError::User(UserError::EmptyString { description })
                if description.contains("server_user")
        ));

        let unknown_filestructure = base_config(
            &one_category().replace(r#"filestructure: "default""#, r#"filestructure: "missing""#),
        );
        assert!(matches!(
            expect_spec_err(&unknown_filestructure),
            AppError::User(UserError::UnknownConfigFileStructure { name }) if name == "missing"
        ));
    }

    #[test]
    fn rejects_bad_globs_and_globs_outside_run_directory() {
        let invalid_glob = base_config(one_category()).replace(r#"- "skip/*.tmp""#, r#"- "[""#);
        assert!(matches!(
            expect_spec_err(&invalid_glob),
            AppError::User(UserError::InvalidConfigGlob { description, .. })
                if description == "filestructures.*.ignore_globs"
        ));

        let invalid_classification_glob = base_config(one_category()).replace(
            r#"classification_glob: "metadata/*.txt""#,
            r#"classification_glob: "[""#,
        );
        assert!(matches!(
            expect_spec_err(&invalid_classification_glob),
            AppError::User(UserError::InvalidConfigGlob { description, .. })
                if description == "category.classification_glob"
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
                AppError::User(UserError::ConfigGlobOutsideRunDirectory { description, .. })
                    if description == "completion glob of file structure default"
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
            AppError::User(UserError::EmptyCompletionFileGlobList { name }) if name == "default"
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
            AppError::User(UserError::NotFound { description, path })
                if description == "Landing zone of a category" && path == landing
        ));
        cleanup_temp_dir(&tempdir);
    }

    #[test]
    fn from_path_reports_file_where_directory_expected() {
        let tempdir = make_temp_dir();
        let flockdir = tempdir.join("flock");
        let logdir = tempdir.join("log");
        let source = tempdir.join("source");
        let staging = tempdir.join("staging");
        let landing = tempdir.join("landing-file");
        for dir in [&flockdir, &logdir, &source, &staging] {
            fs::create_dir(dir).expect("should create fixture dir");
        }
        fs::write(&landing, "").expect("should create landing file");
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

        let error = Config::from_path(&config_path).expect_err("landing file should fail");
        let expected_landing = landing
            .canonicalize()
            .expect("landing fixture should canonicalize");

        assert!(matches!(
            error,
            AppError::User(UserError::NotADirectory { description, path })
                if description == "Landing zone of a category" && path == expected_landing
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
