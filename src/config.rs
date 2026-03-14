use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug)]
pub struct Config {
    // Directory with log files and file lock
    pub flockdir: PathBuf,

    // You must have ssh access to this server with this port, user name.
    pub server_user: String,
    pub server_port: u16,
    pub server_host: String,

    pub platform: PlatformConfig,
}

#[derive(Debug)]
pub enum PlatformConfig {
    Nanopore(NanoporeConfig),
    NextSeq(NextSeqConfig),
}

// For Nanopore, for some sequencing runs, we want to copy all files,
// and for some, we want to skip the raw data files.
// We distinguish these by directory prefix, and copy different file
// subsets to different landing zones.
#[derive(Debug, PartialEq, Eq)]
pub struct NanoporeConfig {
    pub source: PathBuf,
    /// Sorted longest-prefix-first for correct matching.
    pub categories: [NanoporeCategory; 2],
}

#[derive(Debug, PartialEq, Eq)]
pub struct NanoporeCategory {
    pub prefix: String,
    pub landing_zone: PathBuf,
    pub exclude: Vec<String>,
    pub completion_file_glob: String,
}

#[derive(Debug)]
pub struct NextSeqConfig {
    pub source: PathBuf,
    pub regex: Regex,
    pub landing_zone: PathBuf,
    pub exclude: Vec<String>,
    pub completion_file_glob: String,
    /// When true, place runs into a year-based subdirectory under the landing
    /// zone. The year is derived from the directory name by prepending "20" to
    /// its first two characters (e.g. "240101_NB123" → "2024/").
    /// This matches the legacy bash script behavior.
    pub year_subdirectory: bool,
}

impl NextSeqConfig {
    /// Returns the destination directory for a given run. When
    /// `year_subdirectory` is enabled, appends a year derived from the first
    /// two characters of the directory name (e.g. "24" → "2024").
    /// Returns `None` if `year_subdirectory` is enabled but the name is too
    /// short to extract a year prefix.
    pub fn destination_for(&self, dir_name: &str) -> Option<PathBuf> {
        if self.year_subdirectory {
            if dir_name.len() < 2 {
                return None;
            }
            let year = format!("20{}", &dir_name[..2]);
            Some(self.landing_zone.join(year))
        } else {
            Some(self.landing_zone.clone())
        }
    }
}

impl NanoporeConfig {
    /// Returns the category matching this directory name, or None.
    /// Categories are sorted longest-prefix-first, so ONT_WGS_ matches before ONT_.
    pub fn classify(&self, dir_name: &str) -> Option<&NanoporeCategory> {
        self.categories
            .iter()
            .find(|c| dir_name.starts_with(&c.prefix))
    }
}

#[derive(Debug, Deserialize)]
struct UnvalidatedConfig {
    flockdir: PathBuf,
    server_user: String,
    server_port: u16,
    server_host: String,
    nanopore: Option<UnvalidatedNanoporeConfig>,
    nextseq: Option<UnvalidatedNextSeqConfig>,
}

#[derive(Debug, Deserialize)]
struct UnvalidatedNanoporeConfig {
    source: PathBuf,
    basecalled: UnvalidatedNanoporeCategory,
    alldata: UnvalidatedNanoporeCategory,
}

#[derive(Debug, Deserialize)]
struct UnvalidatedNanoporeCategory {
    prefix: String,
    landing_zone: PathBuf,
    #[serde(default)]
    exclude: Vec<String>,
    completion_file_glob: String,
}

#[derive(Debug, Deserialize)]
struct UnvalidatedNextSeqConfig {
    source: PathBuf,
    regex: String,
    landing_zone: PathBuf,
    #[serde(default)]
    exclude: Vec<String>,
    completion_file_glob: String,
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
    #[error("failed to parse TOML config: {0}")]
    Parse(toml::de::Error),
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
    #[error("config must contain exactly one of [nanopore] or [nextseq] sections, found neither")]
    NoPlatformSection,
    #[error("config must contain exactly one of [nanopore] or [nextseq] sections, found both")]
    MultiplePlatformSections,
    #[error("nanopore basecalled and alldata prefixes must differ: `{prefix}`")]
    DuplicatePrefix { prefix: String },
    #[error("config field `{field}` is not a valid regex: {source}")]
    InvalidRegex {
        field: &'static str,
        #[source]
        source: regex::Error,
    },
}

impl Config {
    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        Self::from_toml_str(&contents)
    }

    pub fn from_toml_str(contents: &str) -> Result<Self, ConfigError> {
        let config: UnvalidatedConfig = toml::from_str(contents).map_err(ConfigError::Parse)?;
        config.validate()
    }
}

