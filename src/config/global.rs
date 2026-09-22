//! Global (user-level) configuration reader.

use std::path::Path;
use std::path::PathBuf;

use jj_lib::config::ConfigLayer;
use jj_lib::config::ConfigLoadError;
use jj_lib::config::ConfigSource;

use super::ConfigReader;
use super::GLOBAL_CONFIG_DIR;
use super::GLOBAL_CONFIG_FILE;
use super::JjunctionConfig;
use super::read_layer;

/// Reads the user-level configuration file
/// (`<platform config dir>/jjunction/config.toml`).
///
/// A missing file is treated as an empty layer so that first runs work
/// without any setup.
#[derive(Debug, Clone)]
pub struct GlobalConfigReader {
    path: PathBuf,
}

impl GlobalConfigReader {
    /// Returns the default global config path under the platform config
    /// directory, or `None` if the platform has no such directory.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join(GLOBAL_CONFIG_DIR).join(GLOBAL_CONFIG_FILE))
    }

    /// Creates a reader loading from an explicit path.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the path this reader loads from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads the global configuration as a layer tagged
    /// [`ConfigSource::User`], or `None` if the file does not exist.
    pub fn layer(&self) -> Result<Option<ConfigLayer>, ConfigLoadError> {
        read_layer(ConfigSource::User, &self.path)
    }
}

impl ConfigReader for GlobalConfigReader {
    type Error = ConfigLoadError;

    fn read(&self) -> Result<JjunctionConfig, Self::Error> {
        self.layer()?;
        // TODO: deserialize typed fields from the layer once config keys are defined
        Ok(JjunctionConfig::default())
    }
}
