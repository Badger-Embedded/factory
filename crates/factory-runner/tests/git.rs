use std::fs;
use std::path::Path;
use std::process::Command;

use factory_runner::{Account, Git, GitHub};

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
struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
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
        Fixture { root }
    }

    /// The fixture's repository, reached through the GitHub forge's
    /// authentication header and a `file://` URL.
    fn remote(&self) -> factory_runner::Remote {
        self.remote_as("moonthebot", 7)
    }

    fn remote_as(&self, login: &str, id: u64) -> factory_runner::Remote {
        let account = Account {
            token: Some("ghp_secret".into()),
            login: login.into(),
            name: login.into(),
            email: format!("{id}+{login}@users.noreply.github.com"),
        };
        factory_runner::Remote {
            url: format!(
                "file://{}",
                self.root.path().join("remotes/owner/repo.git").display()
            ),
            ..factory_runner::Remote::new(&GitHub::default(), &account, "owner/repo")
        }
    }

    fn upstream(&self) -> std::path::PathBuf {
        self.root.path().join("upstream")
    }

    fn work(&self) -> std::path::PathBuf {
        self.root.path().join("jobs/7/work")
    }

    fn remote_log(&self, branch: &str) -> String {
        let bare = self.root.path().join("remotes/owner/repo.git");
        git(
            self.root.path(),
            &[
                "--git-dir",
                bare.to_str().unwrap(),
                "log",
                "--format=%s",
                branch,
            ],
        )
    }
}

#[tokio::test]
async fn planning_clones_the_base_branch_with_the_bot_identity() {
    let remote = Fixture::new();
    let work = remote.work();
    Git::default()
        .prepare(&work, &|| Ok(remote.remote()), "main", None)
        .await
        .unwrap();

    assert_eq!(fs::read_to_string(work.join("README")).unwrap(), "first");
    assert_eq!(git(&work, &["config", "user.name"]), "moonthebot");
    let config = fs::read_to_string(work.join(".git/config")).unwrap();
    assert!(!config.contains("ghp_secret"), "{config}");
    assert!(!config.contains("extraHeader"), "{config}");
}

#[tokio::test]
async fn a_new_account_takes_over_an_existing_checkout() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    git_.prepare(&work, &|| Ok(remote.remote_as("first", 1)), "main", None)
        .await
        .unwrap();
    assert_eq!(git(&work, &["config", "user.name"]), "first");

    git_.prepare(&work, &|| Ok(remote.remote_as("second", 2)), "main", None)
        .await
        .unwrap();
    assert_eq!(git(&work, &["config", "user.name"]), "second");
    assert_eq!(
        git(&work, &["config", "user.email"]),
        "2+second@users.noreply.github.com"
    );
}

#[tokio::test]
async fn a_second_planning_run_keeps_the_checkout() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    git_.prepare(&work, &|| Ok(remote.remote()), "main", None)
        .await
        .unwrap();
    fs::write(work.join("notes"), "agent scratch").unwrap();
    commit(&remote.upstream(), "later", "second");
    git(&remote.upstream(), &["push", "-q", "origin", "main"]);

    git_.prepare(&work, &|| Ok(remote.remote()), "main", None)
        .await
        .unwrap();
    assert!(work.join("notes").exists());
    assert!(!work.join("later").exists());
}

#[tokio::test]
async fn a_pull_request_branch_starts_from_the_newest_base_and_keeps_its_commits() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    git_.prepare(&work, &|| Ok(remote.remote()), "main", None)
        .await
        .unwrap();
    commit(&remote.upstream(), "merged", "merged earlier");
    git(&remote.upstream(), &["push", "-q", "origin", "main"]);

    let branch = "factory/job-7-pr-1";
    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();
    assert_eq!(git(&work, &["branch", "--show-current"]), branch);
    assert!(work.join("merged").exists());

    commit(&work, "feature", "agent work");
    git(&work, &["checkout", "-q", "--detach"]);
    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();
    assert_eq!(git(&work, &["branch", "--show-current"]), branch);
    assert_eq!(git(&work, &["log", "-1", "--format=%s"]), "agent work");
}

