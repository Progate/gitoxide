//! Pushes against a real `git receive-pack`, spawned through the `file://` transport.
//!
//! The same protocol code runs over HTTP (stateless-rpc), which differs only in how the transport frames requests.
#![cfg(feature = "blocking-network-client")]

use std::path::Path;
use std::process::Command;

use gix::remote::{
    Direction,
    push::{Options, Rejection, Status, Update},
};
use gix_testtools::tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .expect("git can be executed");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8").trim().to_owned()
}

/// A bare `remote.git` and a clone of it in `local` with one commit on `main`, which isn't pushed yet.
fn setup() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    git(tmp.path(), &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    git(tmp.path(), &["init", "-q", "-b", "main", "local"]);
    std::fs::write(local.join("a.txt"), "a\n").unwrap();
    std::fs::create_dir(local.join("dir")).unwrap();
    std::fs::write(local.join("dir/b.txt"), "b\n").unwrap();
    git(&local, &["add", "."]);
    git(&local, &["commit", "-q", "-m", "first"]);
    git(&local, &["remote", "add", "origin", remote.to_str().unwrap()]);
    (tmp, remote, local)
}

fn push(local: &Path, updates: Vec<Update>) -> gix::remote::push::Outcome {
    let repo = gix::open_opts(local, gix::open::Options::isolated()).expect("valid repo");
    let remote = repo.find_remote("origin").expect("origin exists");
    let connection = remote.connect(Direction::Push).expect("can connect");
    connection
        .push(updates, gix::progress::Discard, |_, _| {}, Options::default())
        .expect("push works")
}

fn head_id(local: &Path) -> gix::ObjectId {
    gix::ObjectId::from_hex(git(local, &["rev-parse", "HEAD"]).as_bytes()).unwrap()
}

fn update(local: &Path, name: &str) -> Update {
    Update {
        local: Some(head_id(local)),
        remote: name.into(),
        force: false,
    }
}

#[test]
fn create_then_fast_forward_then_up_to_date() {
    let (_tmp, remote, local) = setup();

    let outcome = push(&local, vec![update(&local, "refs/heads/main")]);
    assert_eq!(outcome.updates.len(), 1);
    assert_eq!(outcome.updates[0].status, Status::Ok);
    assert_eq!(outcome.updates[0].old, None);
    assert!(outcome.objects_sent >= 5, "commit, two trees, two blobs");
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/main"]),
        head_id(&local).to_string()
    );
    git(&remote, &["fsck", "--strict"]);

    std::fs::write(local.join("a.txt"), "a\nchanged\n").unwrap();
    git(&local, &["commit", "-q", "-am", "second"]);
    let outcome = push(&local, vec![update(&local, "refs/heads/main")]);
    assert_eq!(outcome.updates[0].status, Status::Ok);
    assert_eq!(
        outcome.objects_sent, 3,
        "only the new commit, its root tree and the changed blob are sent, the unchanged subtree is not"
    );
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/main"]),
        head_id(&local).to_string()
    );
    git(&remote, &["fsck", "--strict"]);

    let outcome = push(&local, vec![update(&local, "refs/heads/main")]);
    assert_eq!(outcome.updates[0].status, Status::UpToDate);
    assert_eq!(outcome.objects_sent, 0);
}

#[test]
fn non_fast_forward_is_rejected_unless_forced() {
    let (tmp, remote, local) = setup();
    push(&local, vec![update(&local, "refs/heads/main")]);

    // Someone else pushes a commit we don't have.
    git(tmp.path(), &["clone", "-q", remote.to_str().unwrap(), "other"]);
    let other = tmp.path().join("other");
    std::fs::write(other.join("c.txt"), "c\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "other"]);
    git(&other, &["push", "-q", "origin", "main"]);

    std::fs::write(local.join("a.txt"), "diverged\n").unwrap();
    git(&local, &["commit", "-q", "-am", "diverged"]);
    let outcome = push(&local, vec![update(&local, "refs/heads/main")]);
    assert_eq!(outcome.updates[0].status, Status::Rejected(Rejection::FetchFirst));
    assert!(!outcome.is_success());

    // Once we have the object, it's a plain non-fast-forward.
    git(&local, &["fetch", "-q", "origin"]);
    let outcome = push(&local, vec![update(&local, "refs/heads/main")]);
    assert_eq!(outcome.updates[0].status, Status::Rejected(Rejection::NonFastForward));

    let mut forced = update(&local, "refs/heads/main");
    forced.force = true;
    let outcome = push(&local, vec![forced]);
    assert_eq!(outcome.updates[0].status, Status::Ok);
    assert!(outcome.updates[0].forced);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/main"]),
        head_id(&local).to_string()
    );
    git(&remote, &["fsck", "--strict"]);
}

#[test]
fn new_branch_and_deletion() {
    let (_tmp, remote, local) = setup();
    push(&local, vec![update(&local, "refs/heads/main")]);

    let outcome = push(&local, vec![update(&local, "refs/heads/feature")]);
    assert_eq!(outcome.updates[0].status, Status::Ok);
    assert_eq!(outcome.objects_sent, 0, "the remote already has everything");
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/feature"]),
        head_id(&local).to_string()
    );

    let outcome = push(
        &local,
        vec![Update {
            local: None,
            remote: "refs/heads/feature".into(),
            force: false,
        }],
    );
    assert_eq!(outcome.updates[0].status, Status::Ok);
    assert_eq!(git(&remote, &["branch", "--list", "feature"]), "");

    let outcome = push(
        &local,
        vec![Update {
            local: None,
            remote: "refs/heads/does-not-exist".into(),
            force: false,
        }],
    );
    assert_eq!(outcome.updates[0].status, Status::Rejected(Rejection::NoSuchRef));
}

#[test]
fn remote_rejection_is_reported() {
    let (_tmp, remote, local) = setup();
    let hook = remote.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\necho 'no pushes today' >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let repo = gix::open_opts(&local, gix::open::Options::isolated()).unwrap();
    let origin = repo.find_remote("origin").unwrap();
    let mut messages = Vec::new();
    let outcome = origin
        .connect(Direction::Push)
        .unwrap()
        .push(
            vec![update(&local, "refs/heads/main")],
            gix::progress::Discard,
            |_, text| messages.extend_from_slice(text),
            Options::default(),
        )
        .unwrap();
    assert_eq!(
        outcome.updates[0].status,
        Status::RemoteRejected("pre-receive hook declined".into())
    );
    assert!(
        String::from_utf8_lossy(&messages).contains("no pushes today"),
        "the remote's messages are passed on: {:?}",
        String::from_utf8_lossy(&messages)
    );
}
