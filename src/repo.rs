//! Managed sub-repos: `[[repo]]` entries in the local (untrusted)
//! configuration, materialized as **readonly** plain git checkouts with a
//! detached HEAD at the locked commit. See `docs/design/subrepo.md`.
//!
//! Split of responsibilities (D5): local state is read in-process through
//! [`gix`] (already in the dependency tree via jj-lib); everything that
//! moves data — clone, fetch, checkout — shells out to `git`, which honors
//! the user's gitconfig (proxies, mirrors, credentials). This mirrors how
//! jj-lib itself divides the work.
//!
//! State model (D1): a managed repo is in the desired state iff
//! `HEAD == lock.commit`. Dirty working copies are never touched: `apply`
//! skips them with a status, `doctor` reports them.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use gix::object::Kind as ObjectKind;
use jj_lib::config::ConfigGetError;
use jj_lib::config::ConfigGetResultExt as _;
use jj_lib::config::StackedConfig;
use serde::Deserialize;
use toml_edit::ArrayOfTables;
use toml_edit::DocumentMut;
use toml_edit::Item;
use toml_edit::Table;
use toml_edit::value;

use crate::link::target_within_root;
use crate::lock::LockError;
use crate::lock::RepoLock;

/// One `[[repo]]` entry. `name` defaults to the url basename minus `.git`;
/// `target` defaults to `name` and is relative to the workspace root; `rev`
/// defaults to floating (follows the remote's default branch).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    /// Explicit entry name; defaults to the url basename.
    pub name: Option<String>,
    /// Clone url.
    pub url: String,
    /// Materialization path, relative to the workspace root.
    pub target: Option<String>,
    /// Branch, tag, or full commit id; absent = follow the default branch.
    pub rev: Option<String>,
}

impl RepoEntry {
    /// The entry name used as the lock key.
    pub fn effective_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| default_name(&self.url))
    }

    /// The path the repo materializes at, relative to the workspace root.
    pub fn effective_target(&self) -> String {
        self.target.clone().unwrap_or_else(|| self.effective_name())
    }
}

/// Derives an entry name from a url: last path segment, minus `.git`.
fn default_name(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git")
        .to_owned()
}

/// Loads `[[repo]]` entries from the local configuration stack. Missing
/// entries yield an empty list.
pub fn load_entries(local: &StackedConfig) -> Result<Vec<RepoEntry>, ConfigGetError> {
    Ok(local
        .get::<Vec<RepoEntry>>("repo")
        .optional()?
        .unwrap_or_default())
}

/// Rejects duplicate entry names or targets.
pub fn validate(entries: &[RepoEntry]) -> Result<(), RepoError> {
    let mut names = HashSet::new();
    let mut targets = HashSet::new();
    for entry in entries {
        let name = entry.effective_name();
        if !names.insert(name.clone()) {
            return Err(RepoError::Duplicate {
                kind: "name",
                value: name,
            });
        }
        let target = entry.effective_target();
        if !targets.insert(target.clone()) {
            return Err(RepoError::Duplicate {
                kind: "target",
                value: target,
            });
        }
    }
    Ok(())
}

/// Outcome of materializing one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyStatus {
    /// The repo was cloned and checked out at the commit.
    Cloned(String),
    /// An existing repo was moved to the commit.
    CheckedOut(String),
    /// The repo is already at the commit; nothing was done.
    AlreadyAt(String),
    /// The repo is dirty, so the checkout was skipped (readonly contract).
    SkippedDirty { desired: String },
}

impl fmt::Display for ApplyStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cloned(commit) => write!(f, "cloned at {commit}"),
            Self::CheckedOut(commit) => write!(f, "checked out {commit}"),
            Self::AlreadyAt(commit) => write!(f, "already at {commit}"),
            Self::SkippedDirty { desired } => {
                write!(f, "skipped: dirty working copy (wanted {desired})")
            }
        }
    }
}

