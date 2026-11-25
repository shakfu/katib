//! Two clones of one bare repository in a temporary directory, one per
//! simulated machine (design.md section 9). Git runs with the user's global
//! and system config disabled, so signing or hooks there cannot interfere.

use std::{fs, path::PathBuf, process::Command};

use super::*;

struct Fixture {
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn env() -> Vec<(String, String)> {
    [
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_AUTHOR_NAME", "test"),
        ("GIT_AUTHOR_EMAIL", "test@example.invalid"),
        ("GIT_COMMITTER_NAME", "test"),
        ("GIT_COMMITTER_EMAIL", "test@example.invalid"),
    ]
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .into()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .envs(env())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

impl Fixture {
    /// A bare remote with one commit holding `files`, and clones `a` and `b`.
    fn new(name: &str, files: &[(&str, &str)]) -> (Self, Repo, Repo) {
        let dir = std::env::temp_dir().join(format!("katib-sync-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        git(
            &dir,
            &["init", "--quiet", "--bare", "-b", "main", "remote.git"],
        );
        git(&dir, &["clone", "--quiet", "remote.git", "a"]);
        let a = Repo::open_with_env(&dir.join("a"), env()).unwrap().unwrap();
        for (path, text) in files {
            write(&a, path, text);
        }
        a.commit_all("seed").unwrap();
        git(&dir.join("a"), &["push", "--quiet", "-u", "origin", "main"]);
        git(&dir, &["clone", "--quiet", "remote.git", "b"]);
        let b = Repo::open_with_env(&dir.join("b"), env()).unwrap().unwrap();
        (Self { dir }, a, b)
    }
}

fn write(repo: &Repo, path: &str, text: &str) {
    let p = repo.root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn read(repo: &Repo, path: &str) -> Option<String> {
    fs::read_to_string(repo.root.join(path)).ok()
}

#[test]
fn edits_to_different_files_merge_both_ways() {
    let (_f, a, b) = Fixture::new("clean", &[("a.md", "a\n"), ("b.md", "b\n")]);
    write(&a, "a.md", "a from A\n");
    write(&b, "b.md", "b from B\n");
    assert_eq!(a.sync(true, "A").unwrap().outcome, Outcome::Synced);
    let report = b.sync(true, "B").unwrap();
    assert_eq!(
        (report.committed, report.outcome, report.ahead),
        (true, Outcome::Synced, 0)
    );
    assert_eq!(read(&b, "a.md").as_deref(), Some("a from A\n"));
    assert_eq!(a.sync(true, "A").unwrap().outcome, Outcome::Synced);
    assert_eq!(read(&a, "b.md").as_deref(), Some("b from B\n"));
    assert!(!a.has_changes().unwrap());
}

#[test]
fn a_same_line_edit_conflicts_without_touching_the_work_tree() {
    let (_f, a, b) = Fixture::new("conflict", &[("dir/x.md", "line\n")]);
    write(&a, "dir/x.md", "from A\n");
    write(&b, "dir/x.md", "from B\n");
    a.sync(true, "A").unwrap();
    let report = b.sync(true, "B").unwrap();
    let Outcome::Conflicted(conflicts) = report.outcome else {
        panic!("expected a conflict, got {report:?}");
    };
    assert_eq!(conflicts.paths, ["dir/x.md"]);
    assert_eq!(read(&b, "dir/x.md").as_deref(), Some("from B\n"));
    assert!(!b.has_changes().unwrap());
    assert_eq!(
        b.versions(&conflicts.theirs, "dir/x.md").unwrap(),
        Versions {
            base: Some("line\n".into()),
            mine: Some("from B\n".into()),
            theirs: Some("from A\n".into()),
        }
    );

    let both = [("dir/x.md".to_owned(), Choice::Both)];
    b.resolve(&conflicts.theirs, &both, "B").unwrap();
    assert_eq!(read(&b, "dir/x.md").as_deref(), Some("from B\n"));
    assert_eq!(
        read(&b, "dir/x (conflict B).md").as_deref(),
        Some("from A\n")
    );
    assert_eq!(b.sync(true, "B").unwrap().outcome, Outcome::Synced);
    a.sync(true, "A").unwrap();
    assert_eq!(
        read(&a, "dir/x (conflict B).md").as_deref(),
        Some("from A\n")
    );
}

#[test]
fn edited_here_deleted_there_can_keep_the_edit() {
    let (_f, a, b) = Fixture::new("delete", &[("d.md", "keep\n")]);
    fs::remove_file(a.root.join("d.md")).unwrap();
    write(&b, "d.md", "edited\n");
    a.sync(true, "A").unwrap();
    let Outcome::Conflicted(c) = b.sync(true, "B").unwrap().outcome else {
        panic!("expected a conflict");
    };
    assert_eq!(c.paths, ["d.md"]);
    assert_eq!(b.versions(&c.theirs, "d.md").unwrap().theirs, None);
    let missing = b.resolve(&c.theirs, &[], "B");
    assert!(matches!(missing, Err(SyncError::Unresolved(p)) if p == ["d.md"]));
    b.resolve(&c.theirs, &[("d.md".into(), Choice::Mine)], "B")
        .unwrap();
    assert_eq!(read(&b, "d.md").as_deref(), Some("edited\n"));
    assert_eq!(b.sync(true, "B").unwrap().outcome, Outcome::Synced);
}

#[test]
fn an_unreachable_remote_keeps_commits_local() {
    let (f, a, _b) = Fixture::new("offline", &[("a.md", "a\n")]);
    git(
        &a.root,
        &[
            "remote",
            "set-url",
            "origin",
            &f.dir.join("gone.git").to_string_lossy(),
        ],
    );
    write(&a, "a.md", "changed\n");
    let report = a.sync(true, "A").unwrap();
    assert!(report.committed);
    assert!(matches!(report.outcome, Outcome::Offline(_)), "{report:?}");
    assert_eq!(report.ahead, 1);
}

#[test]
fn no_upstream_commits_only() {
    let (_f, a, _b) = Fixture::new("noremote", &[("a.md", "a\n")]);
    git(&a.root, &["branch", "--unset-upstream"]);
    write(&a, "a.md", "changed\n");
    let report = a.sync(true, "A").unwrap();
    assert_eq!(
        (report.committed, report.outcome),
        (true, Outcome::NoRemote)
    );
}

#[test]
fn temp_files_and_subdirectories() {
    let (_f, a, _b) = Fixture::new("open", &[("a.md", "a\n")]);
    write(&a, ".a.md.katib-tmp", "half");
    assert!(!a.has_changes().unwrap());
    fs::create_dir_all(a.root.join("sub")).unwrap();
    assert!(
        Repo::open_with_env(&a.root.join("sub"), env())
            .unwrap()
            .is_none()
    );
    let outside = std::env::temp_dir().join(format!("katib-sync-none-{}", std::process::id()));
    fs::create_dir_all(&outside).unwrap();
    // A temp directory is not inside a repository on any normal setup.
    assert!(Repo::open_with_env(&outside, env()).unwrap().is_none());
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn names_and_times() {
    assert_eq!(conflict_name("dir/x.md", "h"), "dir/x (conflict h).md");
    assert_eq!(conflict_name(".hidden", "h"), ".hidden (conflict h)");
    assert_eq!(conflict_name("README", "h"), "README (conflict h)");
    let t = UNIX_EPOCH + std::time::Duration::from_secs(1_795_000_000);
    assert_eq!(iso8601(t), "2026-11-18T11:06:40Z");
    assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
}

fn lfs_installed() -> bool {
    let ok = Command::new("git")
        .args(["lfs", "version"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipped: git-lfs is not installed");
    }
    ok
}

const ATTRIBUTES: &str = "attachments/** filter=lfs diff=lfs merge=lfs -text\n";

fn is_pointer(text: &str) -> bool {
    text.starts_with("version https://git-lfs.github.com/spec/v1")
}

#[test]
fn lfs_commits_attachments_as_pointers() {
    if !lfs_installed() {
        return;
    }
    let (_f, mut a, _b) = Fixture::new("lfs", &[("a.md", "a\n")]);
    a.enable_lfs().unwrap();
    write(&a, ".gitattributes", ATTRIBUTES);
    write(&a, "attachments/ab/abc-x.png", "binary bytes");
    assert!(a.commit_all("A").unwrap());
    let blob = git(&a.root, &["show", "HEAD:attachments/ab/abc-x.png"]);
    assert!(is_pointer(&blob), "{blob}");
    // The work tree keeps the real bytes.
    assert_eq!(
        read(&a, "attachments/ab/abc-x.png").as_deref(),
        Some("binary bytes")
    );
}

#[test]
fn lfs_objects_reach_the_other_clone() {
    if !lfs_installed() {
        return;
    }
    let (f, mut a, mut b) = Fixture::new("lfs-sync", &[("a.md", "a\n")]);
    a.enable_lfs().unwrap();
    b.enable_lfs().unwrap();
    write(&a, ".gitattributes", ATTRIBUTES);
    write(&a, "attachments/ab/abc-x.png", "binary bytes");
    let report = a.sync(true, "A").unwrap();
    assert_eq!((report.outcome, report.lfs_error), (Outcome::Synced, None));
    let report = b.sync(true, "B").unwrap();
    assert_eq!((report.outcome, report.lfs_error), (Outcome::Synced, None));
    assert_eq!(
        read(&b, "attachments/ab/abc-x.png").as_deref(),
        Some("binary bytes")
    );
    assert!(!b.has_changes().unwrap());

    // The LFS server loses the next object: the note still syncs, the
    // attachment stays a pointer, and the next cycle downloads it.
    write(&a, "attachments/cd/cde-y.png", "more bytes");
    write(&a, "a.md", "a, edited\n");
    a.sync(true, "A").unwrap();
    let objects = f.dir.join("remote.git/lfs");
    let hidden = f.dir.join("lfs-hidden");
    fs::rename(&objects, &hidden).unwrap();
    let report = b.sync(true, "B").unwrap();
    assert_eq!(report.outcome, Outcome::Synced);
    assert!(report.lfs_error.is_some(), "{report:?}");
    assert_eq!(read(&b, "a.md").as_deref(), Some("a, edited\n"));
    assert!(is_pointer(&read(&b, "attachments/cd/cde-y.png").unwrap()));
    assert!(!b.has_changes().unwrap());
    // The failed download may have recreated the directory.
    let _ = fs::remove_dir_all(&objects);
    fs::rename(&hidden, &objects).unwrap();
    let report = b.sync(true, "B").unwrap();
    assert_eq!(report.lfs_error, None);
    assert_eq!(
        read(&b, "attachments/cd/cde-y.png").as_deref(),
        Some("more bytes")
    );
    assert!(!b.has_changes().unwrap());
}

#[test]
fn enabling_lfs_moves_plain_attachments_to_lfs() {
    if !lfs_installed() {
        return;
    }
    let (_f, mut a, _b) = Fixture::new(
        "lfs-switch",
        &[("a.md", "a\n"), ("attachments/ab/abc-x.png", "old bytes")],
    );
    let blob = git(&a.root, &["show", "HEAD:attachments/ab/abc-x.png"]);
    assert_eq!(blob, "old bytes");
    write(&a, ".gitattributes", ATTRIBUTES);
    a.enable_lfs().unwrap();
    assert!(a.commit_all("A").unwrap());
    let blob = git(&a.root, &["show", "HEAD:attachments/ab/abc-x.png"]);
    assert!(is_pointer(&blob), "{blob}");
    assert!(!a.has_changes().unwrap());
}

#[test]
fn last_commits_name_the_newest_author_per_file() {
    let (_f, a, _b) = Fixture::new("authors", &[("a.md", "a\n"), ("dir/b c.md", "b\n")]);
    write(&a, "a.md", "edited\n");
    let mut env = env();
    env.retain(|(k, _)| !k.starts_with("GIT_AUTHOR_NAME"));
    env.push(("GIT_AUTHOR_NAME".into(), "second".into()));
    let second = Repo::open_with_env(&a.root, env.clone()).unwrap().unwrap();
    second.commit_all("A").unwrap();
    let got = last_commits_with_env(&a.root, 50, &env);
    assert_eq!(got["a.md"].author, "second");
    assert_eq!(got["dir/b c.md"].author, "test");
    assert!(got["a.md"].time > 0);
    let sub = a.root.join("dir");
    assert!(last_commits_with_env(&sub, 50, &env).is_empty());
}

#[test]
fn a_push_rejected_by_a_moved_remote_retries_and_then_merges() {
    let (f, a, b) = Fixture::new("rejected", &[("a.md", "a\n"), ("b.md", "b\n")]);
    // A fetches from a copy taken before B pushes and pushes to the real
    // remote, as if B pushed between A's fetch and A's push.
    git(
        &f.dir,
        &["clone", "--quiet", "--bare", "remote.git", "stale.git"],
    );
    let url = |name: &str| f.dir.join(name).to_string_lossy().into_owned();
    git(&a.root, &["remote", "set-url", "origin", &url("stale.git")]);
    git(
        &a.root,
        &["remote", "set-url", "--push", "origin", &url("remote.git")],
    );
    write(&b, "b.md", "b from B\n");
    assert_eq!(b.sync(true, "B").unwrap().outcome, Outcome::Synced);
    write(&a, "a.md", "a from A\n");
    let report = a.sync(true, "A").unwrap();
    assert_eq!(
        (report.committed, report.outcome, report.ahead),
        (true, Outcome::Retry, 1)
    );
    git(
        &a.root,
        &["remote", "set-url", "origin", &url("remote.git")],
    );
    let report = a.sync(true, "A").unwrap();
    assert_eq!(
        (report.committed, report.outcome, report.ahead),
        (false, Outcome::Synced, 0)
    );
    assert_eq!(read(&a, "b.md").as_deref(), Some("b from B\n"));
    assert_eq!(b.sync(true, "B").unwrap().outcome, Outcome::Synced);
    assert_eq!(read(&b, "a.md").as_deref(), Some("a from A\n"));
}
