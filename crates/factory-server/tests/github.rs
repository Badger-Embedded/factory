use std::future::{Future, ready};

use factory_server::{BranchInfo, GitHubRepos, Permission, RepoInfo, check_repo};

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
    fn repo(&self, _repo: &str) -> impl Future<Output = Result<Option<RepoInfo>, String>> + Send {
        ready(match &self.fail {
            Some(err) => Err(err.clone()),
            None => Ok(self.repo.clone()),
        })
    }

    fn branch(
        &self,
        _repo: &str,
        _branch: &str,
    ) -> impl Future<Output = Result<Option<BranchInfo>, String>> + Send {
        ready(match &self.fail {
            Some(err) => Err(err.clone()),
            None => Ok(self.branch.clone()),
        })
    }

    fn has_ci(&self, _repo: &str) -> impl Future<Output = Result<bool, String>> + Send {
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
        branch: Some(BranchInfo { protected: true }),
        ci: true,
        fail: None,
    }
}

#[tokio::test]
async fn a_fit_repo_has_no_errors_or_warnings() {
    let report = check_repo(&fit(), "Badger-Embedded/maestro", "main")
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
        let report = check_repo(&fit(), repo, "main").await.unwrap();
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
    let report = check_repo(&fake, "Badger-Embedded/ghost", "main")
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
    let report = check_repo(&fake, "Badger-Embedded/maestro", "trunk")
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
    let report = check_repo(&fake, "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("no push access"));
}

#[tokio::test]
async fn squash_merge_disabled_is_rejected() {
    let mut fake = fit();
    fake.repo.as_mut().unwrap().allow_squash_merge = false;
    let report = check_repo(&fake, "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("squash merges"));
}

#[tokio::test]
async fn admin_access_is_only_a_warning() {
    let mut fake = fit();
    fake.repo.as_mut().unwrap().permission = Permission::Admin;
    let report = check_repo(&fake, "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("Admin"));
}

#[tokio::test]
async fn an_unprotected_base_branch_is_only_a_warning() {
    let fake = Fake {
        branch: Some(BranchInfo { protected: false }),
        ..fit()
    };
    let report = check_repo(&fake, "Badger-Embedded/maestro", "main")
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("no protection or ruleset"));
}

#[tokio::test]
async fn missing_ci_is_only_a_warning() {
    let fake = Fake { ci: false, ..fit() };
    let report = check_repo(&fake, "Badger-Embedded/maestro", "main")
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
    let report = check_repo(&fake, "Badger-Embedded/maestro", "main")
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
    let result = check_repo(&fake, "Badger-Embedded/maestro", "main").await;
    assert_eq!(
        result,
        Err("gh api repos/Badger-Embedded/maestro failed: HTTP 500".into())
    );
}