#[tokio::test]
async fn a_rebase_left_unfinished_is_aborted() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    let branch = "factory/job-7-pr-1";
    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();
    commit(&work, "README", "agent edit");
    commit(&remote.upstream(), "README", "upstream edit");
    git(&remote.upstream(), &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin", "main"]);
    let conflict = Command::new("git")
        .args(["rebase", "origin/main"])
        .current_dir(&work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(!conflict.status.success(), "the rebase should conflict");

    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();
    assert_eq!(git(&work, &["branch", "--show-current"]), branch);
    assert_eq!(git(&work, &["log", "-1", "--format=%s"]), "agent edit");
    assert_eq!(git(&work, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn push_publishes_the_branch() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    let branch = "factory/job-7-pr-1";
    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();
    commit(&work, "feature", "agent work");

    git_.push(&work, &|| Ok(remote.remote()), branch, &|| {})
        .await
        .unwrap();
    assert_eq!(remote.remote_log(branch), "agent work\nfirst");

    git(
        &work,
        &["commit", "-q", "--amend", "-m", "agent work, revised"],
    );
    git_.push(&work, &|| Ok(remote.remote()), branch, &|| {})
        .await
        .unwrap();
    assert_eq!(remote.remote_log(branch), "agent work, revised\nfirst");
}

#[tokio::test]
async fn a_half_finished_clone_is_replaced() {
    let remote = Fixture::new();
    let work = remote.work();
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("partial"), "").unwrap();

    Git::default()
        .prepare(&work, &|| Ok(remote.remote()), "main", None)
        .await
        .unwrap();
    assert!(!work.join("partial").exists());
    assert!(work.join("README").exists());
}

#[tokio::test]
async fn a_missing_repository_fails_with_gits_message() {
    let remote = Fixture::new();
    let err = Git::default()
        .prepare(
            &remote.work(),
            &|| {
                Ok(factory_runner::Remote {
                    url: remote.remote().url.replace("owner/repo", "owner/missing"),
                    ..remote.remote()
                })
            },
            "main",
            None,
        )
        .await
        .unwrap_err();
    assert!(err.starts_with("git clone failed:"), "{err}");
}

#[tokio::test]
async fn every_pull_request_run_fetches_the_newest_base() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    let branch = "factory/job-7-pr-1";
    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();
    commit(&work, "feature", "agent work");

    commit(&remote.upstream(), "other", "merged meanwhile");
    git(&remote.upstream(), &["push", "-q", "origin", "main"]);
    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();

    assert_eq!(
        git(&work, &["log", "-1", "--format=%s", "origin/main"]),
        "merged meanwhile"
    );
    assert_eq!(git(&work, &["log", "-1", "--format=%s"]), "agent work");
}

#[tokio::test]
async fn commits_already_on_a_picked_pull_request_are_not_attributed_to_the_job() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    git_.prepare(
        &work,
        &|| Ok(remote.remote()),
        "main",
        Some("factory/job-7-pr-1"),
    )
    .await
    .unwrap();
    let trailer = "\n\nCo-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>";
    commit(&work, "a", &format!("theirs{trailer}"));
    git(&work, &["update-ref", "refs/factory/pr-12/head", "HEAD"]);
    commit(&work, "b", &format!("ours{trailer}"));

    let all = git_
        .attributed_commits_after(&work, "main", None)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    let ours = git_
        .attributed_commits_after(&work, "main", Some(12))
        .await
        .unwrap();
    assert_eq!(ours.len(), 1);
    assert!(ours[0].ends_with("ours"), "{ours:?}");
}

#[tokio::test]
async fn commits_that_credit_an_ai_tool_are_found() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    git_.prepare(
        &work,
        &|| Ok(remote.remote()),
        "main",
        Some("factory/job-7-pr-1"),
    )
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

