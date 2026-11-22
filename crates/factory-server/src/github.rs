//! Repository checks the onboarding wizard runs before it creates a project.
//!
//! The checks ask GitHub, as the bot, whether a repository is fit for the
//! factory: that it exists and is visible, that its base branch exists, that
//! the bot can push, and that squash merges are allowed. They also report
//! warnings that do not block onboarding. See `docs/design.md`, "GitHub
//! identity", for why the bot holds Write and not Admin.
//!
//! The GitHub calls sit behind [`GitHubRepos`]. [`GhRepos`] runs `gh api` with
//! the factory's token; tests pass a fake, so they need no network. The
//! onboarding wizard holds a [`RepoChecker`], so it runs [`check_repo`] without
//! naming the concrete GitHub type.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;

use serde::Deserialize;
use tokio::process::Command;

/// The GitHub facts the onboarding checks need, each from one `gh api` call.
/// A `Result::Err` means the call itself failed, such as `gh` being missing or
/// GitHub being unreachable, and is distinct from a repository that fails a
/// check.
pub trait GitHubRepos {
    /// The repository `owner/name` as the bot sees it, or `None` when GitHub
    /// returns 404 because the repository is missing or not visible to the bot.
    fn repo(&self, repo: &str) -> impl Future<Output = Result<Option<RepoInfo>, String>> + Send;

    /// The base branch as the bot sees it, or `None` when it does not exist.
    fn branch(
        &self,
        repo: &str,
        branch: &str,
    ) -> impl Future<Output = Result<Option<BranchInfo>, String>> + Send;

    /// Whether the repository has any GitHub Actions workflow, the factory's
    /// proxy for "CI is set up".
    fn has_ci(&self, repo: &str) -> impl Future<Output = Result<bool, String>> + Send;
}

/// The bot's access to a repository, from the `permissions` of
/// `gh api repos/{owner}/{name}`. The levels nest: Admin can push, Write can
/// push, the others cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// No access, or read-only.
    Read,
    /// Push (Write) access, the level the design asks for.
    Write,
    /// Admin access. A ruleset's Admin bypass would exempt the bot from it.
    Admin,
}

impl Permission {
    /// Whether the bot can push to the repository.
    #[must_use]
    pub fn can_push(self) -> bool {
        matches!(self, Permission::Write | Permission::Admin)
    }
}

/// What the bot can do with a repository and how it merges, from
/// `gh api repos/{owner}/{name}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    pub permission: Permission,
    /// The repository allows squash merges, which the factory relies on.
    pub allow_squash_merge: bool,
    /// The repository deletes a branch after its pull request merges.
    pub delete_branch_on_merge: bool,
}

/// The state of the base branch that the checks read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub protection: Protection,
}

/// Whether branch protection or a ruleset applies to the base branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protection {
    Protected,
    Unprotected,
    /// GitHub refused to show the branch's rules, for example with HTTP 403
    /// for a private repository on a free plan. `reason` is GitHub's message.
    Unknown {
        reason: String,
    },
}

