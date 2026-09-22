//! Project-local (in-repo) configuration reader.
//!
//! Reads `<workspace root>/.jjunction/config.toml`. The file is treated as
//! **untrusted**: per `docs/design/config.md` it is layered *below* the user
//! global configuration, and only a safe declarative subset of keys may be
//! honored from it. Key filtering lands together with the first real config
//! keys (planned `trust.rs`).

use std::path::PathBuf;

use jj_lib::config::ConfigLayer;
use jj_lib::config::ConfigLoadError;
use jj_lib::config::ConfigSource;

use super::ConfigReader;
use super::JjunctionConfig;
use super::LOCAL_CONFIG_DIR;
use super::LOCAL_CONFIG_FILE;
use super::read_layer;

/// Reads the project-local configuration file below a workspace root.
#[derive(Debug, Clone)]
pub struct LocalConfigReader {
    root: PathBuf,
}

impl LocalConfigReader {
    /// Creates a reader rooted at `root` (a workspace root).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the path of the local configuration file.
    pub fn path(&self) -> PathBuf {
        self.root.join(LOCAL_CONFIG_DIR).join(LOCAL_CONFIG_FILE)
    }

    /// Loads the local configuration as a layer tagged
    /// [`ConfigSource::User`] (inserted below the global layer; see
    /// [`super::load_stacked`]), or `None` if the file does not exist.
    pub fn layer(&self) -> Result<Option<ConfigLayer>, ConfigLoadError> {
        read_layer(ConfigSource::User, &self.path())
    }
}

impl ConfigReader for LocalConfigReader {
    type Error = ConfigLoadError;

    fn read(&self) -> Result<JjunctionConfig, Self::Error> {
        self.layer()?;
        // TODO: deserialize typed fields and filter untrusted keys once defined
        Ok(JjunctionConfig::default())
    }
}
