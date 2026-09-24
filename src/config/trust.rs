//! Trust gate for untrusted (in-repo) configuration.
//!
//! Per `docs/design/config.md` (D1), `.jjunction/config.toml` is untrusted:
//! only a safe declarative subset of keys may be honored from it by default.
//! Anything with side effects — like `[[link]]` — additionally requires the
//! repo to be listed in the global `trusted-repos` key.
//!
//! [`trust`] is the one-command onboarding path (the direnv-allow model):
//! `jjn trust` appends the current workspace root to `trusted-repos`.

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use jj_lib::config::StackedConfig;
use toml_edit::Array;
use toml_edit::DocumentMut;
use toml_edit::Item;
use toml_edit::Value;

/// Returns `true` if `repo_root` is listed in the global `trusted-repos`.
pub fn is_trusted(global: &StackedConfig, repo_root: &Path) -> bool {
    let Ok(list) = global.get::<Vec<String>>("trusted-repos") else {
        return false;
    };
    let root = canonical(repo_root);
    list.iter().any(|entry| canonical(Path::new(entry)) == root)
}

/// Outcome of [`trust`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustOutcome {
    /// The root was appended to `trusted-repos`.
    Added,
    /// The root was already listed.
    AlreadyTrusted,
}

/// Errors while recording trust.
#[derive(Debug)]
pub enum TrustError {
    /// The global config file is not valid TOML.
    Parse {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
    /// Filesystem error.
    Io(io::Error),
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { path, source } => {
                write!(f, "invalid global config {}: {source}", path.display())
            }
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for TrustError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse { source, .. } => Some(source),
            Self::Io(err) => Some(err),
        }
    }
}

impl From<io::Error> for TrustError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Appends `repo_root` (canonicalized) to `trusted-repos` in the global
/// config file at `global_path`, creating the file when absent. Comments and
/// other keys are preserved (`toml_edit` in-place edit).
pub fn trust(global_path: &Path, repo_root: &Path) -> Result<TrustOutcome, TrustError> {
    let root = canonical(repo_root);
    let mut doc: DocumentMut = if global_path.is_file() {
        fs::read_to_string(global_path)?
            .parse()
            .map_err(|source| TrustError::Parse {
                path: global_path.to_owned(),
                source,
            })?
    } else {
        DocumentMut::new()
    };

    let already = doc
        .get("trusted-repos")
        .and_then(|item| item.as_array())
        .is_some_and(|list| {
            list.iter().any(|value| {
                value
                    .as_str()
                    .is_some_and(|entry| canonical(Path::new(entry)) == root)
            })
        });
    if already {
        return Ok(TrustOutcome::AlreadyTrusted);
    }

    let item = doc.entry("trusted-repos").or_insert(Item::None);
    if item.is_none() {
        *item = Item::Value(Value::Array(Array::new()));
    }
    item.as_array_mut()
        .expect("trusted-repos must be an array")
        .push(root.to_string_lossy().to_string());

    if let Some(parent) = global_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(global_path, doc.to_string())?;
    Ok(TrustOutcome::Added)
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
    use std::fs;

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

    fn stack_from_file(path: &std::path::Path) -> StackedConfig {
        let mut stacked = StackedConfig::empty();
        stacked
            .add_layer(ConfigLayer::load_from_file(ConfigSource::User, path.to_owned()).unwrap());
        stacked
    }

    #[test]
    fn trust_appends_and_is_idempotent() {
        let global_dir = tempfile::tempdir().unwrap();
        let global_path = global_dir.path().join("config.toml");
        let repo = tempfile::tempdir().unwrap();

        assert_eq!(
            trust(&global_path, repo.path()).unwrap(),
            TrustOutcome::Added
        );
        assert_eq!(
            trust(&global_path, repo.path()).unwrap(),
            TrustOutcome::AlreadyTrusted
        );

        let stacked = stack_from_file(&global_path);
        assert!(is_trusted(&stacked, repo.path()));
    }

    #[test]
    fn trust_creates_the_file_with_a_single_entry() {
        let global_dir = tempfile::tempdir().unwrap();
        let global_path = global_dir.path().join("jjunction/config.toml");
        let repo = tempfile::tempdir().unwrap();

        trust(&global_path, repo.path()).unwrap();

        let text = fs::read_to_string(&global_path).unwrap();
        assert!(text.contains("trusted-repos"));
        assert_eq!(
            text.matches(&repo.path().to_string_lossy().to_string())
                .count(),
            1,
            "no duplicate entries: {text}"
        );
    }

    #[test]
    fn trust_preserves_existing_content() {
        let global_dir = tempfile::tempdir().unwrap();
        let global_path = global_dir.path().join("config.toml");
        fs::write(&global_path, "# my global config\nsome-key = 1\n").unwrap();
        let repo = tempfile::tempdir().unwrap();

        trust(&global_path, repo.path()).unwrap();

        let text = fs::read_to_string(&global_path).unwrap();
        assert!(text.starts_with("# my global config"), "kept: {text}");
        assert!(text.contains("some-key = 1"));
        let stacked = stack_from_file(&global_path);
        assert!(is_trusted(&stacked, repo.path()));
    }
}