/// The outcome of [`check_repo`]: the checks that failed and the warnings that
/// did not block onboarding. An empty `errors` means the project may be
/// created.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Checks whether `repo` with base branch `base` is fit for onboarding.
///
/// The errors each block the project: a malformed `owner/name`, a repository
/// that is missing or not visible to the bot, a base branch that does not
/// exist, no push access for the bot, and squash merges turned off. The
/// warnings do not: the bot holds Admin, the base branch has no protection or
/// ruleset, the repository has no CI, and merged branches are not deleted.
///
/// A malformed repository short-circuits, because its name cannot be sent to
/// GitHub. When the repository is missing, the branch and CI checks are
/// skipped for the same reason.
///
/// # Errors
///
/// Returns the message from [`GitHubRepos`] when a GitHub call fails, so the
/// wizard can tell "the check could not run" from "the repository failed a
/// check".
pub async fn check_repo(
    gh: &impl GitHubRepos,
    repo: &str,
    base: &str,
) -> Result<RepoReport, String> {
    let mut report = RepoReport::default();
    if !well_formed(repo) {
        report.errors.push(format!(
            "The repository \"{repo}\" is not in owner/name form."
        ));
        return Ok(report);
    }
    let Some(info) = gh.repo(repo).await? else {
        report.errors.push(format!(
            "The repository {repo} does not exist or is not visible to the bot."
        ));
        return Ok(report);
    };
    if !info.permission.can_push() {
        report
            .errors
            .push(format!("The bot has no push access to {repo}."));
    }
    if !info.allow_squash_merge {
        report.errors.push(format!(
            "{repo} does not allow squash merges, which the factory relies on."
        ));
    }
    let base_branch = gh.branch(repo, base).await?;
    if base_branch.is_none() {
        report.errors.push(format!(
            "The base branch \"{base}\" does not exist in {repo}."
        ));
    }

    if info.permission == Permission::Admin {
        report.warnings.push(format!(
            "The bot has Admin access to {repo}. The design asks for Write, so a ruleset's Admin bypass does not apply to it."
        ));
    }
    match base_branch.map(|branch| branch.protection) {
        Some(Protection::Unprotected) => report.warnings.push(format!(
            "The base branch \"{base}\" has no protection or ruleset."
        )),
        Some(Protection::Unknown { reason }) => report.warnings.push(format!(
            "The factory could not check whether the base branch \"{base}\" has protection or a ruleset. GitHub answered: {reason}"
        )),
        Some(Protection::Protected) | None => {}
    }
    if !gh.has_ci(repo).await? {
        report.warnings.push(format!("{repo} has no CI workflow."));
    }
    if !info.delete_branch_on_merge {
        report.warnings.push(format!(
            "{repo} does not delete a branch after its pull request merges."
        ));
    }
    Ok(report)
}

/// Runs the onboarding checks behind a trait object, so the dashboard holds one
/// without naming the concrete GitHub type. [`GitHubRepos`] returns
/// `impl Future`, which a trait object cannot, so this wraps [`check_repo`] in a
/// boxed future. Every [`GitHubRepos`] gets it, so the real [`GhRepos`] and a
/// test fake both serve as the dashboard's checker.
pub trait RepoChecker: Send + Sync {
    /// As [`check_repo`].
    fn check<'a>(
        &'a self,
        repo: &'a str,
        base: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RepoReport, String>> + Send + 'a>>;
}

impl<T: GitHubRepos + Send + Sync> RepoChecker for T {
    fn check<'a>(
        &'a self,
        repo: &'a str,
        base: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RepoReport, String>> + Send + 'a>> {
        Box::pin(check_repo(self, repo, base))
    }
}

/// Whether `repo` is `owner/name` with two non-empty parts and only characters
/// GitHub allows in a repository path.
pub(crate) fn well_formed(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !owner.is_empty()
        && !name.is_empty()
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
}

/// Runs the onboarding checks with the `gh` CLI and the factory's token, as
/// the bot.
#[derive(Debug, Clone)]
pub struct GhRepos {
    pub gh: PathBuf,
    pub token: Option<String>,
}

impl Default for GhRepos {
    fn default() -> Self {
        GhRepos {
            gh: PathBuf::from("gh"),
            token: None,
        }
    }
}

#[derive(Deserialize)]
struct RepoJson {
    #[serde(default)]
    permissions: PermissionsJson,
    #[serde(default)]
    allow_squash_merge: bool,
    #[serde(default)]
    delete_branch_on_merge: bool,
}

#[derive(Default, Deserialize)]
struct PermissionsJson {
    #[serde(default)]
    push: bool,
    #[serde(default)]
    admin: bool,
}

#[derive(Deserialize)]
struct BranchJson {
    #[serde(default)]
    protected: bool,
}

#[derive(Deserialize)]
struct WorkflowsJson {
    #[serde(default)]
    total_count: u64,
}

/// The result of one `gh api` call: its body, or that GitHub answered 404.
enum ApiOutcome {
    Found(String),
    NotFound,
    /// HTTP 403, with gh's message.
    Forbidden(String),
}

