//! One-time workspace wiring: `jjn init`.
//!
//! The reaction loop that keeps workspaces in sync needs two hooks that are
//! awkward to hand-write:
//!
//! - `.envrc`: `watch_file` on the jj workspace index and the jjunction
//!   config, so direnv re-evaluates (and the enterShell hook re-applies) when
//!   either changes;
//! - `devenv.local.nix`: a background `jjn apply --quiet` in `enterShell`.
//!   devenv loads this file automatically and it is gitignored by default,
//!   so jjunction can own it wholesale instead of performing text surgery
//!   on the user's `devenv.nix` module.
//!
//! All edits are idempotent and marker-scoped: `.envrc` gets a managed
//! comment block appended once, `devenv.local.nix` is only written when
//! absent (a foreign file is never touched).

use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::config::LOCAL_CONFIG_DIR;
use crate::config::LOCAL_CONFIG_FILE;

const MARK_BEGIN: &str = "# jjunction:begin (managed by `jjn init`; edits inside are lost)";
const MARK_END: &str = "# jjunction:end";

const ENVRC_BLOCK: &str = "\
# Re-sync jj workspaces and jjunction state at the next prompt.
# (only the default workspace has a .jj/repo directory)
[ -d .jj/repo ] && watch_file .jj/repo/workspace_store/index
watch_file .jjunction/config.toml
";

const DEVENV_LOCAL: &str = r#"# Managed by `jjn init` (jjunction). devenv loads this file automatically
# and it is meant to stay out of version control.
{ pkgs, ... }:
{
  enterShell = ''
    if command -v jjn >/dev/null 2>&1; then
      (jjn apply --quiet >/dev/null 2>&1 &)
    fi
  '';
}
"#;

/// Outcome for one wiring step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireStatus {
    /// The hook was written.
    Created,
    /// The managed block was already present.
    AlreadyWired,
    /// The step does not apply (e.g. no devenv project).
    Skipped,
}

impl fmt::Display for WireStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => write!(f, "wired"),
            Self::AlreadyWired => write!(f, "already wired"),
            Self::Skipped => write!(f, "skipped"),
        }
    }
}

/// Errors while wiring hooks.
#[derive(Debug)]
pub enum HooksError {
    /// The file exists, contains content, but not our managed marker, so we
    /// refuse to touch it.
    ForeignFile(PathBuf),
    /// Filesystem error.
    Io(std::io::Error),
}

impl fmt::Display for HooksError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignFile(path) => write!(
                f,
                "{} exists without a jjunction marker; add the hook manually \
                 (see README) or move the file aside",
                path.display()
            ),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for HooksError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for HooksError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Report of one `jjn init` run.
#[derive(Debug)]
pub struct InitReport {
    /// `.jjunction/config.toml` creation status.
    pub config: WireStatus,
    /// `.envrc` managed block status.
    pub envrc: WireStatus,
    /// `devenv.local.nix` hook status.
    pub devenv_local: WireStatus,
}

/// Wires the workspace reaction loop (idempotent, marker-scoped).
pub fn init(root: &Path) -> Result<InitReport, HooksError> {
    let config = ensure_config(root)?;
    let envrc = wire_envrc(root)?;
    let devenv_local = wire_devenv_local(root)?;
    Ok(InitReport {
        config,
        envrc,
        devenv_local,
    })
}

/// Creates `.jjunction/config.toml` when absent.
fn ensure_config(root: &Path) -> Result<WireStatus, HooksError> {
    let dir = root.join(LOCAL_CONFIG_DIR);
    let path = dir.join(LOCAL_CONFIG_FILE);
    if path.exists() {
        return Ok(WireStatus::AlreadyWired);
    }
    fs::create_dir_all(&dir)?;
    fs::write(&path, "")?;
    Ok(WireStatus::Created)
}

/// Appends the managed `watch_file` block to `.envrc` (creating a minimal
/// file when absent). Never edits outside the markers.
fn wire_envrc(root: &Path) -> Result<WireStatus, HooksError> {
    let path = root.join(".envrc");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };
    if text.contains(MARK_BEGIN) {
        return Ok(WireStatus::AlreadyWired);
    }
    let mut block = String::new();
    if !text.is_empty() && !text.ends_with('\n') {
        block.push('\n');
    }
    block.push('\n');
    block.push_str(MARK_BEGIN);
    block.push('\n');
    block.push_str(ENVRC_BLOCK);
    block.push_str(MARK_END);
    block.push('\n');
    fs::write(&path, text + &block)?;
    Ok(WireStatus::Created)
}

