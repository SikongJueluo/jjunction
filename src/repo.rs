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
use std::io::IsTerminal as _;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;

use gix::object::Kind as ObjectKind;
use indicatif::ProgressBar;
use indicatif::ProgressStyle;
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
    progress: bool,
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
        run_git_progress(
            "clone",
            Some(root),
            &["clone", "--no-checkout", &entry.url, &target_str],
            &name,
            progress,
        )?;
    }

    if refresh {
        run_git_progress(
            "fetch",
            Some(&target),
            &["fetch", "--prune", "--tags", "origin"],
            &name,
            progress,
        )?;
        // Keep the floating-rev anchor in sync with the remote default.
        let _ = run_git(
            "set-head",
            Some(&target),
            &["remote", "set-head", "origin", "--auto"],
        );
    }

    let desired = if refresh {
        resolve(&target, entry.rev.as_deref(), &entry.url, progress)?
    } else {
        match lock.commit(&name) {
            Some(commit) => commit.to_owned(),
            None => resolve(&target, entry.rev.as_deref(), &entry.url, progress)?,
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
        checkout(&target, &desired, progress)?;
        Ok(ApplyStatus::CheckedOut(desired))
    } else {
        checkout(&target, &desired, progress)?;
        Ok(ApplyStatus::Cloned(desired))
    }
}

/// Checks out `commit` detached, fetching first when the object is missing
/// locally (e.g. a teammate advanced the lock, or the server requires a
/// direct SHA fetch).
fn checkout(target: &Path, commit: &str, progress: bool) -> Result<(), RepoError> {
    let args = ["checkout", "--detach", "--quiet", commit];
    if run_git("checkout", Some(target), &args).is_ok() {
        return Ok(());
    }
    let _ = run_git_progress(
        "fetch-sha",
        Some(target),
        &["fetch", "--no-tags", "origin", commit],
        "fetch",
        progress,
    );
    run_git_progress(
        "fetch",
        Some(target),
        &["fetch", "--prune", "--tags", "origin"],
        "fetch",
        progress,
    )?;
    run_git("checkout", Some(target), &args)?;
    Ok(())
}

/// Resolves `rev` to a commit id, fetching when the local refs don't have
/// it. `None` means floating: follow the remote's default branch.
fn resolve(
    target: &Path,
    rev: Option<&str>,
    url: &str,
    progress: bool,
) -> Result<String, RepoError> {
    if let Some(commit) = resolve_local(target, rev)? {
        return Ok(commit);
    }
    // Network fallback: try the revision directly, then a full fetch.
    let spec = rev.unwrap_or("HEAD");
    let _ = run_git_progress(
        "fetch-rev",
        Some(target),
        &["fetch", "--no-tags", "origin", spec],
        "fetch",
        progress,
    );
    run_git_progress(
        "fetch",
        Some(target),
        &["fetch", "--prune", "--tags", "origin"],
        "fetch",
        progress,
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

/// Whether progress bars can be drawn (stderr is a terminal).
fn progress_supported() -> bool {
    std::io::stderr().is_terminal()
}

/// One parsed git progress line: `Phase:  N% (pos/len), extra...`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GitProgress {
    phase: String,
    pos: u64,
    len: u64,
    detail: String,
}

/// Parses a git `--progress` line (optionally `remote:`-prefixed); `None` for
/// plain informational lines.
fn parse_progress_line(line: &str) -> Option<GitProgress> {
    let line = line.trim_start().strip_prefix("remote: ").unwrap_or(line);
    let (phase, rest) = line.split_once(':')?;
    let rest = rest.trim_start();
    let pct_end = rest.find('%')?;
    let pct: u64 = rest[..pct_end].trim().parse().ok()?;
    let (pos, len) = match rest.find('(').zip(rest.find(')')) {
        Some((open, close)) if open < close => {
            let (a, b) = rest[open + 1..close].split_once('/')?;
            (a.trim().parse().ok()?, b.trim().parse().ok()?)
        }
        _ => (pct, 100),
    };
    Some(GitProgress {
        phase: phase.to_owned(),
        pos,
        len,
        detail: rest.to_owned(),
    })
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.bold} {bar:24.cyan/blue} {pos}/{len} {msg}")
        .expect("valid template")
        .progress_chars("=>-")
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.bold} {spinner} {msg}").expect("valid template")
}

