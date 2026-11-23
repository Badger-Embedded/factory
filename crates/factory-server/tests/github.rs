mod support;

use std::future::{Future, ready};

use factory_runner::Account;
use factory_server::{
    BranchInfo, GhRepos, GitHubRepos, Permission, Protection, RepoInfo, check_repo,
};

fn account() -> Account {
    Account {
        token: Some("tok".into()),
        login: "onboarder".into(),
        name: "onboarder".into(),
        email: "1+onboarder@users.noreply.github.com".into(),
    }
}

/// A fake GitHub that answers each call from stored values, so the checks run
/// without the network. `fail` turns every call into an error, to test the
/// path where GitHub itself is unreachable.
#[derive(Clone)]
struct Fake {
    repo: Option<RepoInfo>,
    branch: Option<BranchInfo>,
    ci: bool,
    fail: Option<String>,
}

impl GitHubRepos for Fake {
    fn repo(
        &self,
        _account: &Account,
        _repo: &str,
    ) -> impl Future<Output = Result<Option<RepoInfo>, String>> + Send {
        ready(match &self.fail {
            Some(err) => Err(err.clone()),
            None => Ok(self.repo.clone()),
        })
    }

    fn branch(
        &self,
        _account: &Account,
        _repo: &str,
        _branch: &str,
    ) -> impl Future<Output = Result<Option<BranchInfo>, String>> + Send {
        ready(match &self.fail {
            Some(err) => Err(err.clone()),
            None => Ok(self.branch.clone()),
        })
    }

    fn has_ci(
        &self,
        _account: &Account,
        _repo: &str,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        ready(match &self.fail {
            Some(err) => Err(err.clone()),
            None => Ok(self.ci),
        })
    }
}

/// A repository that passes every check: the bot can push, is not Admin, the
/// repository squash-merges and auto-deletes branches, the base branch exists
/// and is protected, and CI is set up.
fn fit() -> Fake {
    Fake {
        repo: Some(RepoInfo {
            permission: Permission::Write,
            allow_squash_merge: true,
            delete_branch_on_merge: true,
        }),
        branch: Some(BranchInfo {
            protection: Protection::Protected,
        }),
        ci: true,
        fail: None,
    }
}

