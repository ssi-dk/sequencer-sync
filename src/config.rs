use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, PartialEq, Eq)]
pub struct Config {
    // Directory to copy data from and to landing zone
    pub source: PathBuf,

    // Directory to copy from source
    pub landing_zone: PathBuf,

    // Directory with log files and file lock
    pub flockdir: PathBuf,

    // You must have ssh access to this server with this port, user name.
    pub server_user: String,
    pub server_port: u16,
    pub server_host: String,

    // Copy from landing zone to this path on the server
    pub server_dest: PathBuf,
}

#[derive(Debug, Deserialize)]
struct UnvalidatedConfig {
    source: PathBuf,
    landing_zone: PathBuf,
    flockdir: PathBuf,
    server_user: String,
    server_port: u16,
    server_host: String,
    server_dest: PathBuf,
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
        validate_absolute_path("source", &self.source)?;
        validate_absolute_path("landing_zone", &self.landing_zone)?;
        validate_absolute_path("flockdir", &self.flockdir)?;
        validate_absolute_path("server_dest", &self.server_dest)?;
        validate_non_empty("server_user", &self.server_user)?;
        validate_non_empty("server_host", &self.server_host)?;

        if self.server_port == 0 {
            return Err(ConfigError::ZeroPort {
                field: "server_port",
            });
        }

        validate_distinct_paths("source", &self.source, "landing_zone", &self.landing_zone)?;
        validate_distinct_paths("source", &self.source, "flockdir", &self.flockdir)?;
        validate_distinct_paths(
            "landing_zone",
            &self.landing_zone,
            "flockdir",
            &self.flockdir,
        )?;

        Ok(Config {
            source: self.source,
            landing_zone: self.landing_zone,
            flockdir: self.flockdir,
            server_user: self.server_user,
            server_port: self.server_port,
            server_host: self.server_host,
            server_dest: self.server_dest,
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

fn validate_distinct_paths(
    first: &'static str,
    first_path: &Path,
    second: &'static str,
    second_path: &Path,
) -> Result<(), ConfigError> {
    if first_path == second_path {
        Err(ConfigError::DuplicatePath {
            first,
            second,
            path: first_path.to_path_buf(),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Config, ConfigError};

    const EXAMPLE_CONFIG: &str = include_str!("../examples/config.example.toml");

    #[test]
    fn parses_example_config() {
        let config = Config::from_toml_str(EXAMPLE_CONFIG).expect("example config should parse");

        assert_eq!(config.source, PathBuf::from("/var/lib/sequencer/data"));
        assert_eq!(
            config.landing_zone,
            PathBuf::from("/var/lib/sequencer/landing-zone")
        );
        assert_eq!(config.flockdir, PathBuf::from("/var/lib/sequencer/flock"));
        assert_eq!(config.server_user, "sequencer-sync");
        assert_eq!(config.server_port, 22);
        assert_eq!(config.server_host, "sequencer.example.org");
        assert_eq!(config.server_dest, PathBuf::from("/srv/sequencer/incoming"));
    }

    #[test]
    fn rejects_relative_source_path() {
        let error = Config::from_toml_str(
            r#"
source = "relative/data"
landing_zone = "/var/lib/sequencer/landing-zone"
flockdir = "/var/lib/sequencer/flock"
server_user = "sequencer-sync"
server_port = 22
server_host = "sequencer.example.org"
server_dest = "/srv/sequencer/incoming"
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
        let error = Config::from_toml_str(
            r#"
source = "/var/lib/sequencer/data"
landing_zone = "/var/lib/sequencer/landing-zone"
flockdir = "/var/lib/sequencer/flock"
server_user = "   "
server_port = 22
server_host = "sequencer.example.org"
server_dest = "/srv/sequencer/incoming"
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
source = "/var/lib/sequencer/data"
landing_zone = "/var/lib/sequencer/data"
flockdir = "/var/lib/sequencer/flock"
server_user = "sequencer-sync"
server_port = 22
server_host = "sequencer.example.org"
server_dest = "/srv/sequencer/incoming"
"#,
        )
        .expect_err("duplicate local paths should fail validation");

        assert!(matches!(
            error,
            ConfigError::DuplicatePath {
                first: "source",
                second: "landing_zone",
                ..
            }
        ));
    }
}
