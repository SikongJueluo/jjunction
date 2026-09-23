//! Machine-generated lock file: `.jjunction/lock.toml`.
//!
//! Records the resolved commit of every managed sub-repo. The manifest
//! (`[[repo]]` in the local config) describes intent; this file describes the
//! concrete state, and is meant to be tracked in the outer repo for
//! reproducibility (see `docs/design/subrepo.md`, D2/D3).
//!
//! Write discipline (D3, "the diff contains only semantic change"):
//! 1. table-per-repo (`[repo.<name>]`), never arrays;
//! 2. in-place edits only — untouched bytes are rewritten byte for byte;
//! 3. one field per repo (`commit`); no volatile metadata;
//! 4. `version` is a constant that only changes on a format break;
//! 5. first write sorts by name; later additions append, never re-sort.

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use toml_edit::DocumentMut;
use toml_edit::Item;
use toml_edit::Table;
use toml_edit::value;

/// The only lock format understood so far. Bump on a format break only.
pub const LOCK_VERSION: i64 = 1;

/// Errors while loading or writing the lock file.
#[derive(Debug)]
pub enum LockError {
    /// The file exists but is not valid TOML.
    Parse {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
    /// The file declares an unsupported `version`.
    UnsupportedVersion(i64),
    /// The file has entries but no `version`.
    MissingVersion,
    /// An entry under `[repo.<name>]` is not a table with a `commit` string.
    BadEntry(String),
    /// Filesystem error.
    Io(io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { path, source } => {
                write!(f, "invalid lock file {}: {source}", path.display())
            }
            Self::UnsupportedVersion(v) => {
                write!(
                    f,
                    "lock file version {v} is not supported (expected {LOCK_VERSION})"
                )
            }
            Self::MissingVersion => write!(f, "lock file has entries but no version"),
            Self::BadEntry(name) => write!(f, "lock entry repo.{name} lacks a commit string"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse { source, .. } => Some(source),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for LockError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// The parsed lock document plus change tracking for idempotent saves.
#[derive(Debug)]
pub struct RepoLock {
    path: PathBuf,
    doc: DocumentMut,
    changed: bool,
}

impl RepoLock {
    /// Loads the lock file at `path`, or prepares a fresh document (with the
    /// constant `version`) when it does not exist yet.
    pub fn load_or_create(path: &Path) -> Result<Self, LockError> {
        if path.is_file() {
            let text = fs::read_to_string(path)?;
            let doc: DocumentMut = text.parse().map_err(|source| LockError::Parse {
                path: path.to_owned(),
                source,
            })?;
            let repo_table_is_empty = doc
                .get("repo")
                .and_then(|item| item.as_table())
                .is_none_or(|table| table.is_empty());
            match doc.get("version").and_then(|item| item.as_integer()) {
                Some(v) if v == LOCK_VERSION => {}
                Some(v) => return Err(LockError::UnsupportedVersion(v)),
                // Empty (or repo-less) files may omit the version; the next
                // save writes it.
                None if repo_table_is_empty => {}
                None => return Err(LockError::MissingVersion),
            }
            Ok(Self {
                path: path.to_owned(),
                doc,
                changed: false,
            })
        } else {
            let mut doc = DocumentMut::new();
            doc["version"] = value(LOCK_VERSION);
            Ok(Self {
                path: path.to_owned(),
                doc,
                changed: true,
            })
        }
    }

    /// Returns the locked commit of repo `name`, if recorded.
    pub fn commit(&self, name: &str) -> Option<&str> {
        self.doc
            .get("repo")?
            .as_table()?
            .get(name)?
            .as_table()?
            .get("commit")?
            .as_str()
    }

    /// Records the commit of repo `name`: in place when the entry exists,
    /// appended at the end otherwise (discipline 2 and 5). Setting the value
    /// that is already recorded is a no-op.
    pub fn set_commit(&mut self, name: &str, commit: &str) {
        if self.commit(name) == Some(commit) {
            return;
        }
        let repo = self.repo_table_mut();
        match repo.get_mut(name).and_then(|item| item.as_table_mut()) {
            Some(table) => table["commit"] = value(commit),
            None => {
                let mut table = Table::new();
                table["commit"] = value(commit);
                repo.insert(name, Item::Table(table));
            }
        }
        self.changed = true;
    }

    /// Drops the entry for `name`; returns whether one existed.
    pub fn remove(&mut self, name: &str) -> bool {
        let Some(repo) = self
            .doc
            .get_mut("repo")
            .and_then(|item| item.as_table_mut())
        else {
            return false;
        };
        let removed = repo.remove(name).is_some();
        self.changed |= removed;
        removed
    }

    /// Whether any change since load still needs saving.
    pub fn is_changed(&self) -> bool {
        self.changed
    }

    /// Writes the document back, byte-identical outside changed entries.
    pub fn save(&self) -> Result<(), LockError> {
        fs::write(&self.path, self.doc.to_string())?;
        Ok(())
    }

    fn repo_table_mut(&mut self) -> &mut Table {
        let doc = &mut self.doc;
        let item = doc
            .entry("repo")
            .or_insert_with(|| Item::Table(implicit_table()));
        if item.is_none() {
            *item = Item::Table(implicit_table());
        }
        item.as_table_mut().expect("repo must be a table")
    }
}

/// An implicit `[repo]` table renders children as `[repo.<name>]` headers
/// without its own `[repo]` line.
fn implicit_table() -> Table {
    let mut table = Table::new();
    table.set_implicit(true);
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(n: usize) -> String {
        format!("{n:040x}")
    }

    fn write_lock(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("lock.toml");
        fs::write(&path, text).unwrap();
        path
    }

    const TWO_REPOS: &str = "\
version = 1

[repo.alpha]
commit = \"0000000000000000000000000000000000000001\"

[repo.beta]
commit = \"0000000000000000000000000000000000000002\"
";

    #[test]
    fn new_file_gets_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock.toml");
        let mut lock = RepoLock::load_or_create(&path).unwrap();
        lock.set_commit("alpha", &commit(1));
        lock.save().unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            format!("version = 1\n\n[repo.alpha]\ncommit = \"{}\"\n", commit(1))
        );
    }

    #[test]
    fn updating_one_repo_changes_only_that_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lock(dir.path(), TWO_REPOS);

        let mut lock = RepoLock::load_or_create(&path).unwrap();
        lock.set_commit("beta", &commit(3));
        lock.save().unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            TWO_REPOS.replace(&commit(2), &commit(3)),
            "diff must be exactly the beta commit line"
        );
    }

