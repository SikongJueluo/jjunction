//! Managed links: `[[link]]` entries declared in the local (untrusted)
//! configuration.
//!
//! Each entry creates one symlink at `target` (relative to the workspace
//! root) pointing at `source`. `source` accepts absolute paths, `~/`
//! prefixes, and paths relative to the workspace root. Entries only take
//! effect when the repo is trusted (see [`crate::config::trust`]).
//!
//! `type` is `link` today; future kinds (copy, hardlink, template, …) will
//! extend [`LinkType`].
//!
//! Entries may be scoped with two optional whitelist selectors, both
//! default-on (absent = applies everywhere): `workspaces` (jj workspace
//! names, case-sensitive; the default workspace is named `default`) and
//! `machines` (machine identities resolved by
//! [`crate::config::resolve_machine`], compared normalized). See
//! [`skip_reason`].

use std::fmt;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use jj_lib::config::ConfigGetError;
use jj_lib::config::ConfigGetResultExt as _;
use jj_lib::config::StackedConfig;
use serde::Deserialize;
use serde::Serialize;

/// How an entry materializes on disk. Only `link` for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum LinkType {
    /// A symbolic link.
    Link,
}

/// One `[[link]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LinkEntry {
    /// Real file or directory the link points to.
    pub source: String,
    /// Link location, relative to the workspace root.
    pub target: String,
    /// Entry kind; currently only `link`.
    #[serde(rename = "type")]
    pub link_type: LinkType,
    /// jj workspace names this entry applies to; absent = every workspace.
    pub workspaces: Option<Vec<String>>,
    /// Machine names this entry applies to (compared normalized, see
    /// [`crate::config::normalize_machine`]); absent = every machine.
    pub machines: Option<Vec<String>>,
}

/// Outcome of applying one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyStatus {
    /// The link was created.
    Created,
    /// The link already existed with the right target.
    AlreadyLinked,
}

impl fmt::Display for ApplyStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::AlreadyLinked => write!(f, "already linked"),
        }
    }
}

/// Errors while applying one entry.
#[derive(Debug)]
pub enum LinkError {
    /// The link path exists as a real file or directory.
    Occupied(PathBuf),
    /// The link points somewhere else than configured.
    Mismatch {
        /// Configured source.
        expected: PathBuf,
        /// Current link target.
        found: PathBuf,
    },
    /// The target path escapes the workspace root.
    EscapedTarget(String),
    /// Filesystem error.
    Io(std::io::Error),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Occupied(path) => write!(f, "occupied by a real file: {}", path.display()),
            Self::Mismatch { expected, found } => write!(
                f,
                "points to {} instead of {}",
                found.display(),
                expected.display()
            ),
            Self::EscapedTarget(target) => write!(f, "target escapes workspace root: {target}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for LinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for LinkError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Health of one entry as reported by [`doctor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoctorStatus {
    /// Link exists and points at the configured source.
    Ok,
    /// The source does not exist.
    MissingSource,
    /// The link has not been created yet.
    MissingLink,
    /// Something other than a symlink occupies the target.
    NotALink,
    /// The link points somewhere else.
    WrongTarget {
        /// Configured source.
        expected: PathBuf,
        /// Current link target.
        found: PathBuf,
    },
    /// The target path escapes the workspace root.
    EscapedTarget,
}

impl DoctorStatus {
    /// Returns `true` when the entry needs no action.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

impl fmt::Display for DoctorStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => write!(f, "ok"),
            Self::MissingSource => write!(f, "missing source"),
            Self::MissingLink => write!(f, "missing link"),
            Self::NotALink => write!(f, "not a link"),
            Self::WrongTarget { expected, found } => write!(
                f,
                "points to {} instead of {}",
                found.display(),
                expected.display()
            ),
            Self::EscapedTarget => write!(f, "target escapes workspace root"),
        }
    }
}

/// One entry with its [`DoctorStatus`].
#[derive(Debug)]
pub struct Diagnosis {
    /// The checked entry.
    pub entry: LinkEntry,
    /// What was found.
    pub status: DoctorStatus,
}