impl UnvalidatedConfig {
    fn validate(self) -> Result<Config, ConfigError> {
        validate_absolute_path("flockdir", &self.flockdir)?;
        validate_non_empty("server_user", &self.server_user)?;
        validate_non_empty("server_host", &self.server_host)?;

        if self.server_port == 0 {
            return Err(ConfigError::ZeroPort {
                field: "server_port",
            });
        }

        let platform = match (self.nanopore, self.nextseq) {
            (Some(nanopore), None) => PlatformConfig::Nanopore(nanopore.validate(&self.flockdir)?),
            (None, Some(nextseq)) => PlatformConfig::NextSeq(nextseq.validate(&self.flockdir)?),
            (Some(_), Some(_)) => return Err(ConfigError::MultiplePlatformSections),
            (None, None) => return Err(ConfigError::NoPlatformSection),
        };

        Ok(Config {
            flockdir: self.flockdir,
            server_user: self.server_user,
            server_port: self.server_port,
            server_host: self.server_host,
            platform,
        })
    }
}

impl UnvalidatedNanoporeConfig {
    fn validate(self, flockdir: &Path) -> Result<NanoporeConfig, ConfigError> {
        validate_absolute_path("nanopore.source", &self.source)?;
        validate_absolute_path(
            "nanopore.basecalled.landing_zone",
            &self.basecalled.landing_zone,
        )?;
        validate_absolute_path("nanopore.alldata.landing_zone", &self.alldata.landing_zone)?;

        validate_non_empty("nanopore.basecalled.prefix", &self.basecalled.prefix)?;
        validate_non_empty("nanopore.alldata.prefix", &self.alldata.prefix)?;
        validate_non_empty(
            "nanopore.basecalled.completion_file_glob",
            &self.basecalled.completion_file_glob,
        )?;
        validate_non_empty(
            "nanopore.alldata.completion_file_glob",
            &self.alldata.completion_file_glob,
        )?;

        if self.basecalled.prefix == self.alldata.prefix {
            return Err(ConfigError::DuplicatePrefix {
                prefix: self.basecalled.prefix,
            });
        }

        // Check all paths are distinct
        let paths: [(&'static str, &Path); 4] = [
            ("nanopore.source", &self.source),
            (
                "nanopore.basecalled.landing_zone",
                &self.basecalled.landing_zone,
            ),
            ("nanopore.alldata.landing_zone", &self.alldata.landing_zone),
            ("flockdir", flockdir),
        ];
        validate_all_paths_distinct(&paths)?;

        // Sort longest prefix first
        let mut categories = [
            NanoporeCategory {
                prefix: self.basecalled.prefix,
                landing_zone: self.basecalled.landing_zone,
                exclude: self.basecalled.exclude,
                completion_file_glob: self.basecalled.completion_file_glob,
            },
            NanoporeCategory {
                prefix: self.alldata.prefix,
                landing_zone: self.alldata.landing_zone,
                exclude: self.alldata.exclude,
                completion_file_glob: self.alldata.completion_file_glob,
            },
        ];
        categories.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));

        Ok(NanoporeConfig {
            source: self.source,
            categories,
        })
    }
}