    #[test]
    fn setting_the_same_commit_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lock(dir.path(), TWO_REPOS);

        let mut lock = RepoLock::load_or_create(&path).unwrap();
        lock.set_commit("alpha", &commit(1));
        assert!(!lock.is_changed());
    }

    #[test]
    fn appending_keeps_existing_lines_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lock(dir.path(), TWO_REPOS);

        let mut lock = RepoLock::load_or_create(&path).unwrap();
        lock.set_commit("gamma", &commit(9));
        lock.save().unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(TWO_REPOS), "existing entries untouched");
        assert!(text.contains(&format!("[repo.gamma]\ncommit = \"{}\"", commit(9))));
    }

    #[test]
    fn remove_drops_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lock(dir.path(), TWO_REPOS);

        let mut lock = RepoLock::load_or_create(&path).unwrap();
        assert!(lock.remove("alpha"));
        assert!(!lock.remove("absent"));
        assert_eq!(lock.commit("alpha"), None);
        lock.save().unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("alpha"));
        assert!(text.contains("beta"));
    }

    #[test]
    fn rejects_unsupported_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lock(dir.path(), "version = 99\n");
        assert!(matches!(
            RepoLock::load_or_create(&path),
            Err(LockError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn rejects_entries_without_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lock(
            dir.path(),
            "[repo.alpha]\ncommit = \"0000000000000000000000000000000000000001\"\n",
        );
        assert!(matches!(
            RepoLock::load_or_create(&path),
            Err(LockError::MissingVersion)
        ));
    }

    #[test]
    fn empty_file_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lock(dir.path(), "");
        assert!(RepoLock::load_or_create(&path).is_ok());
    }
}