/// Loads `[[link]]` entries from the local configuration stack. Missing
/// entries yield an empty list.
pub fn load_entries(local: &StackedConfig) -> Result<Vec<LinkEntry>, ConfigGetError> {
    Ok(local
        .get::<Vec<LinkEntry>>("link")
        .optional()?
        .unwrap_or_default())
}

/// Why an entry does not apply on this machine / in this workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The machine identity is not in `machines`.
    Machine {
        /// Current machine identity (already normalized).
        current: String,
        /// Configured selector values, verbatim.
        allowed: Vec<String>,
    },
    /// The workspace is not in `workspaces`, or could not be determined.
    Workspace {
        /// Current workspace name, when determinable.
        current: Option<String>,
        /// Configured selector values, verbatim.
        allowed: Vec<String>,
    },
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Machine { current, allowed } => {
                write!(f, "machine {current:?} not in {allowed:?}")
            }
            Self::Workspace {
                current: Some(current),
                allowed,
            } => write!(f, "workspace {current:?} not in {allowed:?}"),
            Self::Workspace {
                current: None,
                allowed,
            } => write!(f, "workspace undeterminable, entry scoped to {allowed:?}"),
        }
    }
}

/// Returns `true` when `entry` applies in `workspace` on `machine`.
/// Absent selectors match everything (default-on).
pub fn in_scope(entry: &LinkEntry, workspace: Option<&str>, machine: &str) -> bool {
    skip_reason(entry, workspace, machine).is_none()
}

/// Returns why `entry` is skipped here, or `None` when it applies.
///
/// Machine names are normalized on both sides
/// ([`crate::config::normalize_machine`]); [`crate::config::resolve_machine`]
/// returns an already-normalized identity. Workspace names compare exactly —
/// jj workspace names are user-chosen, case-sensitive identifiers. An entry
/// scoped to workspaces is skipped when the current workspace name cannot
/// be determined (no `.jj` at the root).
pub fn skip_reason(
    entry: &LinkEntry,
    workspace: Option<&str>,
    machine: &str,
) -> Option<SkipReason> {
    let machine = crate::config::normalize_machine(machine);
    if let Some(allowed) = &entry.machines
        && !allowed
            .iter()
            .any(|name| crate::config::normalize_machine(name) == machine)
    {
        return Some(SkipReason::Machine {
            current: machine,
            allowed: allowed.clone(),
        });
    }
    if let Some(allowed) = &entry.workspaces
        && !workspace.is_some_and(|current| allowed.iter().any(|name| name == current))
    {
        return Some(SkipReason::Workspace {
            current: workspace.map(str::to_owned),
            allowed: allowed.clone(),
        });
    }
    None
}

/// Applies one entry: creates the symlink, or reports why it cannot.
pub fn apply_one(root: &Path, entry: &LinkEntry, force: bool) -> Result<ApplyStatus, LinkError> {
    if !target_within_root(&entry.target) {
        return Err(LinkError::EscapedTarget(entry.target.clone()));
    }
    let source = resolve_source(root, &entry.source);
    let link_path = root.join(&entry.target);

    match fs::symlink_metadata(&link_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                let found = fs::read_link(&link_path)?;
                if found == source {
                    return Ok(ApplyStatus::AlreadyLinked);
                }
                if !force {
                    return Err(LinkError::Mismatch {
                        expected: source,
                        found,
                    });
                }
                fs::remove_file(&link_path)?;
            } else if !force {
                return Err(LinkError::Occupied(link_path));
            } else if meta.is_dir() {
                // Never remove a real directory, even with --force.
                return Err(LinkError::Occupied(link_path));
            } else {
                fs::remove_file(&link_path)?;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent)?;
    }
    symlink(&source, &link_path)?;
    Ok(ApplyStatus::Created)
}

/// Checks all entries without modifying anything.
pub fn doctor(root: &Path, entries: &[LinkEntry]) -> Vec<Diagnosis> {
    entries
        .iter()
        .map(|entry| Diagnosis {
            entry: entry.clone(),
            status: diagnose(root, entry),
        })
        .collect()
}