#[tokio::test]
async fn a_fit_repo_has_no_errors_or_warnings() {
    let report = check_repo(&fit(), &account(), "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[tokio::test]
async fn a_malformed_repo_is_rejected_without_a_call() {
    // The fake would answer, but a malformed name cannot be sent to GitHub, so
    // the check short-circuits on it.
    for repo in ["maestro", "owner/name/extra", "owner/", "/name", "a b/c"] {
        let report = check_repo(&fit(), &account(), repo, "main").await.unwrap();
        assert_eq!(report.errors.len(), 1, "{repo}");
        assert!(report.errors[0].contains("owner/name"), "{repo}");
    }
}

#[tokio::test]
async fn a_missing_repo_is_rejected() {
    let fake = Fake {
        repo: None,
        ..fit()
    };
    let report = check_repo(&fake, &account(), "Badger-Embedded/ghost", "main")
        .await
        .unwrap();
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("does not exist or is not visible"));
    // A missing repository skips the branch and CI checks, so it raises no
    // spurious warning about them.
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[tokio::test]
async fn a_missing_base_branch_is_rejected() {
    let fake = Fake {
        branch: None,
        ..fit()
    };
    let report = check_repo(&fake, &account(), "Badger-Embedded/maestro", "trunk")
        .await
        .unwrap();
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("\"trunk\" does not exist"));
    // A missing branch raises no "no protection" warning.
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[tokio::test]
async fn no_push_access_is_rejected() {
    let mut fake = fit();
    fake.repo.as_mut().unwrap().permission = Permission::Read;
    let report = check_repo(&fake, &account(), "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("no push access"));
}

#[tokio::test]
async fn squash_merge_disabled_is_rejected() {
    let mut fake = fit();
    fake.repo.as_mut().unwrap().allow_squash_merge = false;
    let report = check_repo(&fake, &account(), "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("squash merges"));
}

#[tokio::test]
async fn admin_access_is_only_a_warning() {
    let mut fake = fit();
    fake.repo.as_mut().unwrap().permission = Permission::Admin;
    let report = check_repo(&fake, &account(), "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("Admin"));
}

#[tokio::test]
async fn an_unprotected_base_branch_is_only_a_warning() {
    let fake = Fake {
        branch: Some(BranchInfo {
            protection: Protection::Unprotected,
        }),
        ..fit()
    };
    let report = check_repo(&fake, &account(), "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("no protection or ruleset"));
}

#[tokio::test]
async fn missing_ci_is_only_a_warning() {
    let fake = Fake { ci: false, ..fit() };
    let report = check_repo(&fake, &account(), "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("no CI"));
}

#[tokio::test]
async fn no_branch_auto_delete_is_only_a_warning() {
    let mut fake = fit();
    fake.repo.as_mut().unwrap().delete_branch_on_merge = false;
    let report = check_repo(&fake, &account(), "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("delete a branch"));
}

#[tokio::test]
async fn a_failed_github_call_is_an_error_not_a_report() {
    let fake = Fake {
        fail: Some("gh api repos/Badger-Embedded/maestro failed: HTTP 500".into()),
        ..fit()
    };
    let result = check_repo(&fake, &account(), "Badger-Embedded/maestro", "main").await;
    assert_eq!(
        result,
        Err("gh api repos/Badger-Embedded/maestro failed: HTTP 500".into())
    );
}

#[tokio::test]
async fn unreadable_branch_rules_are_only_a_warning() {
    let fake = Fake {
        branch: Some(BranchInfo {
            protection: Protection::Unknown {
                reason: "Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)".into(),
            },
        }),
        ..fit()
    };
    let report = check_repo(&fake, &account(), "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("could not check whether the base branch \"main\""));
    assert!(report.warnings[0].contains("Upgrade to GitHub Pro"));
}

/// A `gh` stand-in that answers each `gh api <path>` the way GitHub answers
/// for a private repository on a free plan: the rules endpoint returns 403.
/// `forbid` names one more path that returns 403.
fn fake_gh(dir: &std::path::Path, forbid: &str) -> GhRepos {
    let script = format!(
        r#"#!/bin/sh
forbidden() {{
  echo 'gh: Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)' >&2
  exit 1
}}
[ "$2" = "{forbid}" ] && forbidden
case "$2" in
  repos/o/r) echo '{{"permissions":{{"push":true}},"allow_squash_merge":true,"delete_branch_on_merge":true}}' ;;
  repos/o/r/branches/main) echo '{{"protected":false}}' ;;
  repos/o/r/rules/branches/main) forbidden ;;
  repos/o/r/actions/workflows) echo '{{"total_count":1}}' ;;
  *) echo 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
esac
"#
    );
    GhRepos {
        gh: support::executable(dir, "gh", &script),
    }
}

#[tokio::test]
async fn gh_reads_a_forbidden_rules_endpoint_as_unknown_protection() {
    let dir = tempfile::tempdir().unwrap();
    let gh = fake_gh(dir.path(), "");
    let report = check_repo(&gh, &account(), "o/r", "main").await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(
        report.warnings,
        vec![
            "The factory could not check whether the base branch \"main\" has protection or a ruleset. GitHub answered: Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)"
                .to_owned()
        ]
    );
}

#[tokio::test]
async fn gh_still_fails_on_a_forbidden_repository() {
    let dir = tempfile::tempdir().unwrap();
    let gh = fake_gh(dir.path(), "repos/o/r");
    let err = check_repo(&gh, &account(), "o/r", "main")
        .await
        .unwrap_err();
    assert!(
        err.starts_with("gh api repos/o/r failed: Upgrade to GitHub Pro"),
        "{err}"
    );
}

#[tokio::test]
async fn every_gh_call_pins_github_com_and_uses_the_accounts_token() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls");
    let script = format!(
        r#"#!/bin/sh
echo "$GH_TOKEN $*" >> {log}
case "$2" in
  repos/o/r) echo '{{"permissions":{{"push":true}},"allow_squash_merge":true,"delete_branch_on_merge":true}}' ;;
  repos/o/r/branches/main) echo '{{"protected":false}}' ;;
  repos/o/r/rules/branches/main) echo '[{{"type":"x"}}]' ;;
  repos/o/r/actions/workflows) echo '{{"total_count":1}}' ;;
  *) echo 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
esac
"#,
        log = log.display()
    );
    let gh = GhRepos {
        gh: support::executable(dir.path(), "gh", &script),
    };
    check_repo(&gh, &account(), "o/r", "main").await.unwrap();

    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls.lines().count(), 4, "{calls}");
    for line in calls.lines() {
        assert!(line.starts_with("tok api repos/o/r"), "{line}");
        assert!(line.ends_with("--hostname github.com"), "{line}");
    }
}

#[tokio::test]
async fn a_token_in_gh_stderr_never_reaches_the_report_or_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let script = |workflows: &str| {
        format!(
            r#"#!/bin/sh
case "$2" in
  repos/o/r/rules/branches/main) echo "gh: forbidden for $GH_TOKEN (HTTP 403)" >&2; exit 1 ;;
  repos/o/r/actions/workflows) {workflows} ;;
  repos/o/r) echo '{{"permissions":{{"push":true}},"allow_squash_merge":true,"delete_branch_on_merge":true}}' ;;
  repos/o/r/branches/main) echo '{{"protected":false}}' ;;
esac
"#
        )
    };

    let gh = GhRepos {
        gh: support::executable(
            dir.path(),
            "gh-fails",
            &script(r#"echo "gh: boom $GH_TOKEN" >&2; exit 1"#),
        ),
    };
    let err = check_repo(&gh, &account(), "o/r", "main")
        .await
        .unwrap_err();
    assert!(err.contains("boom <redacted>"), "{err}");
    assert!(!err.contains("boom tok"), "{err}");

    let gh = GhRepos {
        gh: support::executable(
            dir.path(),
            "gh-forbids",
            &script(r#"echo '{"total_count":1}'"#),
        ),
    };
    let report = check_repo(&gh, &account(), "o/r", "main").await.unwrap();
    let text = format!("{report:?}");
    assert!(text.contains("forbidden for <redacted>"), "{text}");
    assert!(!text.contains("forbidden for tok"), "{text}");
}