#[tokio::test]
async fn a_branch_summary_lists_the_commits_and_the_diff_stat() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    git_.prepare(
        &work,
        &|| Ok(remote.remote()),
        "main",
        Some("factory/job-7-pr-1"),
    )
    .await
    .unwrap();
    commit(&work, "a", "add a");
    commit(&work, "b", "add b");
    // The base moves on after the branch was cut. Its change is not ours.
    commit(&remote.upstream(), "later", "second");
    git(&remote.upstream(), &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin", "main"]);

    let summary = git_.summarize_branch(&work, "main").await.unwrap();
    let subjects: Vec<&str> = summary
        .commits
        .iter()
        .map(|entry| entry.split_once(' ').unwrap().1)
        .collect();
    assert_eq!(subjects, ["add b", "add a"]);
    assert_eq!(summary.commits[0].split_once(' ').unwrap().0.len(), 7);
    assert!(
        summary.diff_stat.contains(" a | 1 +"),
        "{}",
        summary.diff_stat
    );
    assert!(
        summary.diff_stat.contains(" b | 1 +"),
        "{}",
        summary.diff_stat
    );
    assert!(
        !summary.diff_stat.contains("later"),
        "{}",
        summary.diff_stat
    );
    assert!(
        summary
            .diff_stat
            .ends_with("2 files changed, 2 insertions(+)"),
        "{}",
        summary.diff_stat
    );
}

#[tokio::test]
async fn a_branch_with_no_commits_ahead_has_an_empty_summary() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    git_.prepare(
        &work,
        &|| Ok(remote.remote()),
        "main",
        Some("factory/job-7-pr-1"),
    )
    .await
    .unwrap();

    let summary = git_.summarize_branch(&work, "main").await.unwrap();
    assert_eq!(summary.commits, Vec::<String>::new());
    assert_eq!(summary.diff_stat, "");
}