/// Writes the enterShell hook into `devenv.local.nix` when this is a devenv
/// project and the file is absent or empty. A foreign file is refused.
fn wire_devenv_local(root: &Path) -> Result<WireStatus, HooksError> {
    let is_devenv = root.join("devenv.yaml").is_file() || root.join("devenv.nix").is_file();
    if !is_devenv {
        return Ok(WireStatus::Skipped);
    }
    let path = root.join("devenv.local.nix");
    match fs::read_to_string(&path) {
        Ok(text) => {
            if text.contains("jjn init") {
                Ok(WireStatus::AlreadyWired)
            } else if text.trim().is_empty() {
                fs::write(&path, DEVENV_LOCAL)?;
                Ok(WireStatus::Created)
            } else {
                Err(HooksError::ForeignFile(path))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::write(&path, DEVENV_LOCAL)?;
            Ok(WireStatus::Created)
        }
        Err(err) => Err(err.into()),
    }
}

/// Doctor-side wiring check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HooksHealth {
    /// Both applicable hooks are in place.
    Ok,
    /// `.envrc` lacks the managed watch block.
    EnvrcMissing,
    /// This is a devenv project but `devenv.local.nix` lacks the hook.
    DevenvLocalMissing,
}

impl HooksHealth {
    /// Returns `true` when no action is needed.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

impl fmt::Display for HooksHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => write!(f, "ok"),
            Self::EnvrcMissing => write!(f, ".envrc lacks the jjunction watch block (jjn init)"),
            Self::DevenvLocalMissing => {
                write!(f, "devenv.local.nix lacks the jjn apply hook (jjn init)")
            }
        }
    }
}

/// Checks the wiring without modifying anything.
pub fn doctor(root: &Path) -> HooksHealth {
    let envrc_ok = fs::read_to_string(root.join(".envrc"))
        .map(|text| text.contains(MARK_BEGIN))
        .unwrap_or(false);
    let is_devenv = root.join("devenv.yaml").is_file() || root.join("devenv.nix").is_file();
    let devenv_ok = !is_devenv
        || fs::read_to_string(root.join("devenv.local.nix"))
            .map(|text| text.contains("jjn init"))
            .unwrap_or(false);
    if !envrc_ok {
        HooksHealth::EnvrcMissing
    } else if !devenv_ok {
        HooksHealth::DevenvLocalMissing
    } else {
        HooksHealth::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent_and_marker_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".envrc"), "use devenv\n").unwrap();

        let report = init(root).unwrap();
        assert_eq!(report.config, WireStatus::Created);
        assert_eq!(report.envrc, WireStatus::Created);
        assert_eq!(report.devenv_local, WireStatus::Skipped); // no devenv project

        let text = fs::read_to_string(root.join(".envrc")).unwrap();
        assert!(
            text.starts_with("use devenv\n"),
            "user content kept: {text}"
        );
        assert!(text.contains(MARK_BEGIN));
        assert!(text.contains("watch_file .jjunction/config.toml"));
        assert!(text.ends_with("# jjunction:end\n"));

        let report = init(root).unwrap();
        assert_eq!(report.envrc, WireStatus::AlreadyWired);
        assert_eq!(report.config, WireStatus::AlreadyWired);
        assert_eq!(
            fs::read_to_string(root.join(".envrc")).unwrap(),
            text,
            "second run must not change anything"
        );
    }

    #[test]
    fn init_creates_envrc_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        init(root).unwrap();

        let text = fs::read_to_string(root.join(".envrc")).unwrap();
        assert!(text.contains(MARK_BEGIN));
        assert!(text.contains("[ -d .jj/repo ] && watch_file"));
    }

    #[test]
    fn init_wires_devenv_local_nix_for_devenv_projects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("devenv.yaml"), "").unwrap();

        let report = init(root).unwrap();
        assert_eq!(report.devenv_local, WireStatus::Created);

        let text = fs::read_to_string(root.join("devenv.local.nix")).unwrap();
        assert!(text.contains("jjn apply --quiet"));
        assert!(text.contains("enterShell"));

        assert_eq!(init(root).unwrap().devenv_local, WireStatus::AlreadyWired);
    }

    #[test]
    fn init_refuses_foreign_devenv_local_nix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("devenv.yaml"), "").unwrap();
        fs::write(
            root.join("devenv.local.nix"),
            "{ pkgs, ... }: { packages = [ pkgs.hello ]; }\n",
        )
        .unwrap();

        assert!(matches!(init(root), Err(HooksError::ForeignFile(_))));
        // untouched
        assert!(
            fs::read_to_string(root.join("devenv.local.nix"))
                .unwrap()
                .contains("hello")
        );
    }

    #[test]
    fn doctor_reports_missing_wiring() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        assert_eq!(doctor(root), HooksHealth::EnvrcMissing);

        init(root).unwrap();
        assert_eq!(doctor(root), HooksHealth::Ok);

        fs::write(root.join("devenv.yaml"), "").unwrap();
        assert_eq!(doctor(root), HooksHealth::DevenvLocalMissing);

        init(root).unwrap();
        assert_eq!(doctor(root), HooksHealth::Ok);
    }
}