impl UnvalidatedNextSeqConfig {
    fn validate(self, flockdir: &Path) -> Result<NextSeqConfig, ConfigError> {
        validate_absolute_path("nextseq.source", &self.source)?;
        validate_absolute_path("nextseq.landing_zone", &self.landing_zone)?;
        validate_non_empty("nextseq.regex", &self.regex)?;
        validate_non_empty("nextseq.completion_file_glob", &self.completion_file_glob)?;

        let regex = Regex::new(&self.regex).map_err(|source| ConfigError::InvalidRegex {
            field: "nextseq.regex",
            source,
        })?;

        let paths: [(&'static str, &Path); 3] = [
            ("nextseq.source", &self.source),
            ("nextseq.landing_zone", &self.landing_zone),
            ("flockdir", flockdir),
        ];
        validate_all_paths_distinct(&paths)?;

        Ok(NextSeqConfig {
            source: self.source,
            regex,
            landing_zone: self.landing_zone,
            exclude: self.exclude,
            completion_file_glob: self.completion_file_glob,
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

    use super::{Config, ConfigError, NanoporeConfig, PlatformConfig};

    const NANOPORE_EXAMPLE: &str = include_str!("../examples/nanopore.example.toml");
    const NEXTSEQ_EXAMPLE: &str = include_str!("../examples/nextseq.example.toml");

    #[test]
    fn parses_nanopore_example_config() {
        let config = Config::from_toml_str(NANOPORE_EXAMPLE).expect("nanopore config should parse");

        assert_eq!(config.flockdir, PathBuf::from("/var/lib/sequencer/flock"));
        assert_eq!(config.server_user, "sequencer-sync");
        assert_eq!(config.server_port, 22);
        assert_eq!(config.server_host, "sequencer.example.org");

        let PlatformConfig::Nanopore(ref nano) = config.platform else {
            panic!("expected Nanopore platform");
        };
        assert_eq!(nano.source, PathBuf::from("/data/nanopore"));
        // Longest prefix first
        assert_eq!(nano.categories[0].prefix, "ONT_WGS_");
        assert_eq!(
            nano.categories[0].landing_zone,
            PathBuf::from("/var/lib/sequencer/landing-zone-core")
        );
        assert_eq!(nano.categories[1].prefix, "ONT_");
        assert_eq!(
            nano.categories[1].landing_zone,
            PathBuf::from("/var/lib/sequencer/landing-zone-other")
        );
    }

    #[test]
    fn parses_nextseq_example_config() {
        let config = Config::from_toml_str(NEXTSEQ_EXAMPLE).expect("nextseq config should parse");

        assert_eq!(config.flockdir, PathBuf::from("/var/lib/sequencer/flock"));
        let PlatformConfig::NextSeq(ref ns) = config.platform else {
            panic!("expected NextSeq platform");
        };
        assert_eq!(ns.source, PathBuf::from("/data/nextseq"));
        assert!(ns.regex.is_match("240101_"));
        assert_eq!(
            ns.landing_zone,
            PathBuf::from("/var/lib/sequencer/landing-zone")
        );
    }

    #[test]
    fn rejects_config_with_no_platform() {
        let error = Config::from_toml_str(
            r#"
flockdir = "/var/lib/sequencer/flock"
server_user = "sequencer-sync"
server_port = 22
server_host = "sequencer.example.org"
"#,
        )
        .expect_err("config with no platform should fail");

        assert!(matches!(error, ConfigError::NoPlatformSection));
    }

    #[test]
    fn rejects_config_with_both_platforms() {
        let error = Config::from_toml_str(
            r#"
flockdir = "/var/lib/sequencer/flock"
server_user = "sequencer-sync"
server_port = 22
server_host = "sequencer.example.org"

[nanopore]
source = "/data/nanopore"

[nanopore.basecalled]
prefix = "ONT_WGS_"
landing_zone = "/var/lib/sequencer/landing-zone-core"
completion_file_glob = "report*.html"

[nanopore.alldata]
prefix = "ONT_"
landing_zone = "/var/lib/sequencer/landing-zone-other"
completion_file_glob = "report*.html"

[nextseq]
source = "/data/nextseq"
regex = "^\\d{6}_"
landing_zone = "/var/lib/sequencer/landing-zone"
completion_file_glob = "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        )
        .expect_err("config with both platforms should fail");

        assert!(matches!(error, ConfigError::MultiplePlatformSections));
    }

    #[test]
    fn rejects_duplicate_nanopore_prefixes() {
        let error = Config::from_toml_str(
            r#"
flockdir = "/var/lib/sequencer/flock"
server_user = "sequencer-sync"
server_port = 22
server_host = "sequencer.example.org"

[nanopore]
source = "/data/nanopore"

[nanopore.basecalled]
prefix = "ONT_"
landing_zone = "/var/lib/sequencer/landing-zone-core"
completion_file_glob = "report*.html"

[nanopore.alldata]
prefix = "ONT_"
landing_zone = "/var/lib/sequencer/landing-zone-other"
completion_file_glob = "report*.html"
"#,
        )
        .expect_err("duplicate prefixes should fail");

        assert!(matches!(error, ConfigError::DuplicatePrefix { .. }));
    }

    #[test]
    fn rejects_duplicate_nanopore_landing_zones() {
        let error = Config::from_toml_str(
            r#"
flockdir = "/var/lib/sequencer/flock"
server_user = "sequencer-sync"
server_port = 22
server_host = "sequencer.example.org"

[nanopore]
source = "/data/nanopore"

[nanopore.basecalled]
prefix = "ONT_WGS_"
landing_zone = "/var/lib/sequencer/landing-zone"
completion_file_glob = "report*.html"

[nanopore.alldata]
prefix = "ONT_"
landing_zone = "/var/lib/sequencer/landing-zone"
completion_file_glob = "report*.html"
"#,
        )
        .expect_err("duplicate landing zones should fail");

        assert!(matches!(
            error,
            ConfigError::DuplicatePath {
                first: "nanopore.basecalled.landing_zone",
                second: "nanopore.alldata.landing_zone",
                ..
            }
        ));
    }

    #[test]
    fn rejects_relative_source_path() {
        let error = Config::from_toml_str(
            r#"
flockdir = "/var/lib/sequencer/flock"
server_user = "sequencer-sync"
server_port = 22
server_host = "sequencer.example.org"

[nextseq]
source = "relative/data"
regex = "^\\d{6}_"
landing_zone = "/var/lib/sequencer/landing-zone"
completion_file_glob = "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        )
        .expect_err("relative source path should fail validation");

        assert!(matches!(
            error,
            ConfigError::PathNotAbsolute {
                field: "nextseq.source",
                ..
            }
        ));
    }

    #[test]
    fn rejects_empty_server_user() {
        let error = Config::from_toml_str(
            r#"
flockdir = "/var/lib/sequencer/flock"
server_user = "   "
server_port = 22
server_host = "sequencer.example.org"

[nextseq]
source = "/data/nextseq"
regex = "^\\d{6}_"
landing_zone = "/var/lib/sequencer/landing-zone"
completion_file_glob = "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
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
    fn rejects_duplicate_local_paths() {
        let error = Config::from_toml_str(
            r#"
flockdir = "/var/lib/sequencer/flock"
server_user = "sequencer-sync"
server_port = 22
server_host = "sequencer.example.org"

[nextseq]
source = "/data/nextseq"
regex = "^\\d{6}_"
landing_zone = "/var/lib/sequencer/flock"
completion_file_glob = "PrimaryAnalysisMetrics/PrimaryAnalysisMetrics.csv"
"#,
        )
        .expect_err("duplicate local paths should fail validation");

        assert!(matches!(
            error,
            ConfigError::DuplicatePath {
                first: "nextseq.landing_zone",
                second: "flockdir",
                ..
            }
        ));
    }

    #[test]
    fn classify_matches_longest_prefix_first() {
        let config = NanoporeConfig {
            source: PathBuf::from("/data/nanopore"),
            categories: [
                super::NanoporeCategory {
                    prefix: "ONT_WGS_".to_string(),
                    landing_zone: PathBuf::from("/landing/core"),
                    exclude: vec![],
                    completion_file_glob: "report*.html".to_string(),
                },
                super::NanoporeCategory {
                    prefix: "ONT_".to_string(),
                    landing_zone: PathBuf::from("/landing/other"),
                    exclude: vec![],
                    completion_file_glob: "report*.html".to_string(),
                },
            ],
        };

        let cat = config
            .classify("ONT_WGS_run1")
            .expect("should match basecalled");
        assert_eq!(cat.prefix, "ONT_WGS_");

        let cat = config
            .classify("ONT_raw_run2")
            .expect("should match alldata");
        assert_eq!(cat.prefix, "ONT_");
    }

    #[test]
    fn classify_returns_none_for_unmatched() {
        let config = NanoporeConfig {
            source: PathBuf::from("/data/nanopore"),
            categories: [
                super::NanoporeCategory {
                    prefix: "ONT_WGS_".to_string(),
                    landing_zone: PathBuf::from("/landing/core"),
                    exclude: vec![],
                    completion_file_glob: "report*.html".to_string(),
                },
                super::NanoporeCategory {
                    prefix: "ONT_".to_string(),
                    landing_zone: PathBuf::from("/landing/other"),
                    exclude: vec![],
                    completion_file_glob: "report*.html".to_string(),
                },
            ],
        };

        assert!(config.classify("ILLUMINA_run1").is_none());
    }

    #[test]
    fn rejects_empty_nanopore_prefix() {
        let error = Config::from_toml_str(
            r#"
flockdir = "/var/lib/sequencer/flock"
server_user = "sequencer-sync"
server_port = 22
server_host = "sequencer.example.org"

[nanopore]
source = "/data/nanopore"

[nanopore.basecalled]
prefix = ""
landing_zone = "/var/lib/sequencer/landing-zone-core"
completion_file_glob = "report*.html"

[nanopore.alldata]
prefix = "ONT_"
landing_zone = "/var/lib/sequencer/landing-zone-other"
completion_file_glob = "report*.html"
"#,
        )
        .expect_err("empty prefix should fail");

        assert!(matches!(error, ConfigError::EmptyField { .. }));
    }
}
