//! Workspace enumeration and cross-workspace sync, powered by jj-lib.
//!
//! [`list`] loads the repo in-process (the same way the `jj` CLI does) and
//! returns every workspace root. [`sync_workspaces`] then mirrors files from
//! the default workspace into the others and applies the `direnv allow`
//! policy — so secondary workspaces get a working devenv/direnv setup
//! without anyone manually running `direnv allow` there.

use std::fmt;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use jj_lib::config::ConfigLayer;
use jj_lib::config::ConfigSource;
use jj_lib::config::StackedConfig;
use jj_lib::default_backend_factories::default_backend_factories;
use jj_lib::repo::RepoLoader;
use jj_lib::settings::UserSettings;
use jj_lib::workspace_store::SimpleWorkspaceStore;
use jj_lib::workspace_store::WorkspaceStore as _;

use crate::config::WorkspaceConfig;

/// Files mirrored into every non-default workspace when `[workspace].sync`
/// is not configured.
pub const DEFAULT_SYNC_FILES: &[&str] = &[".envrc", "devenv.nix", "devenv.yaml", "devenv.lock"];

/// One workspace of the current jj repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    /// Workspace name as recorded by jj.
    pub name: String,
    /// Absolute workspace root directory.
    pub root: PathBuf,
    /// True for the default workspace holding the repo itself (its `.jj/repo`
    /// is a directory; other workspaces carry a pointer file instead).
    pub is_default: bool,
}

/// Errors while enumerating workspaces.
#[derive(Debug)]
pub enum WorkspaceError {
    /// No `.jj` directory at or below the given path.
    NoRepo(PathBuf),
    /// The repo could not be loaded (wrong jj version, corrupted repo, …).
    Load(String),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRepo(path) => write!(f, "no jj repo at {}", path.display()),
            Self::Load(msg) => write!(f, "could not load repo: {msg}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

/// What happened to one synced file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    /// Link created pointing at the default workspace.
    Linked,
    /// Correct link already existed.
    AlreadyLinked,
    /// A real (tracked) file occupies the path; left untouched.
    Tracked,
    /// The link points elsewhere; needs `--force` to fix.
    WrongTarget {
        /// Expected (default workspace) path.
        expected: PathBuf,
        /// Current link target.
        found: PathBuf,
    },
    /// The source file does not exist in the default workspace.
    SourceMissing,
    /// A filesystem error occurred; the message describes it.
    Failed(String),
}

/// What happened to the `direnv allow` step for one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowStatus {
    /// `direnv allow` executed for this workspace.
    Allowed,
    /// Policy is `hint`; the command was printed instead of run.
    Hinted,
    /// `.envrc` differs from the default workspace; not auto-allowed.
    EnvrcMismatch,
    /// Policy is `never`.
    Disabled,
    /// `direnv` is not available in `PATH`.
    DirenvMissing,
    /// No `.envrc` to allow.
    NotApplicable,
}

impl fmt::Display for FileStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Linked => write!(f, "linked"),
            Self::AlreadyLinked => write!(f, "already linked"),
            Self::Tracked => write!(f, "tracked, skipped"),
            Self::WrongTarget { expected, found } => write!(
                f,
                "points to {} instead of {}",
                found.display(),
                expected.display()
            ),
            Self::SourceMissing => write!(f, "source missing"),
            Self::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

impl fmt::Display for AllowStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allowed => write!(f, "allowed"),
            Self::Hinted => write!(f, "hint: run direnv allow <workspace>"),
            Self::EnvrcMismatch => write!(f, ".envrc differs from default workspace, not allowed"),
            Self::Disabled => write!(f, "allow disabled"),
            Self::DirenvMissing => write!(f, "direnv not found in PATH"),
            Self::NotApplicable => write!(f, "no .envrc"),
        }
    }
}

/// Per-workspace sync report.
#[derive(Debug)]
pub struct WorkspaceReport {
    /// Workspace root that was synced.
    pub root: PathBuf,
    /// Outcome per synced file.
    pub files: Vec<(String, FileStatus)>,
    /// Outcome of the allow step.
    pub allow: AllowStatus,
}