fn diagnose(root: &Path, entry: &LinkEntry) -> DoctorStatus {
    if !target_within_root(&entry.target) {
        return DoctorStatus::EscapedTarget;
    }
    let source = resolve_source(root, &entry.source);
    let link_path = root.join(&entry.target);

    if !source.exists() {
        return DoctorStatus::MissingSource;
    }
    match fs::symlink_metadata(&link_path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let found = fs::read_link(&link_path).unwrap_or_default();
            if found == source {
                DoctorStatus::Ok
            } else {
                DoctorStatus::WrongTarget {
                    expected: source,
                    found,
                }
            }
        }
        Ok(_) => DoctorStatus::NotALink,
        Err(_) => DoctorStatus::MissingLink,
    }
}

/// Resolves an entry source: `~/` expands to the home directory, relative
/// paths resolve against the workspace root.
fn resolve_source(root: &Path, source: &str) -> PathBuf {
    if let Some(rest) = source.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(rest)
    } else if Path::new(source).is_absolute() {
        PathBuf::from(source)
    } else {
        root.join(source)
    }
}

/// A target must be a relative path staying inside the workspace root.
pub(crate) fn target_within_root(target: &str) -> bool {
    let path = Path::new(target);
    !path.is_absolute()
        && !path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(source: &str, target: &str) -> LinkEntry {
        LinkEntry {
            source: source.to_owned(),
            target: target.to_owned(),
            link_type: LinkType::Link,
            workspaces: None,
            machines: None,
        }
    }

    #[test]
    fn apply_creates_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("assets/a.txt"), "hi").unwrap();

        let e = entry("assets/a.txt", "links/a");

        assert_eq!(apply_one(root, &e, false).unwrap(), ApplyStatus::Created);
        assert_eq!(
            fs::read_link(root.join("links/a")).unwrap(),
            root.join("assets/a.txt")
        );
        assert_eq!(
            apply_one(root, &e, false).unwrap(),
            ApplyStatus::AlreadyLinked
        );
    }

    #[test]
    fn apply_reports_mismatch_and_force_fixes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "a").unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();
        fs::create_dir_all(root.join("links")).unwrap();
        symlink(root.join("b.txt"), root.join("links/a")).unwrap();

        let e = entry("a.txt", "links/a");
        assert!(matches!(
            apply_one(root, &e, false),
            Err(LinkError::Mismatch { .. })
        ));
        assert_eq!(apply_one(root, &e, true).unwrap(), ApplyStatus::Created);
        assert_eq!(
            fs::read_link(root.join("links/a")).unwrap(),
            root.join("a.txt")
        );
    }

    #[test]
    fn apply_refuses_occupied_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "a").unwrap();
        fs::create_dir_all(root.join("links")).unwrap();
        fs::write(root.join("links/a"), "real file").unwrap();

        let e = entry("a.txt", "links/a");
        assert!(matches!(
            apply_one(root, &e, false),
            Err(LinkError::Occupied(_))
        ));
        // force replaces a regular file
        assert_eq!(apply_one(root, &e, true).unwrap(), ApplyStatus::Created);

        // but never a real directory
        fs::create_dir_all(root.join("links/dir")).unwrap();
        let e2 = entry("a.txt", "links/dir");
        assert!(matches!(
            apply_one(root, &e2, true),
            Err(LinkError::Occupied(_))
        ));
    }

    #[test]
    fn apply_rejects_escaping_target() {
        let dir = tempfile::tempdir().unwrap();
        let e = entry("a.txt", "../outside");
        assert!(matches!(
            apply_one(dir.path(), &e, false),
            Err(LinkError::EscapedTarget(_))
        ));
    }

    #[test]
    fn doctor_reports_each_status() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "a").unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();

        // ok
        apply_one(root, &entry("a.txt", "ok-link"), false).unwrap();
        // missing source (link may even exist)
        // missing link
        // not a link
        fs::create_dir_all(root.join("occupied")).unwrap();
        fs::write(root.join("occupied/x"), "real").unwrap();
        // wrong target
        fs::create_dir_all(root.join("d")).unwrap();
        symlink(root.join("b.txt"), root.join("d/wrong")).unwrap();

        let entries = vec![
            entry("a.txt", "ok-link"),
            entry("absent.txt", "missing-source"),
            entry("a.txt", "missing-link"),
            entry("a.txt", "occupied"),
            entry("a.txt", "d/wrong"),
            entry("a.txt", "../escape"),
        ];
        let results: Vec<_> = doctor(root, &entries)
            .into_iter()
            .map(|d| d.status)
            .collect();

        assert_eq!(
            results,
            vec![
                DoctorStatus::Ok,
                DoctorStatus::MissingSource,
                DoctorStatus::MissingLink,
                DoctorStatus::NotALink,
                DoctorStatus::WrongTarget {
                    expected: root.join("a.txt"),
                    found: root.join("b.txt")
                },
                DoctorStatus::EscapedTarget,
            ]
        );
    }

    #[test]
    fn load_entries_parses_link_tables() {
        let mut local = StackedConfig::empty();
        local.add_layer(
            jj_lib::config::ConfigLayer::parse(
                jj_lib::config::ConfigSource::Repo,
                r#"
                [[link]]
                source = "assets/a"
                target = "links/a"
                type = "link"

                [[link]]
                source = "~/dot/vimrc"
                target = ".vimrc"
                type = "link"
                "#,
            )
            .unwrap(),
        );

        let entries = load_entries(&local).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].source, "assets/a");
        assert_eq!(entries[1].target, ".vimrc");
    }

    #[test]
    fn load_entries_rejects_unknown_type() {
        let mut local = StackedConfig::empty();
        local.add_layer(
            jj_lib::config::ConfigLayer::parse(
                jj_lib::config::ConfigSource::Repo,
                r#"
                [[link]]
                source = "a"
                target = "b"
                type = "copy"
                "#,
            )
            .unwrap(),
        );
        assert!(load_entries(&local).is_err());
    }

    #[test]
    fn load_entries_empty_when_absent() {
        let local = StackedConfig::empty();
        assert!(load_entries(&local).unwrap().is_empty());
    }

    #[test]
    fn selectors_default_to_everything() {
        let e = entry("a", "b");
        assert!(in_scope(&e, None, "whatever"));
        assert!(in_scope(&e, Some("default"), "minisforum"));
        assert_eq!(skip_reason(&e, None, "whatever"), None);
    }

    #[test]
    fn machine_selector_compares_normalized() {
        let mut e = entry("a", "b");
        e.machines = Some(vec!["Minisforum.".to_owned(), "Laptop".to_owned()]);

        assert!(in_scope(&e, Some("ws"), "minisforum"));
        assert!(in_scope(&e, Some("ws"), "LAPTOP"));
        assert!(!in_scope(&e, Some("ws"), "tablet"));

        assert_eq!(
            skip_reason(&e, Some("ws"), "tablet").unwrap().to_string(),
            "machine \"tablet\" not in [\"Minisforum.\", \"Laptop\"]"
        );
    }

    #[test]
    fn workspace_selector_requires_known_name() {
        let mut e = entry("a", "b");
        e.workspaces = Some(vec!["feat-x".to_owned(), "default".to_owned()]);

        assert!(in_scope(&e, Some("feat-x"), "m"));
        assert!(!in_scope(&e, Some("feat-y"), "m")); // case-sensitive identifiers
        assert!(!in_scope(&e, None, "m")); // undeterminable -> scoped entries skip

        assert_eq!(
            skip_reason(&e, None, "m").unwrap().to_string(),
            "workspace undeterminable, entry scoped to [\"feat-x\", \"default\"]"
        );
    }

    #[test]
    fn load_entries_parses_selectors() {
        let mut local = StackedConfig::empty();
        local.add_layer(
            jj_lib::config::ConfigLayer::parse(
                jj_lib::config::ConfigSource::Repo,
                r#"
                [[link]]
                source = "assets/a"
                target = "links/a"
                type = "link"
                workspaces = ["feat-x"]
                machines = ["minisforum"]
                "#,
            )
            .unwrap(),
        );

        let entries = load_entries(&local).unwrap();
        assert_eq!(
            entries[0].workspaces.as_deref(),
            Some(&["feat-x".to_owned()][..])
        );
        assert_eq!(
            entries[0].machines.as_deref(),
            Some(&["minisforum".to_owned()][..])
        );
    }
}