/// The part of a git progress detail after the percentage, e.g.
/// `", 12.30 MiB | 5.67 MiB/s"` (rate info for the bar message).
fn tail_after_percent(detail: &str) -> String {
    match detail.find(',') {
        Some(comma) => detail[comma..].to_owned(),
        None => String::new(),
    }
}

/// Mirrors one parsed progress update onto `pb`.
fn show_progress(pb: &ProgressBar, bar_mode: &mut bool, p: GitProgress) {
    if !*bar_mode {
        pb.set_style(bar_style());
        *bar_mode = true;
    }
    pb.set_length(p.len);
    pb.set_position(p.pos.min(p.len));
    pb.set_message(format!("{}{}", p.phase, tail_after_percent(&p.detail)));
}

/// Mirrors one informational line onto `pb`.
fn show_message(pb: &ProgressBar, bar_mode: &mut bool, line: &str) {
    if *bar_mode {
        pb.set_style(spinner_style());
        pb.set_length(0);
        pb.set_position(0);
        *bar_mode = false;
    }
    pb.set_message(line.to_owned());
}

/// Runs git for a network operation with progress: forces `--progress`, pipes
/// stderr, and mirrors git's carriage-return-updated progress lines onto a
/// progress bar prefixed with `label`. Falls back to the silent captured path
/// when `progress` is off or stderr is not a terminal (background hooks,
/// tests).
fn run_git_progress(
    what: &'static str,
    dir: Option<&Path>,
    args: &[&str],
    label: &str,
    progress: bool,
) -> Result<(), RepoError> {
    if !progress || !progress_supported() {
        run_git(what, dir, args)?;
        return Ok(());
    }

    let mut cmd = Command::new("git");
    cmd.arg("-c").arg("advice.detachedHead=false");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    cmd.arg(args[0]).arg("--progress").args(&args[1..]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|err| RepoError::Git {
        what,
        stderr: err.to_string(),
    })?;
    let stderr = child.stderr.take().expect("stderr piped");

    let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&collected);
    let prefix = label.to_owned();
    let reader = std::thread::spawn(move || {
        let pb = ProgressBar::new_spinner();
        pb.set_style(spinner_style());
        pb.set_prefix(prefix);
        pb.enable_steady_tick(std::time::Duration::from_millis(100));
        let mut bar_mode = false;

        let mut pending: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        let mut stderr = stderr;
        loop {
            use std::io::Read as _;
            match stderr.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    while let Some(pos) = pending.iter().position(|&b| b == b'\r' || b == b'\n') {
                        let line: Vec<u8> = pending.drain(..=pos).collect();
                        let line = String::from_utf8_lossy(&line[..line.len() - 1]);
                        let line = line.trim_end();
                        if line.is_empty() {
                            continue;
                        }
                        sink.lock().expect("progress sink").push(line.to_owned());
                        match parse_progress_line(line) {
                            Some(p) => show_progress(&pb, &mut bar_mode, p),
                            None => show_message(&pb, &mut bar_mode, line),
                        }
                    }
                }
            }
        }
        if !pending.is_empty() {
            let line = String::from_utf8_lossy(&pending);
            sink.lock()
                .expect("progress sink")
                .push(line.trim_end().to_owned());
        }
        pb.finish_and_clear();
    });

    let status = child.wait();
    let _ = reader.join();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => {
            let lines = collected.lock().expect("progress sink");
            let tail = lines
                .iter()
                .rev()
                .take(5)
                .rev()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            Err(RepoError::Git { what, stderr: tail })
        }
        Err(err) => Err(RepoError::Git {
            what,
            stderr: err.to_string(),
        }),
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