#[cfg(unix)]
fn fake_git(dir: &Path, body: &str) -> Git {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-git");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    // A concurrent test's fork can hold the script's write handle briefly, so exec fails with ETXTBSY.
    while let Err(e) = std::process::Command::new(&path).output() {
        assert_eq!(e.raw_os_error(), Some(26), "{e}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Git { git: path }
}

#[cfg(unix)]
#[tokio::test]
async fn a_failure_that_echoes_the_credentials_does_not_carry_them() {
    let remote = Fixture::new();
    let tools = tempfile::tempdir().unwrap();
    let git_ = fake_git(
        tools.path(),
        "echo \"fatal: $GIT_CONFIG_VALUE_0 token ghp_secret\" >&2; exit 128",
    );
    let err = git_
        .prepare(&remote.work(), &|| Ok(remote.remote()), "main", None)
        .await
        .unwrap_err();
    assert!(err.starts_with("git clone failed:"), "{err}");
    assert!(!err.contains("ghp_secret"), "{err}");
    assert!(!err.contains("Basic"), "{err}");
    assert!(!err.starts_with(factory_runner::REJECTED), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_refused_token_is_marked_where_git_ran() {
    let remote = Fixture::new();
    let tools = tempfile::tempdir().unwrap();
    let git_ = fake_git(
        tools.path(),
        "echo \"fatal: Authentication failed for $GIT_CONFIG_VALUE_0\" >&2; exit 128",
    );
    let err = git_
        .prepare(&remote.work(), &|| Ok(remote.remote()), "main", None)
        .await
        .unwrap_err();
    assert!(err.starts_with(factory_runner::REJECTED), "{err}");
    assert!(!err.contains("ghp_secret"), "{err}");
}

#[tokio::test]
async fn authenticated_commands_ignore_the_checkouts_origin_and_hooks() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    let branch = "factory/job-7-pr-1";
    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();
    commit(&work, "feature", "agent work");

    let marker = remote.root.path().join("hook-ran");
    let hooks = work.join("evil-hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-push");
    fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    git(
        &work,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    git(&work, &["remote", "set-url", "origin", "file:///nowhere"]);

    git_.push(&work, &|| Ok(remote.remote()), branch, &|| {})
        .await
        .unwrap();
    assert_eq!(remote.remote_log(branch), "agent work\nfirst");
    assert!(!marker.exists(), "a checkout hook ran with the credentials");
}

#[cfg(unix)]
#[tokio::test]
async fn updating_the_base_runs_no_checkout_hooks() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    let branch = "factory/job-7-pr-1";
    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();

    let marker = remote.root.path().join("hook-ran");
    let hook = work.join(".git/hooks/reference-transaction");
    fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    git(&work, &["config", "core.fsmonitor", "touch-never-runs"]);

    let upstream = remote.root.path().join("upstream");
    commit(&upstream, "newer", "newer base");
    git(&upstream, &["push", "-q", "origin", "HEAD:main"]);
    let before = git(&work, &["rev-parse", "origin/main"]);

    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();
    assert_ne!(git(&work, &["rev-parse", "origin/main"]), before);
    assert!(!marker.exists(), "a checkout hook ran during the fetch");
}

#[cfg(unix)]
#[tokio::test]
async fn the_push_resolves_its_remote_after_the_copy_into_the_transfer_repository() {
    let remote = Fixture::new();
    let work = remote.work();
    let branch = "factory/job-7-pr-1";
    Git::default()
        .prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();
    commit(&work, "feature", "agent work");

    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("calls");
    let git_ = fake_git(
        tools.path(),
        &format!("echo \"$@\" >> {}\nexec git \"$@\"", log.display()),
    );
    let seen = std::sync::Mutex::new(String::new());
    git_.push(
        &work,
        &|| {
            *seen.lock().unwrap() = fs::read_to_string(&log).unwrap();
            Ok(remote.remote())
        },
        branch,
        &|| {},
    )
    .await
    .unwrap();
    let seen = seen.into_inner().unwrap();
    assert!(seen.contains(" fetch "), "{seen}");
    assert!(!seen.contains(" push "), "{seen}");
}

#[tokio::test]
async fn transport_settings_in_the_checkout_are_not_loaded_with_the_credentials() {
    let remote = Fixture::new();
    let work = remote.work();
    let git_ = Git::default();
    let branch = "factory/job-7-pr-1";
    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();
    commit(&work, "feature", "agent work");
    for (key, value) in [
        ("url.https://evil.invalid/.insteadOf", "file://"),
        ("protocol.file.allow", "never"),
        ("http.proxy", "http://evil.invalid:3128"),
        ("http.sslVerify", "false"),
        ("include.path", "/nonexistent"),
    ] {
        git(&work, &["config", key, value]);
    }

    git_.push(&work, &|| Ok(remote.remote()), branch, &|| {})
        .await
        .unwrap();
    assert_eq!(remote.remote_log(branch), "agent work\nfirst");
}

#[cfg(unix)]
#[tokio::test]
async fn the_credential_goes_to_a_repository_the_factory_made() {
    let remote = Fixture::new();
    let work = remote.work();
    let tools = tempfile::tempdir().unwrap();
    let seen = tools.path().join("seen");
    let git_ = fake_git(
        tools.path(),
        &format!(
            "case \"$GIT_CONFIG_VALUE_0\" in *Authorization*) echo \"$PWD $*\" >> {} ;; esac\n\
             exec git \"$@\"",
            seen.display()
        ),
    );
    let branch = "factory/job-7-pr-1";
    git_.prepare(&work, &|| Ok(remote.remote()), "main", Some(branch))
        .await
        .unwrap();
    commit(&work, "feature", "agent work");
    git_.push(&work, &|| Ok(remote.remote()), branch, &|| {})
        .await
        .unwrap();

    let seen = fs::read_to_string(&seen).unwrap();
    assert!(
        !seen
            .lines()
            .any(|line| line.starts_with(work.to_str().unwrap())),
        "{seen}"
    );
    assert!(
        seen.lines()
            .all(|line| line.contains("--git-dir=") || line.contains(" clone ")),
        "{seen}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn the_remote_is_resolved_before_each_authenticated_command() {
    let remote = Fixture::new();
    let work = remote.work();
    let asked = std::sync::atomic::AtomicUsize::new(0);
    let git_ = Git::default();
    git_.prepare(
        &work,
        &|| {
            let n = asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            Ok(remote.remote_as(&format!("bot{n}"), n as u64))
        },
        "main",
        Some("factory/job-7-pr-1"),
    )
    .await
    .unwrap();

    // One for the clone and one for the fetch, which sets the identity.
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(git(&work, &["config", "user.name"]), "bot2");
}

#[cfg(unix)]
#[tokio::test]
async fn a_pull_request_prepare_resolves_the_remote_for_each_fetch() {
    let remote = Fixture::new();
    let head = open_pull_request(&remote, "main");
    let work = remote.work();
    let asked = std::sync::atomic::AtomicUsize::new(0);
    Git::default()
        .prepare_pr(
            &work,
            &|| {
                let n = asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                Ok(remote.remote_as(&format!("bot{n}"), n as u64))
            },
            "main",
            head,
            5,
            "pr-branch",
        )
        .await
        .unwrap();

    // The clone, the base fetch with the identity, and the head fetch.
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(git(&work, &["config", "user.name"]), "bot2");
}

/// Makes the forge's head ref of pull request 5 point at a new commit on
/// `pr-branch`, as a fork's pull request would have, and returns the ref.
fn open_pull_request(fixture: &Fixture, base: &str) -> &'static str {
    let upstream = fixture.upstream();
    git(&upstream, &["checkout", "-q", "-b", "pr-branch", base]);
    commit(&upstream, "pr-file", "pr commit");
    let bare_repo = fixture.root.path().join("remotes/owner/repo.git");
    git(
        &upstream,
        &[
            "push",
            "-q",
            bare_repo.to_str().unwrap(),
            "pr-branch:refs/pull/5/head",
        ],
    );
    git(&upstream, &["checkout", "-q", "main"]);
    "refs/pull/5/head"
}

async fn prepare_pr(remote: &Fixture, git_: &Git, base: &str, head: &str) {
    git_.prepare_pr(
        &remote.work(),
        &|| Ok(remote.remote()),
        base,
        head,
        5,
        "pr-branch",
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_pull_request_ref_starts_the_branch_and_commits_land_on_top() {
    let remote = Fixture::new();
    let head = open_pull_request(&remote, "main");
    let work = remote.work();
    let git_ = Git::default();
    prepare_pr(&remote, &git_, "main", head).await;
    assert_eq!(git(&work, &["branch", "--show-current"]), "pr-branch");
    assert!(work.join("pr-file").exists());

    commit(&work, "fix", "agent fix");
    git_.push_fast_forward(
        &work,
        &|| Ok(remote.remote()),
        "pr-branch",
        "pr-branch",
        &|| {},
    )
    .await
    .unwrap();
    assert_eq!(
        remote.remote_log("pr-branch"),
        "agent fix\npr commit\nfirst"
    );
}

#[tokio::test]
async fn a_foreign_commit_is_never_overwritten() {
    let remote = Fixture::new();
    let head = open_pull_request(&remote, "main");
    let work = remote.work();
    let git_ = Git::default();
    let bare = remote.root.path().join("remotes/owner/repo.git");
    git(
        remote.root.path(),
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "update-ref",
            "refs/heads/pr-branch",
            "refs/pull/5/head",
        ],
    );
    prepare_pr(&remote, &git_, "main", head).await;
    commit(&work, "fix", "agent fix");

    let upstream = remote.upstream();
    git(&upstream, &["checkout", "-q", "pr-branch"]);
    commit(&upstream, "theirs", "foreign commit");
    git(&upstream, &["push", "-q", "origin", "pr-branch"]);
    git(
        &upstream,
        &[
            "push",
            "-q",
            bare.to_str().unwrap(),
            "pr-branch:refs/pull/5/head",
        ],
    );

    let rejected = git_
        .push_fast_forward(
            &work,
            &|| Ok(remote.remote()),
            "pr-branch",
            "pr-branch",
            &|| {},
        )
        .await
        .unwrap_err();
    assert!(
        matches!(rejected, factory_runner::PushError::Rejected(_)),
        "{rejected}"
    );
    assert_eq!(
        remote.remote_log("pr-branch"),
        "foreign commit\npr commit\nfirst"
    );

    prepare_pr(&remote, &git_, "main", head).await;
    assert!(work.join("theirs").exists());
    assert!(work.join("fix").exists());
    git_.push_fast_forward(
        &work,
        &|| Ok(remote.remote()),
        "pr-branch",
        "pr-branch",
        &|| {},
    )
    .await
    .unwrap();
    let log = remote.remote_log("pr-branch");
    assert!(
        log.contains("foreign commit") && log.contains("agent fix"),
        "{log}"
    );
}

#[tokio::test]
async fn a_base_merge_makes_a_merge_commit_and_the_summary_uses_the_pr_base() {
    let remote = Fixture::new();
    let upstream = remote.upstream();
    git(&upstream, &["checkout", "-q", "-b", "develop"]);
    commit(&upstream, "dev", "develop work");
    git(&upstream, &["push", "-q", "origin", "develop"]);
    let head = open_pull_request(&remote, "develop");
    let work = remote.work();
    let git_ = Git::default();
    prepare_pr(&remote, &git_, "develop", head).await;

    git(&upstream, &["checkout", "-q", "develop"]);
    commit(&upstream, "dev2", "develop moved");
    git(&upstream, &["push", "-q", "origin", "develop"]);
    prepare_pr(&remote, &git_, "develop", head).await;
    assert!(git_.merge_base(&work, "develop").await.unwrap());
    assert_eq!(
        git(&work, &["rev-list", "--parents", "-n1", "HEAD"])
            .split(' ')
            .count(),
        3
    );
    assert!(work.join("dev2").exists());

    let summary = git_.summarize_branch(&work, "develop").await.unwrap();
    assert!(
        summary.diff_stat.contains("pr-file"),
        "{}",
        summary.diff_stat
    );
    assert!(!summary.diff_stat.contains("dev"), "{}", summary.diff_stat);
}

#[test]
fn the_head_ref_depends_on_the_forge() {
    assert_eq!(
        factory_runner::ForgeKind::GitHub.pr_head_ref(5),
        "refs/pull/5/head"
    );
    assert_eq!(
        factory_runner::ForgeKind::Salsa.pr_head_ref(5),
        "refs/merge-requests/5/head"
    );
}

#[tokio::test]
async fn a_conflicted_base_merge_survives_the_next_prepare() {
    let remote = Fixture::new();
    let head = open_pull_request(&remote, "main");
    let work = remote.work();
    let git_ = Git::default();
    prepare_pr(&remote, &git_, "main", head).await;
    commit(&work, "README", "pr edit");
    commit(&remote.upstream(), "README", "main edit");
    git(&remote.upstream(), &["push", "-q", "origin", "main"]);
    prepare_pr(&remote, &git_, "main", head).await;
    assert!(!git_.merge_base(&work, "main").await.unwrap());

    prepare_pr(&remote, &git_, "main", head).await;
    assert_eq!(git(&work, &["branch", "--show-current"]), "pr-branch");
    assert!(work.join(".git/MERGE_HEAD").exists());
    assert!(git(&work, &["status", "--porcelain"]).contains("UU README"));
}

#[tokio::test]
async fn a_pull_request_into_a_branch_named_like_its_ref_keeps_its_base() {
    let remote = Fixture::new();
    let upstream = remote.upstream();
    git(&upstream, &["checkout", "-q", "-b", "pr-5"]);
    commit(&upstream, "base-file", "pr-5 base");
    git(&upstream, &["push", "-q", "origin", "pr-5"]);
    let head = open_pull_request(&remote, "pr-5");
    let work = remote.work();
    prepare_pr(&remote, &Git::default(), "pr-5", head).await;

    let summary = Git::default()
        .summarize_branch(&work, "pr-5")
        .await
        .unwrap();
    assert_eq!(summary.commits.len(), 1, "{:?}", summary.commits);
    assert!(summary.commits[0].ends_with("pr commit"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_push_that_loses_a_race_is_reported_as_rejected() {
    let remote = Fixture::new();
    let head = open_pull_request(&remote, "main");
    let work = remote.work();
    let bare = remote.root.path().join("remotes/owner/repo.git");
    git(
        remote.root.path(),
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "update-ref",
            "refs/heads/pr-branch",
            "refs/pull/5/head",
        ],
    );
    prepare_pr(&remote, &Git::default(), "main", head).await;
    commit(&work, "fix", "agent fix");

    // Someone pushes after git read the refs, and the server answers with a
    // generic failure instead of a non-fast-forward rejection.
    let upstream = remote.upstream();
    git(&upstream, &["checkout", "-q", "pr-branch"]);
    commit(&upstream, "theirs", "foreign commit");
    let tools = tempfile::tempdir().unwrap();
    let git_ = fake_git(
        tools.path(),
        &format!(
            "if echo \"$@\" | grep -q ' push '; then\n  git -C {} push -q {} pr-branch:pr-branch\n  echo ' ! [remote rejected] pr-branch -> pr-branch (failed to update ref)' >&2\n  exit 1\nfi\nexec git \"$@\"",
            upstream.display(),
            bare.display()
        ),
    );
    let err = git_
        .push_fast_forward(
            &work,
            &|| Ok(remote.remote()),
            "pr-branch",
            "pr-branch",
            &|| {},
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, factory_runner::PushError::Rejected(_)),
        "{err}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_push_that_fails_for_another_reason_is_not_reported_as_rejected() {
    let remote = Fixture::new();
    let head = open_pull_request(&remote, "main");
    let work = remote.work();
    let bare = remote.root.path().join("remotes/owner/repo.git");
    git(
        remote.root.path(),
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "update-ref",
            "refs/heads/pr-branch",
            "refs/pull/5/head",
        ],
    );
    prepare_pr(&remote, &Git::default(), "main", head).await;
    commit(&work, "fix", "agent fix");
    let tools = tempfile::tempdir().unwrap();
    let git_ = fake_git(
        tools.path(),
        "if echo \"$@\" | grep -q ' push '; then\n  echo ' ! [remote rejected] pr-branch (hook declined)' >&2\n  exit 1\nfi\nexec git \"$@\"",
    );
    let err = git_
        .push_fast_forward(
            &work,
            &|| Ok(remote.remote()),
            "pr-branch",
            "pr-branch",
            &|| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(err, factory_runner::PushError::Failed(_)), "{err}");
}

#[tokio::test]
async fn a_pending_base_merge_is_dropped_when_the_pull_request_head_moved() {
    let remote = Fixture::new();
    let head = open_pull_request(&remote, "main");
    let work = remote.work();
    let git_ = Git::default();
    let bare = remote.root.path().join("remotes/owner/repo.git");
    prepare_pr(&remote, &git_, "main", head).await;
    commit(&work, "README", "pr edit");
    commit(&remote.upstream(), "README", "main edit");
    git(&remote.upstream(), &["push", "-q", "origin", "main"]);
    prepare_pr(&remote, &git_, "main", head).await;
    assert!(!git_.merge_base(&work, "main").await.unwrap());

    let upstream = remote.upstream();
    git(&upstream, &["checkout", "-q", "pr-branch"]);
    commit(&upstream, "theirs", "foreign commit");
    git(
        &upstream,
        &[
            "push",
            "-q",
            bare.to_str().unwrap(),
            "pr-branch:refs/pull/5/head",
        ],
    );
    prepare_pr(&remote, &git_, "main", head).await;

    assert!(!work.join(".git/MERGE_HEAD").exists());
    assert!(work.join("theirs").exists());
    assert_eq!(git(&work, &["status", "--porcelain"]), "");
    assert!(!git_.merge_base(&work, "main").await.unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn a_denied_push_is_not_reported_as_rejected_when_the_head_also_moved() {
    let remote = Fixture::new();
    let head = open_pull_request(&remote, "main");
    let work = remote.work();
    let bare = remote.root.path().join("remotes/owner/repo.git");
    git(
        remote.root.path(),
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "update-ref",
            "refs/heads/pr-branch",
            "refs/pull/5/head",
        ],
    );
    prepare_pr(&remote, &Git::default(), "main", head).await;
    commit(&work, "fix", "agent fix");
    let upstream = remote.upstream();
    git(&upstream, &["checkout", "-q", "pr-branch"]);
    commit(&upstream, "theirs", "foreign commit");
    git(&upstream, &["push", "-q", "origin", "pr-branch"]);
    let tools = tempfile::tempdir().unwrap();
    let git_ = fake_git(
        tools.path(),
        "if echo \"$@\" | grep -q ' push '; then\n  echo ' ! [remote rejected] pr-branch (permission denied)' >&2\n  exit 1\nfi\nexec git \"$@\"",
    );
    let err = git_
        .push_fast_forward(
            &work,
            &|| Ok(remote.remote()),
            "pr-branch",
            "pr-branch",
            &|| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(err, factory_runner::PushError::Failed(_)), "{err}");
}

/// Pushes a new commit to the forge's head ref of pull request 5, as a person
/// would after the checkout was made.
fn advance_pull_request(fixture: &Fixture, file: &str, content: &str) {
    let upstream = fixture.upstream();
    git(&upstream, &["checkout", "-q", "pr-branch"]);
    commit(&upstream, file, content);
    let bare_repo = fixture.root.path().join("remotes/owner/repo.git");
    git(
        &upstream,
        &[
            "push",
            "-q",
            bare_repo.to_str().unwrap(),
            "pr-branch:refs/pull/5/head",
        ],
    );
    git(&upstream, &["checkout", "-q", "main"]);
}

#[tokio::test]
async fn a_moved_pull_request_head_merges_into_the_checkout() {
    let remote = Fixture::new();
    let head = open_pull_request(&remote, "main");
    let work = remote.work();
    let git_ = Git::default();
    prepare_pr(&remote, &git_, "main", head).await;
    commit(&work, "agent-file", "agent fix");
    advance_pull_request(&remote, "theirs", "their fix");

    let merged = git_
        .merge_pr_head(&work, &|| Ok(remote.remote()), head, 5)
        .await
        .unwrap();
    assert!(merged);
    assert!(work.join("theirs").exists());
    assert!(work.join("agent-file").exists());
}

#[tokio::test]
async fn a_moved_pull_request_head_that_conflicts_stays_unfinished_and_survives_the_next_prepare() {
    let remote = Fixture::new();
    let head = open_pull_request(&remote, "main");
    let work = remote.work();
    let git_ = Git::default();
    prepare_pr(&remote, &git_, "main", head).await;
    commit(&work, "pr-file", "agent edit");
    advance_pull_request(&remote, "pr-file", "their edit");

    let merged = git_
        .merge_pr_head(&work, &|| Ok(remote.remote()), head, 5)
        .await
        .unwrap();
    assert!(!merged);
    assert!(work.join(".git/MERGE_HEAD").exists());

    prepare_pr(&remote, &git_, "main", head).await;
    assert!(work.join(".git/MERGE_HEAD").exists());
    assert!(git(&work, &["status", "--porcelain"]).contains("UU pr-file"));
}

#[tokio::test]
async fn a_head_that_moves_again_leaves_the_conflict_against_the_latest_head() {
    let remote = Fixture::new();
    let head = open_pull_request(&remote, "main");
    let work = remote.work();
    let git_ = Git::default();
    prepare_pr(&remote, &git_, "main", head).await;
    commit(&work, "pr-file", "agent edit");
    advance_pull_request(&remote, "pr-file", "their edit");
    assert!(
        !git_
            .merge_pr_head(&work, &|| Ok(remote.remote()), head, 5)
            .await
            .unwrap()
    );

    advance_pull_request(&remote, "pr-file", "their second edit");
    prepare_pr(&remote, &git_, "main", head).await;
    let latest = git(&work, &["rev-parse", "refs/factory/pr-5/head"]);
    assert_eq!(git(&work, &["rev-parse", "MERGE_HEAD"]), latest);
    assert!(git(&work, &["status", "--porcelain"]).contains("UU pr-file"));
}
