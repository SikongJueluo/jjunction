//! Configuration reading interface.
//!
//! jjunction has two configuration layers:
//!
//! - **global**: a user-level configuration file, overriding the built-in
//!   defaults,
//! - **local**: a project-level configuration file (`.jjunction/config.toml`
//!   at the workspace root), treated as **untrusted** by default and layered
//!   *below* the global one.
//!
//! The on-disk format is TOML, carried by [`jj_lib::config::StackedConfig`] on
//! top of `toml_edit`. Layer assembly lives here once implemented; see
//! `docs/design/config.md` for the decision record and the planned
//! `global.rs` / `local.rs` / `trust.rs` submodules, and
//! `docs/research/rust-config-libraries.md` for the library survey.
//!
//! File formats and lookup paths are not fully wired yet; this module only
//! fixes the reading interface. Concrete loaders should implement
//! [`ConfigReader`].

pub mod global;
pub mod local;

use std::path::Path;

use jj_lib::config::ConfigLayer;
use jj_lib::config::ConfigLoadError;
use jj_lib::config::ConfigSource;
use jj_lib::config::StackedConfig;

/// Directory name of the project-local configuration, at the workspace
/// root. The local file is `.jjunction/config.toml` — a directory to avoid
/// confusion with jj's own `jj*.toml` configuration names.
pub const LOCAL_CONFIG_DIR: &str = ".jjunction";

/// File name of the project-local (untrusted) configuration file, inside
/// [`LOCAL_CONFIG_DIR`].
pub const LOCAL_CONFIG_FILE: &str = "config.toml";

/// Directory name of the global configuration, relative to the platform
/// config directory (e.g. `~/.config/jjunction/`).
pub const GLOBAL_CONFIG_DIR: &str = "jjunction";

/// File name of the global configuration file.
pub const GLOBAL_CONFIG_FILE: &str = "config.toml";

/// Assembles the effective configuration stack:
///
/// 1. project-local layer (untrusted, lowest),
/// 2. user-global layer on top.
///
/// Both layers use [`ConfigSource::User`]; `StackedConfig` orders
/// same-source layers by insertion, so the local layer must be added first
/// (see `docs/design/config.md`, D2). Env and CLI override layers will be
/// added on top once needed.
pub fn load_stacked(
    local: &local::LocalConfigReader,
    global: &global::GlobalConfigReader,
) -> Result<StackedConfig, ConfigLoadError> {
    let mut stacked = StackedConfig::empty();
    if let Some(layer) = local.layer()? {
        stacked.add_layer(layer);
    }
    if let Some(layer) = global.layer()? {
        stacked.add_layer(layer);
    }
    Ok(stacked)
}

/// Parses `path` as a configuration layer, or `Ok(None)` if it is absent.
pub(super) fn read_layer(
    source: ConfigSource,
    path: &Path,
) -> Result<Option<ConfigLayer>, ConfigLoadError> {
    if path.is_file() {
        ConfigLayer::load_from_file(source, path.to_owned()).map(Some)
    } else {
        Ok(None)
    }
}

/// Root configuration of jjunction.
///
/// Intentionally empty for now; fields will be added once the configuration
/// keys are decided.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JjunctionConfig {
    // Reserved: fields will be defined together with the config keys.
}

/// Reads jjunction configuration from one layer (global or local).
///
/// Implementations decide where to load from and how to parse, so the format
/// choice can be made later without touching callers.
pub trait ConfigReader {
    /// Error type produced by this reader.
    type Error: std::error::Error;

    /// Reads and parses the configuration layer.
    fn read(&self) -> Result<JjunctionConfig, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubReader;

    impl ConfigReader for StubReader {
        type Error = std::convert::Infallible;

        fn read(&self) -> Result<JjunctionConfig, Self::Error> {
            Ok(JjunctionConfig::default())
        }
    }

    #[test]
    fn reads_config_via_trait() {
        let config = StubReader.read().unwrap();
        assert_eq!(config, JjunctionConfig::default());
    }

    #[test]
    fn stacks_local_below_global() {
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let local_dir = dir.path().join(LOCAL_CONFIG_DIR);
        fs::create_dir_all(&local_dir).unwrap();
        fs::write(
            local_dir.join(LOCAL_CONFIG_FILE),
            "key = \"local\"\nonly-local = 1\n",
        )
        .unwrap();
        fs::write(dir.path().join("global.toml"), "key = \"global\"\n").unwrap();

        let local = local::LocalConfigReader::new(dir.path());
        let global = global::GlobalConfigReader::from_path(dir.path().join("global.toml"));
        let stacked = load_stacked(&local, &global).unwrap();

        assert_eq!(stacked.get::<String>("key").unwrap(), "global");
        assert_eq!(stacked.get::<i64>("only-local").unwrap(), 1);
    }

    #[test]
    fn missing_files_give_empty_stack() {
        let dir = tempfile::tempdir().unwrap();
        let local = local::LocalConfigReader::new(dir.path());
        let global = global::GlobalConfigReader::from_path(dir.path().join("absent.toml"));

        let stacked = load_stacked(&local, &global).unwrap();

        assert!(stacked.get::<String>("key").is_err());
    }

    #[test]
    fn malformed_local_file_is_an_error() {
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let local_dir = dir.path().join(LOCAL_CONFIG_DIR);
        fs::create_dir_all(&local_dir).unwrap();
        fs::write(local_dir.join(LOCAL_CONFIG_FILE), "not = [closed\n").unwrap();

        let local = local::LocalConfigReader::new(dir.path());

        assert!(local.layer().is_err());
    }
}
