//! Repository checks of the onboarding wizard for a Forgejo project.
//!
//! The same questions as [`crate::github::check_repo`], asked of the Forgejo
//! API as the onboarding login's account. See `docs/design.md`, "Forgejo", for
//! how each check maps to Forgejo's API.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use factory_runner::{Account, ForgeKind, Forgejo};
use serde::Deserialize;

use crate::github::{RepoChecker, RepoReport};

/// The directories where Forgejo Actions looks for workflows. GitHub's is
/// read too, which Forgejo falls back to.
const WORKFLOW_DIRS: [&str; 2] = [".forgejo/workflows", ".github/workflows"];

#[derive(Clone)]
pub struct ForgejoRepos {
    pub forgejo: Arc<Forgejo>,
    pub kind: ForgeKind,
}

#[derive(Default, Deserialize)]
struct PermissionsJson {
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    push: bool,
}

#[derive(Deserialize)]
struct RepoJson {
    #[serde(default)]
    permissions: PermissionsJson,
    #[serde(default)]
    allow_squash_merge: bool,
    has_pull_requests: Option<bool>,
    has_actions: Option<bool>,
}

#[derive(Deserialize)]
struct BranchJson {
    #[serde(default)]
    protected: bool,
    #[serde(default)]
    user_can_merge: bool,
}

#[derive(Deserialize)]
struct EntryJson {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "type")]
    kind: String,
}

fn parse<T: for<'de> Deserialize<'de>>(value: serde_json::Value, what: &str) -> Result<T, String> {
    serde_json::from_value(value).map_err(|_| format!("{what} returned unexpected output"))
}

impl ForgejoRepos {
    async fn has_workflow(
        &self,
        account: &Account,
        repo: &str,
        base: &str,
    ) -> Result<bool, String> {
        for dir in WORKFLOW_DIRS {
            let Some(listing) = self
                .forgejo
                .read_repo_json(account, repo, &format!("contents/{dir}"), &[("ref", base)])
                .await?
            else {
                continue;
            };
            if !listing.is_array() {
                continue;
            }
            let entries: Vec<EntryJson> = parse(listing, &format!("the {dir} listing"))?;
            if entries.iter().any(|entry| {
                entry.kind == "file"
                    && std::path::Path::new(&entry.name)
                        .extension()
                        .is_some_and(|ext| ext == "yml" || ext == "yaml")
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn check(&self, account: &Account, repo: &str, base: &str) -> Result<RepoReport, String> {
        let mut report = RepoReport::default();
        if !self.kind.valid_repo(repo) {
            report.errors.push(format!(
                "The repository \"{repo}\" is not in owner/name form."
            ));
            return Ok(report);
        }
        let Some(info) = self.forgejo.read_repo_json(account, repo, "", &[]).await? else {
            report.errors.push(format!(
                "The repository {repo} does not exist or is not visible to {}.",
                account.login
            ));
            return Ok(report);
        };
        let info: RepoJson = parse(info, "the repository read")?;
        if !info.permissions.push {
            report
                .errors
                .push(format!("{} has no push access to {repo}.", account.login));
        }
        if !info.allow_squash_merge {
            report.errors.push(format!(
                "{repo} does not allow squash merges, which the factory relies on."
            ));
        }
        if info.has_pull_requests == Some(false) {
            report.errors.push(format!(
                "{repo} has pull requests turned off, which the factory relies on."
            ));
        }
        let branch = if let Some(branch) = self
            .forgejo
            .read_repo_json(account, repo, &format!("branches/{base}"), &[])
            .await?
        {
            Some(parse::<BranchJson>(branch, "the branch read")?)
        } else {
            report.errors.push(format!(
                "The base branch \"{base}\" does not exist in {repo}."
            ));
            None
        };

        if info.permissions.admin {
            report.warnings.push(format!(
                "{} has Admin access to {repo}. The design asks for Write, so a protection rule's Admin bypass does not apply to it.",
                account.login
            ));
        }
        match &branch {
            Some(branch) if !branch.protected => report
                .warnings
                .push(format!("The base branch \"{base}\" has no protection.")),
            Some(branch) if !branch.user_can_merge => report.warnings.push(format!(
                "The protection of the base branch \"{base}\" does not let {} merge.",
                account.login
            )),
            _ => {}
        }
        let ci = info.has_actions != Some(false)
            && (branch.is_none() || self.has_workflow(account, repo, base).await?);
        if !ci {
            report.warnings.push(format!("{repo} has no CI workflow."));
        }
        Ok(report)
    }
}

impl RepoChecker for ForgejoRepos {
    fn check<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        base: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RepoReport, String>> + Send + 'a>> {
        Box::pin(ForgejoRepos::check(self, account, repo, base))
    }
}