/// Errors while materializing or inspecting one entry.
#[derive(Debug)]
pub enum RepoError {
    /// The target path escapes the workspace root.
    EscapedTarget(String),
    /// The target exists but is not a git repository.
    Occupied(PathBuf),
    /// Duplicate entry name or target.
    Duplicate { kind: &'static str, value: String },
    /// A `git` subprocess failed.
    Git { what: &'static str, stderr: String },
    /// A revision could not be resolved, even after fetching.
    Resolve(String),
    /// Lock file error.
    Lock(LockError),
    /// Filesystem error.
    Io(io::Error),
}

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EscapedTarget(target) => write!(f, "target escapes workspace root: {target}"),
            Self::Occupied(path) => write!(
                f,
                "target exists and is not a git repository: {} \
                 (target names the repository itself, not its parent; \
                 only empty directories are cloned into)",
                path.display()
            ),
            Self::Duplicate { kind, value } => write!(f, "duplicate repo {kind}: {value}"),
            Self::Git { what, stderr } => write!(f, "git {what} failed: {stderr}"),
            Self::Resolve(message) => write!(f, "{message}"),
            Self::Lock(err) => write!(f, "lock: {err}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for RepoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Lock(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for RepoError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<LockError> for RepoError {
    fn from(err: LockError) -> Self {
        Self::Lock(err)
    }
}

/// Materializes one entry against (and updating) the lock.
///
/// With `refresh`, revisions are re-resolved from the remote (fetch first);
/// without, the locked commit is authoritative and the network is touched
/// only when objects are missing locally.
pub fn apply_one(
    root: &Path,
    entry: &RepoEntry,
    lock: &mut RepoLock,
    refresh: bool,
) -> Result<ApplyStatus, RepoError> {
    let name = entry.effective_name();
    let target_str = entry.effective_target();
    if !target_within_root(&target_str) {
        return Err(RepoError::EscapedTarget(target_str));
    }
    let target = root.join(&target_str);

    let existed = target.join(".git").symlink_metadata().is_ok();
    if !existed {
        match fs::symlink_metadata(&target) {
            // Only an existing *empty* directory may be cloned into, matching
            // `git clone` semantics; anything else is occupied.
            Ok(meta) if meta.is_dir() => {
                let empty = fs::read_dir(&target)
                    .map(|mut entries| entries.next().is_none())
                    .unwrap_or(false);
                if !empty {
                    return Err(RepoError::Occupied(target));
                }
            }
            Ok(_) => return Err(RepoError::Occupied(target)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        run_git(
            "clone",
            Some(root),
            &["clone", "--no-checkout", &entry.url, &target_str],
        )?;
    }

    if refresh {
        run_git(
            "fetch",
            Some(&target),
            &["fetch", "--prune", "--tags", "origin"],
        )?;
        // Keep the floating-rev anchor in sync with the remote default.
        let _ = run_git(
            "set-head",
            Some(&target),
            &["remote", "set-head", "origin", "--auto"],
        );
    }

    let desired = if refresh {
        resolve(&target, entry.rev.as_deref(), &entry.url)?
    } else {
        match lock.commit(&name) {
            Some(commit) => commit.to_owned(),
            None => resolve(&target, entry.rev.as_deref(), &entry.url)?,
        }
    };
    if lock.commit(&name) != Some(desired.as_str()) {
        lock.set_commit(&name, &desired);
    }

    if existed {
        if head_commit(&target)? == Some(desired.clone()) {
            return Ok(ApplyStatus::AlreadyAt(desired));
        }
        if is_dirty(&target)? {
            return Ok(ApplyStatus::SkippedDirty { desired });
        }
        checkout(&target, &desired)?;
        Ok(ApplyStatus::CheckedOut(desired))
    } else {
        checkout(&target, &desired)?;
        Ok(ApplyStatus::Cloned(desired))
    }
}

/// Checks out `commit` detached, fetching first when the object is missing
/// locally (e.g. a teammate advanced the lock, or the server requires a
/// direct SHA fetch).
fn checkout(target: &Path, commit: &str) -> Result<(), RepoError> {
    let args = ["checkout", "--detach", "--quiet", commit];
    if run_git("checkout", Some(target), &args).is_ok() {
        return Ok(());
    }
    let _ = run_git(
        "fetch-sha",
        Some(target),
        &["fetch", "--no-tags", "origin", commit],
    );
    run_git(
        "fetch",
        Some(target),
        &["fetch", "--prune", "--tags", "origin"],
    )?;
    run_git("checkout", Some(target), &args)?;
    Ok(())
}

/// Resolves `rev` to a commit id, fetching when the local refs don't have
/// it. `None` means floating: follow the remote's default branch.
fn resolve(target: &Path, rev: Option<&str>, url: &str) -> Result<String, RepoError> {
    if let Some(commit) = resolve_local(target, rev)? {
        return Ok(commit);
    }
    // Network fallback: try the revision directly, then a full fetch.
    let spec = rev.unwrap_or("HEAD");
    let _ = run_git(
        "fetch-rev",
        Some(target),
        &["fetch", "--no-tags", "origin", spec],
    );
    run_git(
        "fetch",
        Some(target),
        &["fetch", "--prune", "--tags", "origin"],
    )?;
    let _ = run_git(
        "set-head",
        Some(target),
        &["remote", "set-head", "origin", "--auto"],
    );
    resolve_local(target, rev)?.ok_or_else(|| {
        RepoError::Resolve(match rev {
            Some(rev) => format!(
                "revision {rev:?} of {url} not found after fetch; \
                 expected a branch, tag, or full commit id"
            ),
            None => format!("no default branch found for {url}"),
        })
    })
}

/// Resolves `rev` against local refs only: full commit id, remote branch,
/// annotated tag (peeled), or the floating `origin/HEAD`.
fn resolve_local(target: &Path, rev: Option<&str>) -> Result<Option<String>, RepoError> {
    let Ok(repo) = gix::open(target) else {
        return Ok(None);
    };
    if let Some(rev) = rev {
        if let Ok(oid) = gix::ObjectId::from_hex(rev.as_bytes()) {
            return Ok(peel_to_commit(&repo, oid));
        }
        for name in [
            format!("refs/remotes/origin/{rev}"),
            format!("refs/tags/{rev}"),
        ] {
            if let Ok(mut reference) = repo.find_reference(&name)
                && let Ok(oid) = reference.peel_to_id()
                && let Some(commit) = peel_to_commit(&repo, oid.detach())
            {
                return Ok(Some(commit));
            }
        }
    } else if let Ok(mut reference) = repo.find_reference("refs/remotes/origin/HEAD")
        && let Ok(oid) = reference.peel_to_id()
        && let Some(commit) = peel_to_commit(&repo, oid.detach())
    {
        return Ok(Some(commit));
    }
    Ok(None)
}

/// Verifies the object exists locally and peels tags to the commit id.
fn peel_to_commit(repo: &gix::Repository, oid: gix::ObjectId) -> Option<String> {
    let object = repo.find_object(oid).ok()?;
    let object = object.peel_tags_to_end().ok()?;
    (object.kind == ObjectKind::Commit).then(|| object.id.to_string())
}

/// The commit id HEAD points at, or `None` for an unborn HEAD.
fn head_commit(target: &Path) -> Result<Option<String>, RepoError> {
    let repo = match gix::open(target) {
        Ok(repo) => repo,
        Err(_) => return Ok(None),
    };
    match repo.head_id() {
        Ok(id) => Ok(Some(id.to_string())),
        Err(_) => Ok(None), // unborn HEAD
    }
}

/// Whether the working tree has worktree-side changes (modifications,
/// untracked files). Staged-only deletions are ignored so an interrupted
/// first materialization (`clone --no-checkout`) can still complete.
fn is_dirty(target: &Path) -> Result<bool, RepoError> {
    let out = run_git("status", Some(target), &["status", "--porcelain"])?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.as_bytes().get(1).is_some_and(|&c| c != b' ')))
}

/// Runs `git` capturing output; non-zero exits become [`RepoError::Git`]
/// with the tail of stderr.
fn run_git(what: &'static str, dir: Option<&Path>, args: &[&str]) -> Result<Output, RepoError> {
    let mut cmd = Command::new("git");
    cmd.arg("-c").arg("advice.detachedHead=false");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);
    let out = cmd.output().map_err(|err| RepoError::Git {
        what,
        stderr: err.to_string(),
    })?;
    if out.status.success() {
        Ok(out)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr.chars().rev().take(300).collect();
        let tail: String = tail.chars().rev().collect();
        Err(RepoError::Git { what, stderr: tail })
    }
}

/// Health of one entry as reported by [`doctor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoctorStatus {
    /// Materialized at the locked commit, clean.
    Ok,
    /// The target does not exist.
    MissingTarget,
    /// The target exists but is not a git repository.
    Occupied,
    /// The target path escapes the workspace root.
    EscapedTarget,
    /// No lock entry for this repo.
    MissingLock,
    /// HEAD is not at the locked commit.
    Diverged { at: Option<String>, locked: String },
    /// The manifest pins a full commit id that differs from the lock.
    ManifestPinned { rev: String, locked: String },
    /// The working copy has local changes.
    Dirty,
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
            Self::MissingTarget => write!(f, "not materialized"),
            Self::Occupied => write!(f, "target is not a git repository"),
            Self::EscapedTarget => write!(f, "target escapes workspace root"),
            Self::MissingLock => write!(f, "no lock entry (jjn repo sync records one)"),
            Self::Diverged { at, locked } => match at {
                Some(at) => write!(f, "at {at}, locked {locked}"),
                None => write!(f, "unborn HEAD, locked {locked}"),
            },
            Self::ManifestPinned { rev, locked } => {
                write!(
                    f,
                    "manifest pins {rev}, lock has {locked} (jjn repo update)"
                )
            }
            Self::Dirty => write!(f, "dirty working copy (readonly repo)"),
        }
    }
}