/// Lists all workspaces of the repo that `workspace_root` belongs to. The
/// default workspace is marked with `is_default`.
pub fn list(workspace_root: &Path) -> Result<Vec<WorkspaceInfo>, WorkspaceError> {
    let jj_dir = workspace_root.join(".jj");
    if !jj_dir.is_dir() {
        return Err(WorkspaceError::NoRepo(workspace_root.to_owned()));
    }
    let repo_dir = resolve_repo_dir(&jj_dir)?;

    let settings = synth_settings().map_err(WorkspaceError::Load)?;
    let loader =
        RepoLoader::init_from_file_system(&settings, &repo_dir, &default_backend_factories())
            .map_err(|err| WorkspaceError::Load(err.to_string()))?;
    let repo = pollster::block_on(loader.load_at_head())
        .map_err(|err| WorkspaceError::Load(err.to_string()))?;
    let store = SimpleWorkspaceStore::load(&repo_dir)
        .map_err(|err| WorkspaceError::Load(err.to_string()))?;

    let mut out = Vec::new();
    for name in repo.view().wc_commit_ids().keys() {
        let Some(stored) = store
            .get_workspace_path(name)
            .map_err(|err| WorkspaceError::Load(err.to_string()))?
        else {
            continue;
        };
        // The store may keep paths relative to the repo dir; resolve them.
        let root = if stored.is_absolute() {
            stored
        } else {
            repo_dir.join(stored)
        };
        let root = root.canonicalize().unwrap_or(root);
        if !root.is_dir() {
            continue; // stale entry (workspace deleted on disk)
        }
        // The default workspace owns the repo dir; others point at it.
        let is_default = root.join(".jj").join("repo").is_dir();
        out.push(WorkspaceInfo {
            name: name.clone().into_string(),
            root,
            is_default,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Mirrors `config` files from the default workspace root into every other
/// workspace root and applies the allow policy. See [`DEFAULT_SYNC_FILES`].
pub fn sync_workspaces(
    default_root: &Path,
    other_roots: &[PathBuf],
    config: &WorkspaceConfig,
    force: bool,
) -> Vec<WorkspaceReport> {
    let files = config.files();
    other_roots
        .iter()
        .map(|root| {
            let statuses = files
                .iter()
                .map(|name| (name.clone(), sync_file(default_root, root, name, force)))
                .collect();
            WorkspaceReport {
                root: root.clone(),
                files: statuses,
                allow: apply_allow(default_root, root, config.allow),
            }
        })
        .collect()
}

fn sync_file(default_root: &Path, root: &Path, name: &str, force: bool) -> FileStatus {
    let source = default_root.join(name);
    let destination = root.join(name);

    match fs::symlink_metadata(&destination) {
        Ok(meta) if meta.file_type().is_symlink() => match fs::read_link(&destination) {
            Ok(found) if found == source => FileStatus::AlreadyLinked,
            Ok(_) if force => match replace_link(&source, &destination) {
                Ok(()) => FileStatus::Linked,
                Err(err) => FileStatus::Failed(err.to_string()),
            },
            Ok(found) => FileStatus::WrongTarget {
                expected: source,
                found,
            },
            Err(err) => FileStatus::Failed(err.to_string()),
        },
        Ok(_) => FileStatus::Tracked,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if !source.exists() {
                return FileStatus::SourceMissing;
            }
            if let Some(parent) = destination.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match symlink(&source, &destination) {
                Ok(()) => FileStatus::Linked,
                Err(err) => FileStatus::Failed(err.to_string()),
            }
        }
        Err(err) => FileStatus::Failed(err.to_string()),
    }
}

fn replace_link(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::remove_file(destination)?;
    symlink(source, destination)
}

/// Runs (or hints) `direnv allow <root>`, gated on `.envrc` being identical
/// to the default workspace's copy: allowing an unmodified copy of already
/// trusted content is safe, allowing arbitrary repo content is not.
fn apply_allow(
    default_root: &Path,
    root: &Path,
    policy: crate::config::AllowPolicy,
) -> AllowStatus {
    let default_envrc = default_root.join(".envrc");
    let workspace_envrc = root.join(".envrc");
    if !default_envrc.is_file() || !workspace_envrc.exists() {
        return AllowStatus::NotApplicable;
    }

    let identical = reads_equal(&default_envrc, &workspace_envrc);
    match policy {
        crate::config::AllowPolicy::Never => AllowStatus::Disabled,
        crate::config::AllowPolicy::Hint => AllowStatus::Hinted,
        crate::config::AllowPolicy::Auto => {
            if !identical {
                return AllowStatus::EnvrcMismatch;
            }
            let status = Command::new("direnv")
                .arg("allow")
                .arg(root)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            match status {
                Ok(exit) if exit.success() => AllowStatus::Allowed,
                Ok(_) => AllowStatus::EnvrcMismatch,
                Err(_) => AllowStatus::DirenvMissing,
            }
        }
    }
}

/// Byte-for-byte file equality — stronger and simpler than any hash for
/// local, small files.
fn reads_equal(a: &Path, b: &Path) -> bool {
    match (fs::read(a), fs::read(b)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// `.jj/repo` is a directory in the default workspace; in other workspaces
/// it is a file containing a relative path to the real repo dir.
fn resolve_repo_dir(jj_dir: &Path) -> Result<PathBuf, WorkspaceError> {
    let repo_dir = jj_dir.join("repo");
    if repo_dir.is_dir() {
        return Ok(repo_dir);
    }
    if repo_dir.is_file() {
        let buf = fs::read(&repo_dir)
            .map_err(|err| WorkspaceError::Load(format!("reading .jj/repo: {err}")))?;
        let path = jj_dir.join(String::from_utf8_lossy(&buf).trim());
        let path = path.canonicalize().unwrap_or(path);
        if path.is_dir() {
            return Ok(path);
        }
        return Err(WorkspaceError::Load(format!(
            "repo dir pointer is dangling: {}",
            path.display()
        )));
    }
    Err(WorkspaceError::Load("no .jj/repo".to_owned()))
}

/// Read-only repo loading only needs syntactically valid settings; identity
/// placeholders suffice and never land in any commit.
fn synth_settings() -> Result<UserSettings, String> {
    let mut config = StackedConfig::with_defaults();
    let layer = ConfigLayer::parse(
        ConfigSource::Default,
        r#"
        user.name = "jjunction"
        user.email = "jjunction@localhost"
        operation.username = "jjunction"
        operation.hostname = "localhost"
        "#,
    )
    .map_err(|err| err.to_string())?;
    config.add_layer(layer);
    UserSettings::from_config(config).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AllowPolicy;

    fn write(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        let other = dir.path().join("other");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&other).unwrap();
        write(&main.join(".envrc"), "use devenv\n");
        write(&main.join("devenv.nix"), "{}\n");
        (dir, main, other)
    }

    fn config(allow: AllowPolicy) -> WorkspaceConfig {
        WorkspaceConfig { sync: None, allow }
    }

    #[test]
    fn sync_links_missing_files() {
        let (_dir, main, other) = setup();
        let reports = sync_workspaces(
            &main,
            std::slice::from_ref(&other),
            &config(AllowPolicy::Never),
            false,
        );
        assert_eq!(
            reports[0].files[0],
            (".envrc".to_owned(), FileStatus::Linked)
        );
        assert_eq!(
            fs::read_link(other.join(".envrc")).unwrap(),
            main.join(".envrc")
        );
    }

    #[test]
    fn sync_is_idempotent_and_skips_tracked() {
        let (_dir, main, other) = setup();
        let cfg = config(AllowPolicy::Never);
        sync_workspaces(&main, std::slice::from_ref(&other), &cfg, false);
        let reports = sync_workspaces(&main, std::slice::from_ref(&other), &cfg, false);
        assert_eq!(reports[0].files[0].1, FileStatus::AlreadyLinked);

        write(&other.join("devenv.yaml"), "tracked: true\n");
        let reports = sync_workspaces(&main, std::slice::from_ref(&other), &cfg, false);
        let yaml = reports[0]
            .files
            .iter()
            .find(|(name, _)| name == "devenv.yaml")
            .unwrap();
        assert_eq!(yaml.1, FileStatus::Tracked);
    }

    #[test]
    fn sync_reports_wrong_target_and_force_fixes() {
        let (_dir, main, other) = setup();
        let decoy = main.parent().unwrap().join("decoy");
        write(&decoy, "x");
        symlink(&decoy, other.join(".envrc")).unwrap();

        let reports = sync_workspaces(
            &main,
            std::slice::from_ref(&other),
            &config(AllowPolicy::Never),
            false,
        );
        assert_eq!(
            reports[0].files[0].1,
            FileStatus::WrongTarget {
                expected: main.join(".envrc"),
                found: decoy,
            }
        );

        let reports = sync_workspaces(
            &main,
            std::slice::from_ref(&other),
            &config(AllowPolicy::Never),
            true,
        );
        assert_eq!(reports[0].files[0].1, FileStatus::Linked);
    }

    #[test]
    fn allow_gated_on_identical_envrc() {
        // fresh workspace: .envrc gets linked, then allowed (direnv may be
        // missing in test environments, in which case the status degrades)
        let (_dir, main, other) = setup();
        let reports = sync_workspaces(
            &main,
            std::slice::from_ref(&other),
            &config(AllowPolicy::Auto),
            false,
        );
        assert_eq!(reports[0].files[0].1, FileStatus::Linked);
        assert!(matches!(
            reports[0].allow,
            AllowStatus::Allowed | AllowStatus::DirenvMissing
        ));

        // workspace with a diverging .envrc of its own: tracked, never allowed
        let (_dir, main, other) = setup();
        write(&other.join(".envrc"), "use something-else\n");
        let reports = sync_workspaces(
            &main,
            std::slice::from_ref(&other),
            &config(AllowPolicy::Auto),
            false,
        );
        assert_eq!(reports[0].files[0].1, FileStatus::Tracked);
        assert_eq!(reports[0].allow, AllowStatus::EnvrcMismatch);
    }

    #[test]
    fn allow_hint_and_disabled() {
        let (_dir, main, other) = setup();
        let reports = sync_workspaces(
            &main,
            std::slice::from_ref(&other),
            &config(AllowPolicy::Hint),
            false,
        );
        assert_eq!(reports[0].allow, AllowStatus::Hinted);

        let reports = sync_workspaces(
            &main,
            std::slice::from_ref(&other),
            &config(AllowPolicy::Never),
            false,
        );
        assert_eq!(reports[0].allow, AllowStatus::Disabled);
    }

    #[test]
    fn reads_equal_is_byte_equality() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        write(&a, "same");
        write(&b, "same");
        assert!(reads_equal(&a, &b));
        write(&b, "same\n");
        assert!(!reads_equal(&a, &b));
    }
}
