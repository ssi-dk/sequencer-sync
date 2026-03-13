use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub source: PathBuf,
    pub landing_zone: PathBuf,
    pub server_user: String,
    pub server_port: u16,
    pub server_host: String,
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(toml::de::Error),
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
        toml::from_str(contents).map_err(ConfigError::Parse)
    }
}

impl Display for ConfigError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "failed to read config file {}: {source}", path.display())
            }
            Self::Parse(source) => write!(f, "failed to parse TOML config: {source}"),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::Config;

    const EXAMPLE_CONFIG: &str = include_str!("../examples/config.example.toml");

    #[test]
    fn parses_example_config() {
        let config = Config::from_toml_str(EXAMPLE_CONFIG).expect("example config should parse");

        assert_eq!(config.source, PathBuf::from("/var/lib/sequencer/data"));
        assert_eq!(
            config.landing_zone,
            PathBuf::from("/var/lib/sequencer/landing-zone")
        );
        assert_eq!(config.server_user, "sequencer-sync");
        assert_eq!(config.server_port, 22);
        assert_eq!(config.server_host, "sequencer.example.org");
    }
}
