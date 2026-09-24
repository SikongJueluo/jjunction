//! Tempdir tests for cross-workspace file sync and the direnv allow gate.

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
