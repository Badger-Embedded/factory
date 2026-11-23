//! Repository checks of the onboarding wizard for a Salsa project.
//!
//! The same questions as [`crate::github::check_repo`], asked of GitLab through
//! `glab` as the onboarding login's account. See `docs/design.md`, "Salsa and
//! GitLab", for how each check maps to GitLab's API.

use std::future::Future;
use std::pin::Pin;

use factory_runner::{
    Access, Account, Forge, ForgeKind, GitLab, gitlab_encode, gitlab_not_found, gitlab_project,
};
use serde::Deserialize;

use crate::github::{RepoChecker, RepoReport};

/// GitLab's access level of a Maintainer, which the factory treats as the
/// level of an account with [`Access::Admin`].
const MAINTAINER: u32 = 40;
const DEVELOPER: u32 = 30;

#[derive(Debug, Clone)]
pub struct GlabRepos {
    pub gitlab: GitLab,
}

#[derive(Deserialize)]
struct ProjectJson {
    #[serde(default)]
    squash_option: Option<String>,
}

#[derive(Deserialize)]
struct BranchJson {
    #[serde(default)]
    protected: bool,
    #[serde(default)]
    commit: Option<BranchCommitJson>,
}

#[derive(Deserialize)]
struct BranchCommitJson {
    id: String,
}

#[derive(Deserialize)]
struct ProtectedBranchJson {
    name: String,
    #[serde(default)]
    merge_access_levels: Vec<MergeLevelJson>,
}

#[derive(Deserialize)]
struct MergeLevelJson {
    #[serde(default)]
    access_level: Option<u32>,
    #[serde(default)]
    user_id: Option<u64>,
    #[serde(default)]
    group_id: Option<u64>,
}

/// What the base branch's protection says about the account merging.
#[derive(Debug, PartialEq, Eq)]
enum Merge {
    Allowed,
    Denied,
    /// No rule was found, or a rule names a user or group the checks cannot
    /// match to the account.
    Unverified,
}

/// Whether the protected branch `pattern`, which may hold `*` wildcards,
/// matches `branch`.
fn glob_matches(pattern: &str, branch: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == branch;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if branch.len() < first.len() + last.len()
        || !branch.starts_with(first)
        || !branch.ends_with(last)
    {
        return false;
    }
    let mut rest = &branch[first.len()..branch.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    true
}

impl GlabRepos {
    /// A JSON read that is `None` when the resource is missing.
    async fn read<T: for<'de> Deserialize<'de>>(
        &self,
        account: &Account,
        endpoint: &str,
    ) -> Result<Option<T>, String> {
        match self.gitlab.api_get(account, endpoint).await {
            Ok(body) => serde_json::from_str(&body)
                .map(Some)
                .map_err(|_| format!("glab api {endpoint} returned unexpected output")),
            Err(err) if gitlab_not_found(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Whether the protection rules that match the base branch let the
    /// account merge. GitLab lists every rule of the project, and a branch
    /// that matches several, by name or by wildcard, takes the most
    /// permissive one.
    async fn protection_allows_merge(
        &self,
        account: &Account,
        repo: &str,
        base: &str,
        access: Access,
    ) -> Result<Merge, String> {
        let endpoint = format!("{}/protected_branches?per_page=100", gitlab_project(repo));
        let rules = match self
            .read::<Vec<ProtectedBranchJson>>(account, &endpoint)
            .await
        {
            Ok(Some(rules)) => rules,
            Ok(None) => return Ok(Merge::Unverified),
            Err(err) if err.contains("HTTP 403") => return Ok(Merge::Unverified),
            Err(err) => return Err(err),
        };
        let level = if access == Access::Admin {
            MAINTAINER
        } else {
            DEVELOPER
        };
        let mut matched = false;
        let mut unverified = false;
        for rule in rules.iter().filter(|rule| glob_matches(&rule.name, base)) {
            matched = true;
            for allowed in &rule.merge_access_levels {
                if allowed.user_id.is_some() || allowed.group_id.is_some() {
                    // The account's id and groups are not known here.
                    unverified = true;
                } else if allowed
                    .access_level
                    .is_some_and(|needed| needed > 0 && needed <= level)
                {
                    return Ok(Merge::Allowed);
                }
            }
        }
        Ok(if !matched || unverified {
            Merge::Unverified
        } else {
            Merge::Denied
        })
    }

    async fn check_repo(
        &self,
        account: &Account,
        repo: &str,
        base: &str,
    ) -> Result<RepoReport, String> {
        let mut report = RepoReport::default();
        if !ForgeKind::Salsa.valid_repo(repo) {
            report.errors.push(format!(
                "The repository \"{repo}\" is not a Salsa project path such as python-team/packages/foo."
            ));
            return Ok(report);
        }
        let project = gitlab_project(repo);
        let Some(info) = self.read::<ProjectJson>(account, &project).await? else {
            report.errors.push(format!(
                "The project {repo} does not exist or is not visible to {}.",
                account.login
            ));
            return Ok(report);
        };
        let access = self.gitlab.access(account, repo).await?;
        match access {
            Access::NotVisible => {
                report.errors.push(format!(
                    "The project {repo} does not exist or is not visible to {}.",
                    account.login
                ));
                return Ok(report);
            }
            Access::Read => report.errors.push(format!(
                "{} has no push access to {repo}. The account needs the Developer role.",
                account.login
            )),
            Access::Write => {}
            Access::Admin => report.warnings.push(format!(
                "{} is a Maintainer or Owner of {repo}. The design asks for Developer, so branch protection that exempts Maintainers does not apply to it.",
                account.login
            )),
        }
        if info.squash_option.as_deref() == Some("never") {
            report.errors.push(format!(
                "{repo} does not allow squash merges, which the factory relies on. Set the squash option to Allow, Encourage or Require."
            ));
        }

        let branch_endpoint = format!("{project}/repository/branches/{}", gitlab_encode(base));
        let Some(branch) = self.read::<BranchJson>(account, &branch_endpoint).await? else {
            report.errors.push(format!(
                "The base branch \"{base}\" does not exist in {repo}."
            ));
            return Ok(report);
        };
        if branch.protected {
            if self
                .protection_allows_merge(account, repo, base, access)
                .await?
                == Merge::Denied
            {
                report.warnings.push(format!(
                    "The protection of the base branch \"{base}\" does not let {} merge, so auto-merge will not work for this account.",
                    account.login
                ));
            }
        } else {
            report
                .warnings
                .push(format!("The base branch \"{base}\" is not protected."));
        }
        // A pipeline of the same commit on another ref, such as a feature
        // branch or a tag, is not CI of the base branch.
        let has_pipeline = match &branch.commit {
            Some(commit) => {
                let endpoint = format!(
                    "{project}/pipelines?ref={}&sha={}&per_page=1",
                    gitlab_encode(base),
                    gitlab_encode(&commit.id)
                );
                self.read::<Vec<serde_json::Value>>(account, &endpoint)
                    .await?
                    .is_some_and(|pipelines| !pipelines.is_empty())
            }
            None => false,
        };
        if !has_pipeline {
            report.warnings.push(format!(
                "The head of the base branch \"{base}\" in {repo} has no pipeline, so the factory has no CI to wait for."
            ));
        }
        Ok(report)
    }
}

impl RepoChecker for GlabRepos {
    fn check<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        base: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RepoReport, String>> + Send + 'a>> {
        Box::pin(self.check_repo(account, repo, base))
    }
}