/// One entry with its [`DoctorStatus`].
#[derive(Debug)]
pub struct Diagnosis {
    /// The checked entry.
    pub entry: RepoEntry,
    /// What was found.
    pub status: DoctorStatus,
}

/// Checks all entries without modifying anything.
pub fn doctor(root: &Path, entries: &[RepoEntry], lock: &RepoLock) -> Vec<Diagnosis> {
    entries
        .iter()
        .map(|entry| Diagnosis {
            entry: entry.clone(),
            status: doctor_one(root, entry, lock),
        })
        .collect()
}

/// Checks one entry without modifying anything.
pub fn doctor_one(root: &Path, entry: &RepoEntry, lock: &RepoLock) -> DoctorStatus {
    let target_str = entry.effective_target();
    if !target_within_root(&target_str) {
        return DoctorStatus::EscapedTarget;
    }
    let target = root.join(&target_str);
    if !target.join(".git").exists() {
        return if target.exists() {
            DoctorStatus::Occupied
        } else {
            DoctorStatus::MissingTarget
        };
    }
    let Some(locked) = lock.commit(&entry.effective_name()).map(str::to_owned) else {
        return DoctorStatus::MissingLock;
    };
    if is_dirty(&target).unwrap_or(false) {
        return DoctorStatus::Dirty;
    }
    let head = head_commit(&target).ok().flatten();
    if head.as_deref() != Some(locked.as_str()) {
        return DoctorStatus::Diverged { at: head, locked };
    }
    if let Some(rev) = entry.rev.as_deref()
        && gix::ObjectId::from_hex(rev.as_bytes()).is_ok()
        && rev != locked
    {
        return DoctorStatus::ManifestPinned {
            rev: rev.to_owned(),
            locked,
        };
    }
    DoctorStatus::Ok
}

