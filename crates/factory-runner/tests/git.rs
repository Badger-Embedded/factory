use std::fs;
use std::path::Path;
use std::process::Command;

use factory_runner::Git;

/// Runs git for test setup, without the user's config.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit(dir: &Path, file: &str, message: &str) {
    fs::write(dir.join(file), message).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", message]);
}

/// A bare repository at `<root>/remotes/owner/repo.git` with one commit on
/// main, and a clone of it in `<root>/upstream` to push more commits from.
struct Remote {
    root: tempfile::TempDir,
}

impl Remote {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remotes/owner/repo.git");
        fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "--bare", "--initial-branch=main"]);
        let upstream = root.path().join("upstream");
        git(
            root.path(),
            &["clone", "-q", bare.to_str().unwrap(), "upstream"],
        );
        git(&upstream, &["checkout", "-q", "-b", "main"]);
        commit(&upstream, "README", "first");
        git(&upstream, &["push", "-q", "origin", "main"]);
        Remote { root }
    }

    fn git(&self) -> Git {
        Git {
            base_url: format!("file://{}", self.root.path().join("remotes").display()),
            token: Some("ghp_secret".into()),
            ..Git::default()
        }
    }

    fn upstream(&self) -> std::path::PathBuf {
        self.root.path().join("upstream")
    }

    fn work(&self) -> std::path::PathBuf {
        self.root.path().join("jobs/7/work")
    }

    fn remote_log(&self, branch: &str) -> String {
        git(
            &self.root.path().join("remotes/owner/repo.git"),
            &["log", "--format=%s", branch],
        )
    }
}

#[tokio::test]
async fn planning_clones_the_base_branch_with_the_bot_identity() {
    let remote = Remote::new();
    let work = remote.work();
    remote
        .git()
        .prepare(&work, "owner/repo", "main", None)
        .await
        .unwrap();

    assert_eq!(fs::read_to_string(work.join("README")).unwrap(), "first");
    assert_eq!(git(&work, &["config", "user.name"]), "moonthebot");
    let config = fs::read_to_string(work.join(".git/config")).unwrap();
    assert!(!config.contains("ghp_secret"), "{config}");
    assert!(!config.contains("extraHeader"), "{config}");
}

#[tokio::test]
async fn a_second_planning_run_keeps_the_checkout() {
    let remote = Remote::new();
    let work = remote.work();
    let git_ = remote.git();
    git_.prepare(&work, "owner/repo", "main", None)
        .await
        .unwrap();
    fs::write(work.join("notes"), "agent scratch").unwrap();
    commit(&remote.upstream(), "later", "second");
    git(&remote.upstream(), &["push", "-q", "origin", "main"]);

    git_.prepare(&work, "owner/repo", "main", None)
        .await
        .unwrap();
    assert!(work.join("notes").exists());
    assert!(!work.join("later").exists());
}

#[tokio::test]
async fn a_pull_request_branch_starts_from_the_newest_base_and_keeps_its_commits() {
    let remote = Remote::new();
    let work = remote.work();
    let git_ = remote.git();
    git_.prepare(&work, "owner/repo", "main", None)
        .await
        .unwrap();
    commit(&remote.upstream(), "merged", "merged earlier");
    git(&remote.upstream(), &["push", "-q", "origin", "main"]);

    let branch = "factory/job-7-pr-1";
    git_.prepare(&work, "owner/repo", "main", Some(branch))
        .await
        .unwrap();
    assert_eq!(git(&work, &["branch", "--show-current"]), branch);
    assert!(work.join("merged").exists());

    commit(&work, "feature", "agent work");
    git(&work, &["checkout", "-q", "--detach"]);
    git_.prepare(&work, "owner/repo", "main", Some(branch))
        .await
        .unwrap();
    assert_eq!(git(&work, &["branch", "--show-current"]), branch);
    assert_eq!(git(&work, &["log", "-1", "--format=%s"]), "agent work");
}

#[tokio::test]
async fn push_publishes_the_branch() {
    let remote = Remote::new();
    let work = remote.work();
    let git_ = remote.git();
    let branch = "factory/job-7-pr-1";
    git_.prepare(&work, "owner/repo", "main", Some(branch))
        .await
        .unwrap();
    commit(&work, "feature", "agent work");

    git_.push(&work, branch).await.unwrap();
    assert_eq!(remote.remote_log(branch), "agent work\nfirst");

    git(
        &work,
        &["commit", "-q", "--amend", "-m", "agent work, revised"],
    );
    git_.push(&work, branch).await.unwrap();
    assert_eq!(remote.remote_log(branch), "agent work, revised\nfirst");
}

#[tokio::test]
async fn a_half_finished_clone_is_replaced() {
    let remote = Remote::new();
    let work = remote.work();
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("partial"), "").unwrap();

    remote
        .git()
        .prepare(&work, "owner/repo", "main", None)
        .await
        .unwrap();
    assert!(!work.join("partial").exists());
    assert!(work.join("README").exists());
}

#[tokio::test]
async fn a_missing_repository_fails_with_gits_message() {
    let remote = Remote::new();
    let err = remote
        .git()
        .prepare(&remote.work(), "owner/missing", "main", None)
        .await
        .unwrap_err();
    assert!(err.starts_with("git clone failed:"), "{err}");
}

#[tokio::test]
async fn every_pull_request_run_fetches_the_newest_base() {
    let remote = Remote::new();
    let work = remote.work();
    let git_ = remote.git();
    let branch = "factory/job-7-pr-1";
    git_.prepare(&work, "owner/repo", "main", Some(branch))
        .await
        .unwrap();
    commit(&work, "feature", "agent work");

    commit(&remote.upstream(), "other", "merged meanwhile");
    git(&remote.upstream(), &["push", "-q", "origin", "main"]);
    git_.prepare(&work, "owner/repo", "main", Some(branch))
        .await
        .unwrap();

    assert_eq!(
        git(&work, &["log", "-1", "--format=%s", "origin/main"]),
        "merged meanwhile"
    );
    assert_eq!(git(&work, &["log", "-1", "--format=%s"]), "agent work");
}

#[tokio::test]
async fn commits_that_credit_an_ai_tool_are_found() {
    let remote = Remote::new();
    let work = remote.work();
    let git_ = remote.git();
    git_.prepare(&work, "owner/repo", "main", Some("factory/job-7-pr-1"))
        .await
        .unwrap();
    assert_eq!(
        git_.attributed_commits(&work, "main").await.unwrap(),
        Vec::<String>::new()
    );

    commit(&work, "a", "clean change");
    commit(
        &work,
        "b",
        "with trailer\n\nBody.\n\nCo-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>",
    );
    commit(
        &work,
        "c",
        "with footer\n\n\u{1f916} Generated with [Claude Code](https://claude.com/claude-code)",
    );
    commit(
        &work,
        "d",
        "human co-author\n\nCo-authored-by: Alice <alice@example.com>",
    );

    let found = git_.attributed_commits(&work, "main").await.unwrap();
    let subjects: Vec<&str> = found
        .iter()
        .map(|entry| entry.split_once(' ').unwrap().1)
        .collect();
    assert_eq!(subjects, ["with footer", "with trailer"]);
}
