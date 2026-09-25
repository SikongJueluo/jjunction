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
pub mod trust;

use std::path::Path;

use jj_lib::config::ConfigGetResultExt as _;
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

/// File name of the machine-generated lock file (resolved sub-repo commits),
/// inside [`LOCAL_CONFIG_DIR`]. See `docs/design/subrepo.md`.
pub const LOCAL_LOCK_FILE: &str = "lock.toml";

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

/// How `jjn` deals with `direnv allow` for non-default workspaces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllowPolicy {
    /// Run `direnv allow <workspace>` when `.envrc` matches the default
    /// workspace's copy byte for byte.
    Auto,
    /// Print the `direnv allow` command without running it.
    #[default]
    Hint,
    /// Do nothing.
    Never,
}

/// `[workspace]` section: cross-workspace file sync configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Files mirrored from the default workspace into every other workspace.
    /// Defaults to the devenv/direnv set ([`crate::workspace::DEFAULT_SYNC_FILES`]).
    pub sync: Option<Vec<String>>,
    /// `direnv allow` policy for non-default workspaces.
    #[serde(default)]
    pub allow: AllowPolicy,
}

impl WorkspaceConfig {
    /// Returns the effective sync file list.
    pub fn files(&self) -> Vec<String> {
        self.sync.clone().unwrap_or_else(|| {
            crate::workspace::DEFAULT_SYNC_FILES
                .iter()
                .map(|file| (*file).to_owned())
                .collect()
        })
    }
}

/// How secondary jj workspaces materialize `[[repo]]` entries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecondaryMode {
    /// Symlink each secondary workspace's target to the default workspace's
    /// checkout (one canonical clone shared by all workspaces).
    #[default]
    Link,
    /// Independent clone in the invoking workspace only.
    Clone,
    /// Secondary workspaces get nothing.
    Skip,
}

/// `[repos]` section: sub-repo materialization options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReposConfig {
    /// How secondary jj workspaces see sub-repos.
    #[serde(default)]
    pub secondary: SecondaryMode,
}

/// Loads the `[repos]` section from the local configuration stack. Missing
/// section yields the default configuration.
pub fn load_repos_config(
    local: &jj_lib::config::StackedConfig,
) -> Result<ReposConfig, jj_lib::config::ConfigGetError> {
    Ok(local
        .get::<ReposConfig>("repos")
        .optional()?
        .unwrap_or_default())
}

/// Loads the `[workspace]` section from the local configuration stack.
/// Missing section yields the default configuration.
pub fn load_workspace_config(
    local: &jj_lib::config::StackedConfig,
) -> Result<WorkspaceConfig, jj_lib::config::ConfigGetError> {
    Ok(local
        .get::<WorkspaceConfig>("workspace")
        .optional()?
        .unwrap_or_default())
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
    fn parses_repos_secondary_mode() {
        let mut local = StackedConfig::empty();
        local.add_layer(
            ConfigLayer::parse(ConfigSource::Repo, "[repos]\nsecondary = \"clone\"\n").unwrap(),
        );
        assert_eq!(
            load_repos_config(&local).unwrap().secondary,
            SecondaryMode::Clone
        );

        // absent section defaults to link; unknown values are rejected
        let empty = StackedConfig::empty();
        assert_eq!(
            load_repos_config(&empty).unwrap().secondary,
            SecondaryMode::Link
        );
        let mut bad = StackedConfig::empty();
        bad.add_layer(
            ConfigLayer::parse(ConfigSource::Repo, "[repos]\nsecondary = \"teleport\"\n").unwrap(),
        );
        assert!(load_repos_config(&bad).is_err());
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