impl GitHubRepos for GhRepos {
    async fn repo(&self, repo: &str) -> Result<Option<RepoInfo>, String> {
        let body = match self.api(&format!("repos/{repo}")).await? {
            ApiOutcome::Found(body) => body,
            ApiOutcome::NotFound => return Ok(None),
            ApiOutcome::Forbidden(message) => {
                return Err(format!("gh api repos/{repo} failed: {message}"));
            }
        };
        let json: RepoJson =
            serde_json::from_str(&body).map_err(|err| format!("gh api repos/{repo}: {err}"))?;
        let permission = if json.permissions.admin {
            Permission::Admin
        } else if json.permissions.push {
            Permission::Write
        } else {
            Permission::Read
        };
        Ok(Some(RepoInfo {
            permission,
            allow_squash_merge: json.allow_squash_merge,
            delete_branch_on_merge: json.delete_branch_on_merge,
        }))
    }

    async fn branch(&self, repo: &str, branch: &str) -> Result<Option<BranchInfo>, String> {
        let body = match self.api(&format!("repos/{repo}/branches/{branch}")).await? {
            ApiOutcome::Found(body) => body,
            ApiOutcome::NotFound => return Ok(None),
            ApiOutcome::Forbidden(message) => {
                return Err(format!(
                    "gh api repos/{repo}/branches/{branch} failed: {message}"
                ));
            }
        };
        let json: BranchJson = serde_json::from_str(&body)
            .map_err(|err| format!("gh api repos/{repo}/branches/{branch}: {err}"))?;
        if json.protected {
            return Ok(Some(BranchInfo {
                protection: Protection::Protected,
            }));
        }
        // A ruleset protects a branch without setting `protected`.
        let protection = match self
            .api(&format!("repos/{repo}/rules/branches/{branch}"))
            .await?
        {
            ApiOutcome::Found(body) if !body.trim().trim_start_matches('[').trim().is_empty() => {
                Protection::Protected
            }
            ApiOutcome::Found(_) | ApiOutcome::NotFound => Protection::Unprotected,
            ApiOutcome::Forbidden(reason) => Protection::Unknown { reason },
        };
        Ok(Some(BranchInfo { protection }))
    }

    async fn has_ci(&self, repo: &str) -> Result<bool, String> {
        let body = match self.api(&format!("repos/{repo}/actions/workflows")).await? {
            ApiOutcome::Found(body) => body,
            ApiOutcome::NotFound => return Ok(false),
            ApiOutcome::Forbidden(message) => {
                return Err(format!(
                    "gh api repos/{repo}/actions/workflows failed: {message}"
                ));
            }
        };
        let json: WorkflowsJson = serde_json::from_str(&body)
            .map_err(|err| format!("gh api repos/{repo}/actions/workflows: {err}"))?;
        Ok(json.total_count > 0)
    }
}

impl GhRepos {
    /// Runs `gh api <path>`. A 404 is [`ApiOutcome::NotFound`], because the
    /// checks read it as "missing", not as a failure. A 403 is
    /// [`ApiOutcome::Forbidden`], so a check that GitHub withholds on the
    /// repository's plan can become a warning. Any other non-zero exit is an
    /// error with gh's stderr.
    async fn api(&self, path: &str) -> Result<ApiOutcome, String> {
        let mut command = Command::new(&self.gh);
        command
            .args(["api", path])
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(token) = &self.token {
            command.env("GH_TOKEN", token);
        }
        let output = command
            .output()
            .await
            .map_err(|err| format!("could not start {}: {err}", self.gh.display()))?;
        if output.status.success() {
            return Ok(ApiOutcome::Found(
                String::from_utf8_lossy(&output.stdout).into_owned(),
            ));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("HTTP 404") {
            Ok(ApiOutcome::NotFound)
        } else if stderr.contains("HTTP 403") {
            let message = stderr.trim();
            Ok(ApiOutcome::Forbidden(
                message.strip_prefix("gh: ").unwrap_or(message).to_owned(),
            ))
        } else {
            Err(format!("gh api {path} failed: {}", stderr.trim()))
        }
    }
}