/// One-line summary of an entry for error messages and listings.
pub fn entry_summary(entry: &RepoEntry) -> String {
    format!(
        "\"{}\" url={} target={} rev={}",
        entry.effective_name(),
        entry.url,
        entry.effective_target(),
        entry.rev.as_deref().unwrap_or("(floating)")
    )
}

/// Drops lock entries whose manifest entry is gone; returns the dropped
/// names (lock is derived state, so this is a safe garbage collection).
pub fn gc_lock(lock: &mut RepoLock, entries: &[RepoEntry]) -> Vec<String> {
    let declared: HashSet<String> = entries.iter().map(RepoEntry::effective_name).collect();
    lock.names()
        .into_iter()
        .filter(|name| !declared.contains(name))
        .filter(|name| lock.remove(name))
        .collect()
}

/// Appends a `[[repo]]` entry to the manifest, preserving comments and
/// formatting (`toml_edit` in-place edit).
pub fn append_manifest_entry(path: &Path, entry: &RepoEntry) -> Result<(), RepoError> {
    let mut doc = load_manifest(path)?;
    let aot = manifest_repo_array(&mut doc);
    let mut table = Table::new();
    if let Some(name) = &entry.name {
        table["name"] = value(name);
    }
    table["url"] = value(&entry.url);
    if let Some(target) = &entry.target {
        table["target"] = value(target);
    }
    if let Some(rev) = &entry.rev {
        table["rev"] = value(rev);
    }
    aot.push(table);
    save_manifest(path, &doc)
}

