//! Git-fixture integration tests: real `git` subprocesses against local-path remotes.

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
fn parses_git_progress_lines() {
    let p =
        parse_progress_line("Receiving objects:  45% (900/2000), 12.30 MiB | 5.67 MiB/s").unwrap();
    assert_eq!(p.phase, "Receiving objects");
    assert_eq!((p.pos, p.len), (900, 2000));
    assert!(p.detail.contains("5.67 MiB/s"));

    // remote-prefixed, done-suffixed
    let p = parse_progress_line("remote: Counting objects: 100% (10/10), done.").unwrap();
    assert_eq!(p.phase, "Counting objects");
    assert_eq!((p.pos, p.len), (10, 10));

    // percent without counts
    let p = parse_progress_line("Resolving deltas:  75%").unwrap();
    assert_eq!((p.pos, p.len), (75, 100));

    // informational lines are not progress
    assert!(parse_progress_line("Cloning into 'deps/NavDP'...").is_none());
    assert!(parse_progress_line("").is_none());
}

#[test]
fn apply_clones_locks_and_checks_out() {
    let dir = tempfile::tempdir().unwrap();
    let (_c1, c2) = fixture_remote(dir.path());
    let (_ws, root, entry, mut lock) = fixture_workspace(&dir.path().join("remote"));

    let status = apply_one(&root, &entry, &mut lock, false, false).unwrap();
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

    apply_one(&root, &entry, &mut lock, false, false).unwrap();
    assert_eq!(
        apply_one(&root, &entry, &mut lock, false, false).unwrap(),
        ApplyStatus::AlreadyAt(lock.commit("remote").unwrap().to_owned())
    );

    // Rewind the lock: apply must move HEAD without touching the network.
    lock.set_commit("remote", &c1);
    assert_eq!(
        apply_one(&root, &entry, &mut lock, false, false).unwrap(),
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
        let status = apply_one(&root, &entry, &mut lock, false, false).unwrap();
        assert_eq!(status, ApplyStatus::Cloned(expected.clone()), "rev {rev}");
    }
}

#[test]
fn apply_skips_dirty_repos() {
    let dir = tempfile::tempdir().unwrap();
    let (c1, c2) = fixture_remote(dir.path());
    let (_ws, root, entry, mut lock) = fixture_workspace(&dir.path().join("remote"));

    apply_one(&root, &entry, &mut lock, false, false).unwrap();
    let target = target_path(&root, &entry);
    fs::write(target.join("extra.txt"), "local change").unwrap();

    lock.set_commit("remote", &c1);
    let status = apply_one(&root, &entry, &mut lock, false, false).unwrap();
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
        apply_one(&root, &entry, &mut lock, false, false),
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

    let status = apply_one(&root, &entry, &mut lock, false, false).unwrap();
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

    apply_one(&root, &entry, &mut lock, false, false).unwrap();

    fs::write(remote.join("file.txt"), "three").unwrap();
    commit_all(&remote, "c3");
    let c3 = rev_parse(&remote, "HEAD");

    let status = apply_one(&root, &entry, &mut lock, true, false).unwrap();
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

    apply_one(&root, &entry, &mut lock, false, false).unwrap();
    entry.rev = Some("no-such-branch".to_owned());

    let err = apply_one(&root, &entry, &mut lock, true, false).unwrap_err();
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

    apply_one(&root, &entry, &mut lock, false, false).unwrap();
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