/// Health of one secondary-workspace link, as created by
/// [`link_secondary`] / reported by [`check_secondary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondaryStatus {
    /// The link was created (only from [`link_secondary`]).
    Linked,
    /// The link exists and points at the default workspace's checkout.
    AlreadyLinked,
    /// The link has not been created yet.
    MissingLink,
    /// The default workspace has not materialized this repo yet.
    DefaultMissing,
    /// Something other than the expected symlink occupies the target.
    Occupied(PathBuf),
    /// The link points somewhere else.
    WrongTarget { expected: PathBuf, found: PathBuf },
}

impl SecondaryStatus {
    /// Returns `true` when the entry needs no action.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::AlreadyLinked)
    }
}

impl fmt::Display for SecondaryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Linked => write!(f, "linked"),
            Self::AlreadyLinked => write!(f, "already linked"),
            Self::MissingLink => write!(f, "missing link"),
            Self::DefaultMissing => write!(f, "default workspace has not materialized this repo"),
            Self::Occupied(path) => {
                write!(f, "occupied by a real path: {}", path.display())
            }
            Self::WrongTarget { expected, found } => write!(
                f,
                "points to {} instead of {}",
                found.display(),
                expected.display()
            ),
        }
    }
}

/// Links every entry's target in `secondary_root` at the default
/// workspace's materialized checkout.
pub fn link_secondary(
    default_root: &Path,
    secondary_root: &Path,
    entries: &[RepoEntry],
    force: bool,
) -> Vec<(String, SecondaryStatus)> {
    entries
        .iter()
        .map(|entry| {
            let status = secondary_link_one(default_root, secondary_root, entry, force, true);
            (entry.effective_name(), status)
        })
        .collect()
}

/// Reports what [`link_secondary`] would do, without modifying anything.
pub fn check_secondary(
    default_root: &Path,
    secondary_root: &Path,
    entries: &[RepoEntry],
) -> Vec<(String, SecondaryStatus)> {
    entries
        .iter()
        .map(|entry| {
            let status = secondary_link_one(default_root, secondary_root, entry, false, false);
            (entry.effective_name(), status)
        })
        .collect()
}

/// One secondary target: symlink `secondary_root/<target>` →
/// `default_root/<target>` (absolute; `default_root` is canonicalized by the
/// caller). With `create` disabled it only reports.
fn secondary_link_one(
    default_root: &Path,
    secondary_root: &Path,
    entry: &RepoEntry,
    force: bool,
    create: bool,
) -> SecondaryStatus {
    let source = default_root.join(entry.effective_target());
    let link_path = secondary_root.join(entry.effective_target());

    if !source.join(".git").symlink_metadata().is_ok() {
        return SecondaryStatus::DefaultMissing;
    }
    match fs::symlink_metadata(&link_path) {
        Ok(meta) if meta.file_type().is_symlink() => match fs::read_link(&link_path) {
            Ok(found) if found == source => SecondaryStatus::AlreadyLinked,
            Ok(found) if force => {
                if !create {
                    return SecondaryStatus::WrongTarget {
                        expected: source,
                        found,
                    };
                }
                let _ = fs::remove_file(&link_path);
                match symlink(&source, &link_path) {
                    Ok(()) => SecondaryStatus::Linked,
                    Err(_) => SecondaryStatus::WrongTarget {
                        expected: source,
                        found,
                    },
                }
            }
            Ok(found) => SecondaryStatus::WrongTarget {
                expected: source,
                found,
            },
            Err(_) => SecondaryStatus::MissingLink,
        },
        Ok(_) => SecondaryStatus::Occupied(link_path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if !create {
                return SecondaryStatus::MissingLink;
            }
            if let Some(parent) = link_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match symlink(&source, &link_path) {
                Ok(()) => SecondaryStatus::Linked,
                Err(_) => SecondaryStatus::MissingLink,
            }
        }
        Err(_) => SecondaryStatus::MissingLink,
    }
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
mod tests;