/// Removes the entry whose effective name (or target) equals `name`;
/// returns whether one was found.
pub fn remove_manifest_entry(path: &Path, name: &str) -> Result<bool, RepoError> {
    let mut doc = load_manifest(path)?;
    let aot = manifest_repo_array(&mut doc);
    let index = aot.iter().position(|table| {
        let url = table.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let effective = table
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| default_name(url));
        let target = table
            .get("target")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or(effective.clone());
        effective == name || target == name
    });
    match index {
        Some(index) => {
            aot.remove(index);
            save_manifest(path, &doc)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

fn load_manifest(path: &Path) -> Result<DocumentMut, RepoError> {
    if !path.is_file() {
        return Ok(DocumentMut::new());
    }
    let text = fs::read_to_string(path)?;
    text.parse().map_err(|source| {
        RepoError::Lock(LockError::Parse {
            path: path.to_owned(),
            source,
        })
    })
}

fn manifest_repo_array(doc: &mut DocumentMut) -> &mut ArrayOfTables {
    let item = doc
        .entry("repo")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    item.as_array_of_tables_mut()
        .expect("repo must be an array of tables")
}

fn save_manifest(path: &Path, doc: &DocumentMut) -> Result<(), RepoError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, doc.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str) -> RepoEntry {
        RepoEntry {
            name: None,
            url: url.to_owned(),
            target: None,
            rev: None,
        }
    }

    fn entry_rev(url: &str, rev: &str) -> RepoEntry {
        RepoEntry {
            rev: Some(rev.to_owned()),
            ..entry(url)
        }
    }

    #[test]
    fn default_name_strips_git_suffix() {
        assert_eq!(default_name("https://x/y/repo.git"), "repo");
        assert_eq!(default_name("https://x/y/repo"), "repo");
        assert_eq!(default_name("git@host:org/repo.git"), "repo");
        assert_eq!(default_name("https://x/y/repo/"), "repo");
    }

    #[test]
    fn effective_target_defaults_to_name() {
        let e = entry("https://x/y/repo.git");
        assert_eq!(e.effective_name(), "repo");
        assert_eq!(e.effective_target(), "repo");
    }

    #[test]
    fn validate_rejects_duplicate_names() {
        let entries = vec![entry("https://x/a.git"), entry("https://y/a.git")];
        assert!(matches!(
            validate(&entries),
            Err(RepoError::Duplicate { kind: "name", .. })
        ));
    }

    #[test]
    fn load_entries_parses_repo_tables() {
        let mut local = StackedConfig::empty();
        local.add_layer(
            jj_lib::config::ConfigLayer::parse(
                jj_lib::config::ConfigSource::Repo,
                r#"
                [[repo]]
                url = "https://x/y/habitat-sim.git"
                target = "third_party/habitat-sim"
                rev = "main"

                [[repo]]
                name = "mini"
                url = "https://x/y/mini.git"
                "#,
            )
            .unwrap(),
        );

        let entries = load_entries(&local).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].target.as_deref(),
            Some("third_party/habitat-sim")
        );
        assert_eq!(entries[1].effective_name(), "mini");
        assert_eq!(entries[1].effective_target(), "mini");
    }

    #[test]
    fn load_entries_rejects_unknown_fields() {
        let mut local = StackedConfig::empty();
        local.add_layer(
            jj_lib::config::ConfigLayer::parse(
                jj_lib::config::ConfigSource::Repo,
                r#"
                [[repo]]
                url = "https://x/y/a.git"
                brancy = "typo"
                "#,
            )
            .unwrap(),
        );
        assert!(load_entries(&local).is_err());
    }

    // --- git fixtures -------------------------------------------------

    fn git_ok(dir: Option<&Path>, args: &[&str]) {
        let mut cmd = Command::new("git");
        if let Some(dir) = dir {
            cmd.arg("-C").arg(dir);
        }
        cmd.args(args);
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit_all(dir: &Path, message: &str) {
        git_ok(
            Some(dir),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-am",
                message,
            ],
        );
    }

    fn rev_parse(dir: &Path, what: &str) -> String {
        let out = run_git("rev-parse", Some(dir), &["rev-parse", what]).unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Creates a remote at `<dir>/remote` with two commits on `main` and an
    /// annotated tag `v1` on the first; returns (c1, c2).
    fn fixture_remote(dir: &Path) -> (String, String) {
        let remote = dir.join("remote");
        fs::create_dir_all(&remote).unwrap();
        git_ok(Some(&remote), &["init", "-q", "-b", "main"]);
        fs::write(remote.join("file.txt"), "one").unwrap();
        git_ok(Some(&remote), &["add", "."]);
        commit_all(&remote, "c1");
        let c1 = rev_parse(&remote, "HEAD");
        git_ok(
            Some(&remote),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "tag",
                "-a",
                "v1",
                "-m",
                "v1",
            ],
        );
        fs::write(remote.join("file.txt"), "two").unwrap();
        commit_all(&remote, "c2");
        let c2 = rev_parse(&remote, "HEAD");
        (c1, c2)
    }

    fn fixture_workspace(remote: &Path) -> (tempfile::TempDir, PathBuf, RepoEntry, RepoLock) {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path().to_owned();
        fs::create_dir_all(root.join(".jjunction")).unwrap();
        let entry = entry(remote.to_str().unwrap());
        let lock = RepoLock::load_or_create(&root.join(".jjunction/lock.toml")).unwrap();
        (ws, root, entry, lock)
    }

    fn target_path(root: &Path, e: &RepoEntry) -> PathBuf {
        root.join(e.effective_target())
    }

    #[test]
    fn apply_clones_locks_and_checks_out() {
        let dir = tempfile::tempdir().unwrap();
        let (_c1, c2) = fixture_remote(dir.path());
        let (_ws, root, entry, mut lock) = fixture_workspace(&dir.path().join("remote"));

        let status = apply_one(&root, &entry, &mut lock, false).unwrap();
        assert_eq!(status, ApplyStatus::Cloned(c2.clone()));
        assert_eq!(lock.commit("remote"), Some(c2.as_str()));
        assert!(lock.is_changed());

        let target = target_path(&root, &entry);
        assert_eq!(rev_parse(&target, "HEAD"), c2);
        assert_eq!(fs::read_to_string(target.join("file.txt")).unwrap(), "two");
    }

    #[test]
    fn apply_is_idempotent_and_follows_lock_offline() {
        let dir = tempfile::tempdir().unwrap();
        let (c1, _c2) = fixture_remote(dir.path());
        let (_ws, root, entry, mut lock) = fixture_workspace(&dir.path().join("remote"));

        apply_one(&root, &entry, &mut lock, false).unwrap();
        assert_eq!(
            apply_one(&root, &entry, &mut lock, false).unwrap(),
            ApplyStatus::AlreadyAt(lock.commit("remote").unwrap().to_owned())
        );

        // Rewind the lock: apply must move HEAD without touching the network.
        lock.set_commit("remote", &c1);
        assert_eq!(
            apply_one(&root, &entry, &mut lock, false).unwrap(),
            ApplyStatus::CheckedOut(c1.clone())
        );
        let target = target_path(&root, &entry);
        assert_eq!(fs::read_to_string(target.join("file.txt")).unwrap(), "one");
    }

    #[test]
    fn apply_resolves_rev_forms() {
        let dir = tempfile::tempdir().unwrap();
        let (c1, c2) = fixture_remote(dir.path());
        let remote = dir.path().join("remote");

        for (rev, expected) in [("main", &c2), ("v1", &c1), (c1.as_str(), &c1)] {
            let (_ws, root, mut entry, mut lock) = fixture_workspace(&remote);
            entry.rev = Some(rev.to_owned());
            let status = apply_one(&root, &entry, &mut lock, false).unwrap();
            assert_eq!(status, ApplyStatus::Cloned(expected.clone()), "rev {rev}");
        }
    }

    #[test]
    fn apply_skips_dirty_repos() {
        let dir = tempfile::tempdir().unwrap();
        let (c1, c2) = fixture_remote(dir.path());
        let (_ws, root, entry, mut lock) = fixture_workspace(&dir.path().join("remote"));

        apply_one(&root, &entry, &mut lock, false).unwrap();
        let target = target_path(&root, &entry);
        fs::write(target.join("extra.txt"), "local change").unwrap();

        lock.set_commit("remote", &c1);
        let status = apply_one(&root, &entry, &mut lock, false).unwrap();
        assert_eq!(
            status,
            ApplyStatus::SkippedDirty {
                desired: c1.clone()
            }
        );
        assert_eq!(rev_parse(&target, "HEAD"), c2, "HEAD untouched");
        assert!(target.join("extra.txt").exists(), "worktree untouched");
    }

    #[test]
    fn apply_refuses_occupied_paths() {
        let dir = tempfile::tempdir().unwrap();
        let (_c1, _c2) = fixture_remote(dir.path());
        let (_ws, root, entry, mut lock) = fixture_workspace(&dir.path().join("remote"));

        let target = target_path(&root, &entry);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), "user data").unwrap();

        assert!(matches!(
            apply_one(&root, &entry, &mut lock, false),
            Err(RepoError::Occupied(_))
        ));
        assert!(target.join("keep.txt").exists());
    }

    #[test]
    fn apply_clones_into_an_existing_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (_c1, c2) = fixture_remote(dir.path());
        let (_ws, root, entry, mut lock) = fixture_workspace(&dir.path().join("remote"));

        // Pre-existing empty directory, like `git clone url dir` accepts.
        let target = target_path(&root, &entry);
        fs::create_dir_all(&target).unwrap();

        let status = apply_one(&root, &entry, &mut lock, false).unwrap();
        assert_eq!(status, ApplyStatus::Cloned(c2.clone()));
        assert_eq!(rev_parse(&target, "HEAD"), c2);
    }

    #[test]
    fn gc_lock_drops_orphans_only() {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote");
        let (_ws, _root, entry, mut lock) = fixture_workspace(&remote);

        lock.set_commit("ghost", &"a".repeat(40));
        lock.set_commit("remote", &"b".repeat(40));

        let dropped = gc_lock(&mut lock, std::slice::from_ref(&entry));
        assert_eq!(dropped, vec!["ghost".to_owned()]);
        assert_eq!(lock.commit("remote"), Some("b".repeat(40).as_str()));
        assert_eq!(lock.commit("ghost"), None);
    }

    #[test]
    fn refresh_follows_remote_main() {
        let dir = tempfile::tempdir().unwrap();
        let (_c1, _c2) = fixture_remote(dir.path());
        let remote = dir.path().join("remote");
        let (_ws, root, entry, mut lock) = fixture_workspace(&remote);

        apply_one(&root, &entry, &mut lock, false).unwrap();

        fs::write(remote.join("file.txt"), "three").unwrap();
        commit_all(&remote, "c3");
        let c3 = rev_parse(&remote, "HEAD");

        let status = apply_one(&root, &entry, &mut lock, true).unwrap();
        assert_eq!(status, ApplyStatus::CheckedOut(c3.clone()));
        assert_eq!(lock.commit("remote"), Some(c3.as_str()));
        let target = target_path(&root, &entry);
        assert_eq!(
            fs::read_to_string(target.join("file.txt")).unwrap(),
            "three"
        );
    }

    #[test]
    fn resolve_reports_unresolvable_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let (_c1, _c2) = fixture_remote(dir.path());
        let remote = dir.path().join("remote");
        let (_ws, root, mut entry, mut lock) = fixture_workspace(&remote);

        apply_one(&root, &entry, &mut lock, false).unwrap();
        entry.rev = Some("no-such-branch".to_owned());

        let err = apply_one(&root, &entry, &mut lock, true).unwrap_err();
        assert!(matches!(err, RepoError::Resolve(_)), "got: {err}");
    }

    #[test]
    fn doctor_reports_each_status() {
        let dir = tempfile::tempdir().unwrap();
        let (c1, c2) = fixture_remote(dir.path());
        let remote = dir.path().join("remote");
        let (_ws, root, entry, mut lock) = fixture_workspace(&remote);

        // not materialized
        let missing = doctor_one(&root, &entry, &lock);
        assert_eq!(missing, DoctorStatus::MissingTarget);

        apply_one(&root, &entry, &mut lock, false).unwrap();
        let target = target_path(&root, &entry);

        // ok
        assert_eq!(doctor_one(&root, &entry, &lock), DoctorStatus::Ok);

        // diverged
        lock.set_commit("remote", &c1);
        assert_eq!(
            doctor_one(&root, &entry, &lock),
            DoctorStatus::Diverged {
                at: Some(c2.clone()),
                locked: c1.clone()
            }
        );

        // manifest pins a different full sha
        let pinned = entry_rev(remote.to_str().unwrap(), &c1);
        lock.set_commit("remote", &c2);
        assert_eq!(
            doctor_one(&root, &pinned, &lock),
            DoctorStatus::ManifestPinned {
                rev: c1.clone(),
                locked: c2.clone()
            }
        );

        // dirty
        fs::write(target.join("local.txt"), "x").unwrap();
        assert_eq!(doctor_one(&root, &entry, &lock), DoctorStatus::Dirty);

        // missing lock entry
        lock.remove("remote");
        assert_eq!(doctor_one(&root, &entry, &lock), DoctorStatus::MissingLock);
    }

    #[test]
    fn manifest_append_preserves_comments_and_remove_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".jjunction/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "# team config\n\n[[link]]\nsource = \"a\"\ntarget = \"b\"\ntype = \"link\"\n",
        )
        .unwrap();

        append_manifest_entry(
            &path,
            &RepoEntry {
                name: None,
                url: "https://x/y/repo.git".to_owned(),
                target: Some("third_party/repo".to_owned()),
                rev: Some("main".to_owned()),
            },
        )
        .unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# team config"), "comment kept: {text}");
        assert!(text.contains("[[repo]]"));
        assert!(text.contains("target = \"third_party/repo\""));
        assert!(text.contains("[[link]]"));

        assert!(remove_manifest_entry(&path, "repo").unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("[[repo]]"));
        assert!(text.contains("[[link]]"));
        assert!(!remove_manifest_entry(&path, "repo").unwrap());
    }
}
