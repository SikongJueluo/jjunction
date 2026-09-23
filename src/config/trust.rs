//! Trust gate for untrusted (in-repo) configuration.
//!
//! Per `docs/design/config.md` (D1), `.jjunction/config.toml` is untrusted:
//! only a safe declarative subset of keys may be honored from it by default.
//! Anything with side effects — like `[[link]]` — additionally requires the
//! repo to be listed in the global `trusted-repos` key. This is the first
//! consumer of that gate.

use std::path::Path;
use std::path::PathBuf;

use jj_lib::config::StackedConfig;

/// Returns `true` if `repo_root` is listed in the global `trusted-repos`.
pub fn is_trusted(global: &StackedConfig, repo_root: &Path) -> bool {
    let Ok(list) = global.get::<Vec<String>>("trusted-repos") else {
        return false;
    };
    let root = canonical(repo_root);
    list.iter().any(|entry| canonical(Path::new(entry)) == root)
}

/// Canonicalizes `path`, falling back to the literal path when it does not
/// exist yet (e.g. a configured repo that has not been created).
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jj_lib::config::ConfigLayer;
    use jj_lib::config::ConfigSource;

    fn stack_with_trusted(paths: &[&str]) -> StackedConfig {
        let mut stacked = StackedConfig::empty();
        let text = paths
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ");
        stacked.add_layer(
            ConfigLayer::parse(ConfigSource::User, &format!("trusted-repos = [{text}]")).unwrap(),
        );
        stacked
    }

    #[test]
    fn trusted_repo_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        let stack = stack_with_trusted(&[dir.path().to_str().unwrap()]);
        assert!(is_trusted(&stack, dir.path()));
    }

    #[test]
    fn untrusted_repo_is_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let stack = stack_with_trusted(&[other.path().to_str().unwrap()]);
        assert!(!is_trusted(&stack, dir.path()));
    }

    #[test]
    fn missing_key_is_untrusted() {
        let dir = tempfile::tempdir().unwrap();
        let stacked = StackedConfig::empty();
        assert!(!is_trusted(&stacked, dir.path()));
    }
}
