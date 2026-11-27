use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use factory_core::PrDescription;

use crate::forge::{
    Access, Account, BoxFuture, Forge, ForgeAccount, LookupError, PrQuery, SEARCH_PAGE,
    command_error,
};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub(crate) const MAX_LOG: usize = 30_000;
/// The largest diff `pr_details` returns. A larger one is an error,
/// because a review of a cut diff could approve changes it never saw.
pub(crate) const MAX_DIFF: usize = 200_000;
/// The most changed files GitHub's compare API lists.
const COMPARE_FILE_LIMIT: usize = 300;

/// Talks to GitHub through the `gh` CLI as the account each call names.
#[derive(Debug, Clone)]
pub struct GitHub {
    pub gh: PathBuf,
    /// How many times `rerun_failed` reads the pull request while
    /// it waits for GitHub to stop reporting the old failure.
    pub settle_polls: u32,
    pub settle_interval: Duration,
    /// Where a call with no token goes, because `gh` refuses to run without
    /// one. Tests point it at a local server.
    pub api_base: String,
}

impl Default for GitHub {
    fn default() -> Self {
        GitHub {
            gh: PathBuf::from("gh"),
            settle_polls: 10,
            settle_interval: Duration::from_secs(3),
            api_base: "https://api.github.com".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// What `rerun_failed` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rerun {
    /// The IDs of the Actions runs it restarted.
    Started(Vec<u64>),
    /// The head commit has no failed Actions run, such as when the failed
    /// check comes from outside Actions.
    NothingToRerun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck {
    pub name: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ci {
    /// No check reported yet, such as in the seconds after a push before
    /// GitHub starts the workflows, or in a repository without CI.
    NoChecks,
    Pending,
    Passed,
    Failed(Vec<FailedCheck>),
}

/// A comment or review on the pull request by someone other than the bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub author: String,
    pub body: String,
    /// GitHub's UTC timestamp, such as `2026-11-19T22:40:45Z`.
    pub at: String,
    /// The file and line of an inline review comment.
    pub location: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStatus {
    pub state: PrState,
    /// GitHub reports that the pull request does not merge cleanly into its
    /// base. GitHub works this out after each push, and while it does, the
    /// pull request does not count as conflicting.
    pub conflicting: bool,
    pub ci: Ci,
    pub comments: Vec<Comment>,
    /// The SHA of the pull request's head commit.
    pub head: String,
}

/// What the factory tells the job after looking at its pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    Merged,
    Closed,
    Conflicted,
    CiFailed(Vec<FailedCheck>),
    /// Comments newer than the watermark, and the new watermark.
    Comments {
        text: String,
        seen_until: String,
    },
    /// Open, every check passed and no comment is waiting for the agent.
    Ready,
    Nothing,
}

/// An open pull request that Renovate made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenovatePr {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// The SHA of the pull request's head commit.
    pub head: String,
}

/// A row of the open pull request search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPr {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head_branch: String,
    pub base_branch: String,
    /// The head branch is in another repository than the base branch.
    pub fork: bool,
    pub draft: bool,
    pub url: String,
}

/// A pull request in any state, as `pull_request` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub state: PrState,
    pub draft: bool,
    /// The head branch is in another repository than the base branch.
    pub fork: bool,
    /// Where the head repository can be cloned. `None` when the forge does
    /// not say, as when the fork was deleted.
    pub head_repo_clone_url: Option<String>,
    pub head_branch: String,
    /// The SHA of the head commit.
    pub head_sha: String,
    pub base_branch: String,
    pub title: String,
    pub body: String,
    pub author: String,
    pub url: String,
}

/// A pull request without its diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrInfo {
    pub title: String,
    pub body: String,
    /// The SHA of the pull request's head commit.
    pub head: String,
    pub files: Vec<String>,
}

/// The head commit of a pull request and what GitHub says about when it
/// became the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadCommit {
    pub sha: String,
    /// UTC, as `YYYY-MM-DD HH:MM:SS`, which compares with SQLite's
    /// `CURRENT_TIMESTAMP` as text. When GitHub recorded the force push that
    /// made the commit the head, or `None` when the timeline has no such
    /// event, as after a plain push, which GitHub does not date.
    pub pushed_at: Option<String>,
    /// When the commit was made. The commit was not the head before this, but
    /// may have become the head long after.
    pub committed_at: Option<String>,
}

/// What an agent needs to judge a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrDetails {
    pub title: String,
    pub body: String,
    /// The SHA of the pull request's head commit.
    pub head: String,
    pub files: Vec<String>,
    /// The complete diff.
    pub diff: String,
}

#[derive(Deserialize)]
struct PullJson {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    html_url: String,
    user: Option<Login>,
    head: HeadJson,
}

#[derive(Deserialize)]
struct HeadJson {
    sha: String,
}

#[derive(Deserialize)]
struct FullPullJson {
    number: u64,
    #[serde(default)]
    title: String,
    body: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    html_url: String,
    user: Option<Login>,
    head: FullRef,
    base: FullRef,
}

#[derive(Deserialize)]
struct FullRef {
    #[serde(default)]
    sha: String,
    #[serde(default, rename = "ref")]
    name: String,
    repo: Option<RepoJson>,
}

#[derive(Deserialize)]
struct RepoJson {
    #[serde(default)]
    full_name: String,
    clone_url: Option<String>,
}

impl FullPullJson {
    fn is_fork(&self) -> bool {
        match (&self.head.repo, &self.base.repo) {
            (Some(head), Some(base)) => !head.full_name.eq_ignore_ascii_case(&base.full_name),
            _ => true,
        }
    }

    fn author(&self) -> String {
        self.user
            .as_ref()
            .map_or_else(|| "ghost".to_owned(), |user| user.login.clone())
    }

    fn into_open(self) -> OpenPr {
        OpenPr {
            fork: self.is_fork(),
            author: self.author(),
            number: self.number,
            title: self.title,
            head_branch: self.head.name,
            base_branch: self.base.name,
            draft: self.draft,
            url: self.html_url,
        }
    }

    fn into_pull_request(self) -> PullRequest {
        let state = if self.merged {
            PrState::Merged
        } else if self.state == "open" {
            PrState::Open
        } else {
            PrState::Closed
        };
        PullRequest {
            number: self.number,
            state,
            draft: self.draft,
            fork: self.is_fork(),
            author: self.author(),
            head_repo_clone_url: self.head.repo.and_then(|repo| repo.clone_url),
            head_branch: self.head.name,
            head_sha: self.head.sha,
            base_branch: self.base.name,
            title: self.title,
            body: self.body.unwrap_or_default(),
            url: self.html_url,
        }
    }
}

/// A node of the GraphQL pull request search. A node that is not a pull
/// request is an empty object.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchNode {
    number: Option<u64>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    is_cross_repository: bool,
    #[serde(default)]
    head_ref_name: String,
    #[serde(default)]
    base_ref_name: String,
    author: Option<Login>,
}

/// GitHub's `2026-01-02T03:04:05Z` as `2026-01-02 03:04:05`.
fn sql_time(github: &str) -> String {
    github.trim_end_matches('Z').replace('T', " ")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsJson {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    head_ref_oid: String,
    #[serde(default)]
    files: Vec<FileJson>,
}

#[derive(Deserialize)]
struct FileJson {
    path: String,
}

#[derive(Deserialize)]
struct ContentJson {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    content: String,
    encoding: Option<String>,
}

#[derive(Deserialize)]
struct CompareJson {
    #[serde(default)]
    files: Vec<CompareFileJson>,
}

#[derive(Deserialize)]
struct CompareFileJson {
    filename: String,
    patch: Option<String>,
}

/// Whether the REST API's `user.login` belongs to Renovate. The REST API
/// reports the app as `renovate[bot]`, while `gh pr list` reports it as
/// `app/renovate`.
#[must_use]
pub fn is_renovate_login(login: &str) -> bool {
    login == "renovate[bot]" || login == "renovate"
}

/// Parses the pull requests API output, one JSON object per line, and keeps
/// the ones Renovate opened.
///
/// # Errors
///
/// Fails when a line does not parse.
pub fn parse_renovate_prs(lines: &str) -> Result<Vec<RenovatePr>, String> {
    let mut prs = Vec::new();
    for line in lines.lines().filter(|line| !line.trim().is_empty()) {
        let pull: PullJson =
            serde_json::from_str(line).map_err(|err| format!("pull request list: {err}"))?;
        if pull.user.is_some_and(|user| is_renovate_login(&user.login)) {
            prs.push(RenovatePr {
                number: pull.number,
                title: pull.title,
                url: pull.html_url,
                head: pull.head.sha,
            });
        }
    }
    Ok(prs)
}

fn is_not_found(err: &str) -> bool {
    err.contains("HTTP 404")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewJson {
    state: String,
    #[serde(default)]
    head_ref_oid: String,
    #[serde(default)]
    mergeable: String,
    #[serde(default)]
    status_check_rollup: Vec<CheckJson>,
    #[serde(default)]
    comments: Vec<IssueCommentJson>,
    #[serde(default)]
    reviews: Vec<ReviewJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckJson {
    name: Option<String>,
    context: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    state: Option<String>,
    details_url: Option<String>,
    target_url: Option<String>,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueCommentJson {
    author: Option<Login>,
    body: String,
    created_at: String,
}

#[derive(Deserialize)]
struct PostedJson {
    login: Option<String>,
    body: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewJson {
    author: Option<Login>,
    #[serde(default)]
    body: String,
    submitted_at: Option<String>,
}

#[derive(Deserialize)]
struct InlineCommentJson {
    user: Option<Login>,
    body: String,
    path: Option<String>,
    line: Option<u64>,
    created_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunJson {
    database_id: u64,
    #[serde(default)]
    conclusion: String,
}

const FAILED_CONCLUSIONS: [&str; 5] = [
    "FAILURE",
    "TIMED_OUT",
    "CANCELLED",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
];

/// Parses `gh pr view --json state,mergeable,statusCheckRollup,comments,reviews` and
/// the review comments API as one JSON object per line. Comments by
/// a login in `ignore` or by GitHub apps are left out.
///
/// # Errors
///
/// Fails when either JSON does not parse.
pub fn parse_status(view: &str, inline: &str, ignore: &[String]) -> Result<PrStatus, String> {
    let view: ViewJson =
        serde_json::from_str(view).map_err(|err| format!("gh pr view output: {err}"))?;
    let inline: Vec<InlineCommentJson> = inline
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|err| format!("review comments: {err}"))?;

    let state = match view.state.as_str() {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Open,
    };

    let mut failed = Vec::new();
    let mut pending = false;
    let any_checks = !view.status_check_rollup.is_empty();
    for check in view.status_check_rollup {
        let name = check.name.or(check.context).unwrap_or_default();
        let url = check.details_url.or(check.target_url);
        if let Some(state) = check.state {
            match state.as_str() {
                "FAILURE" | "ERROR" => failed.push(FailedCheck { name, url }),
                "PENDING" | "EXPECTED" => pending = true,
                _ => {}
            }
        } else if check.status.as_deref() != Some("COMPLETED") {
            pending = true;
        } else if FAILED_CONCLUSIONS.contains(&check.conclusion.as_deref().unwrap_or_default()) {
            failed.push(FailedCheck { name, url });
        }
    }
    let ci = if !failed.is_empty() {
        Ci::Failed(failed)
    } else if pending {
        Ci::Pending
    } else if any_checks {
        Ci::Passed
    } else {
        Ci::NoChecks
    };

    let human = |login: &Option<Login>| -> Option<String> {
        let login = login.as_ref()?.login.clone();
        (!ignore.contains(&login) && !login.ends_with("[bot]")).then_some(login)
    };
    let mut comments = Vec::new();
    for comment in view.comments {
        if let Some(author) = human(&comment.author) {
            comments.push(Comment {
                author,
                body: comment.body,
                at: comment.created_at,
                location: None,
            });
        }
    }
    for review in view.reviews {
        if let (Some(author), Some(at)) = (human(&review.author), review.submitted_at)
            && !review.body.trim().is_empty()
        {
            comments.push(Comment {
                author,
                body: review.body,
                at,
                location: None,
            });
        }
    }
    for comment in inline {
        if let Some(author) = human(&comment.user) {
            comments.push(Comment {
                author,
                body: comment.body,
                at: comment.created_at,
                location: comment.path.map(|path| match comment.line {
                    Some(line) => format!("{path}:{line}"),
                    None => path,
                }),
            });
        }
    }
    comments.sort_by(|a, b| a.at.cmp(&b.at));
    Ok(PrStatus {
        state,
        conflicting: view.mergeable == "CONFLICTING",
        ci,
        comments,
        head: view.head_ref_oid,
    })
}

/// Decides what to tell the job. A merge or close comes first, then a merge
/// conflict, because GitHub does not run pull request CI on a conflicting
/// pull request, then a CI failure, then comments newer than `seen_until`. Without any of those, a
/// pull request whose checks all passed is [`Observation::Ready`].
#[must_use]
pub fn observe(status: &PrStatus, seen_until: Option<&str>) -> Observation {
    match status.state {
        PrState::Merged => return Observation::Merged,
        PrState::Closed => return Observation::Closed,
        PrState::Open => {}
    }
    if status.conflicting {
        return Observation::Conflicted;
    }
    if let Ci::Failed(checks) = &status.ci {
        return Observation::CiFailed(checks.clone());
    }
    let new: Vec<&Comment> = status
        .comments
        .iter()
        .filter(|comment| seen_until.is_none_or(|seen| comment.at.as_str() > seen))
        .collect();
    let Some(last) = new.last() else {
        return if status.ci == Ci::Passed {
            Observation::Ready
        } else {
            Observation::Nothing
        };
    };
    let mut text = String::new();
    for comment in &new {
        let place = comment
            .location
            .as_ref()
            .map_or(String::new(), |location| format!(" on {location}"));
        let _ = write!(
            text,
            "{}{place}:\n{}\n\n",
            comment.author,
            comment.body.trim()
        );
    }
    Observation::Comments {
        text: text.trim_end().to_owned(),
        seen_until: last.at.clone(),
    }
}

/// The job ID in a GitHub Actions job URL, such as
/// `https://github.com/o/r/actions/runs/1/job/2`.
#[must_use]
pub fn actions_job_id(url: &str) -> Option<&str> {
    let (_, job) = url.split_once("/job/")?;
    let job = job.split(['/', '?', '#']).next()?;
    (!job.is_empty() && job.bytes().all(|b| b.is_ascii_digit())).then_some(job)
}

impl GitHub {
    /// The number of the merged pull request for `branch`, if one exists.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails.
    async fn merged_pr(
        &self,
        account: &Account,
        repo: &str,
        branch: &str,
    ) -> Result<Option<u64>, String> {
        let merged = self
            .run(
                account,
                &[
                    "pr",
                    "list",
                    "--repo",
                    repo,
                    "--head",
                    branch,
                    "--state",
                    "merged",
                    "--json",
                    "number",
                    "--jq",
                    ".[0].number",
                ],
                None,
            )
            .await?;
        Ok(merged.trim().parse().ok())
    }

    /// Opens the pull request for `branch`, or updates the title and
    /// description of the open one, and returns its number.
    ///
    /// When the open pull request's checks had failed before the edit, the
    /// failed Actions runs on its head run again afterwards, so they read
    /// the new description. No round is counted. A failed rerun is logged,
    /// because the edit itself worked.
    ///
    /// # Errors
    ///
    /// Returns gh's message when a call fails.
    async fn open_pr(
        &self,
        account: &Account,
        repo: &str,
        base: &str,
        branch: &str,
        description: &PrDescription,
    ) -> Result<u64, String> {
        let existing = self
            .run(
                account,
                &[
                    "pr",
                    "list",
                    "--repo",
                    repo,
                    "--head",
                    branch,
                    "--state",
                    "open",
                    "--json",
                    "number",
                    "--jq",
                    ".[0].number",
                ],
                None,
            )
            .await?;
        if let Ok(number) = existing.trim().parse::<u64>() {
            let number_text = number.to_string();
            let was_failed = match self.status(account, repo, number, &[]).await {
                Ok(status) => matches!(status.ci, Ci::Failed(_)),
                Err(err) => {
                    tracing::error!("reading the checks of pull request {number}: {err}");
                    false
                }
            };
            self.run(
                account,
                &[
                    "pr",
                    "edit",
                    &number_text,
                    "--repo",
                    repo,
                    "--title",
                    &description.title,
                    "--body-file",
                    "-",
                ],
                Some(&description.body),
            )
            .await?;
            if was_failed {
                match self.rerun_failed(account, repo, number).await {
                    Ok(Rerun::Started(_)) => {}
                    Ok(Rerun::NothingToRerun) => {
                        tracing::error!("pull request {number} has no failed run to rerun");
                    }
                    Err(err) => tracing::error!("rerunning pull request {number}: {err}"),
                }
            }
            return Ok(number);
        }
        let url = self
            .run(
                account,
                &[
                    "pr",
                    "create",
                    "--repo",
                    repo,
                    "--base",
                    base,
                    "--head",
                    branch,
                    "--title",
                    &description.title,
                    "--body-file",
                    "-",
                ],
                Some(&description.body),
            )
            .await?;
        url.trim()
            .rsplit('/')
            .next()
            .and_then(|tail| tail.parse().ok())
            .ok_or_else(|| format!("gh pr create printed no pull request URL: {url}"))
    }

    /// Runs `gh run rerun --failed` for each failed Actions run on the head
    /// commit of the pull request.
    ///
    /// GitHub keeps reporting the old failure for a moment after a rerun.
    /// A watch in that moment would read it as a new failure and count a
    /// second round. So after the reruns this reads the pull request until
    /// no check is failed, up to `settle_polls` times.
    ///
    /// # Errors
    ///
    /// Returns gh's message when a call fails or its output does not parse.
    async fn rerun_failed(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<Rerun, String> {
        let number_text = number.to_string();
        let head = self
            .run(
                account,
                &[
                    "pr",
                    "view",
                    &number_text,
                    "--repo",
                    repo,
                    "--json",
                    "headRefOid",
                    "--jq",
                    ".headRefOid",
                ],
                None,
            )
            .await?;
        let runs = self
            .run(
                account,
                &[
                    "run",
                    "list",
                    "--repo",
                    repo,
                    "--commit",
                    head.trim(),
                    "--limit",
                    "100",
                    "--json",
                    "databaseId,conclusion",
                ],
                None,
            )
            .await?;
        let runs: Vec<RunJson> =
            serde_json::from_str(&runs).map_err(|err| format!("gh run list output: {err}"))?;
        let failed: Vec<u64> = runs
            .iter()
            .filter(|run| FAILED_CONCLUSIONS.contains(&run.conclusion.to_uppercase().as_str()))
            .map(|run| run.database_id)
            .collect();
        if failed.is_empty() {
            return Ok(Rerun::NothingToRerun);
        }
        for id in &failed {
            self.run(
                account,
                &["run", "rerun", &id.to_string(), "--repo", repo, "--failed"],
                None,
            )
            .await?;
        }
        for _ in 0..self.settle_polls {
            if !matches!(
                self.status(account, repo, number, &[]).await?.ci,
                Ci::Failed(_)
            ) {
                break;
            }
            tokio::time::sleep(self.settle_interval).await;
        }
        Ok(Rerun::Started(failed))
    }

    /// Squash-merges the pull request and deletes its branch. GitHub builds
    /// the commit message from the repository's squash merge settings.
    ///
    /// # Errors
    ///
    /// Returns gh's message, such as when a ruleset blocks the merge.
    async fn merge(&self, account: &Account, repo: &str, number: u64) -> Result<(), String> {
        let number_text = number.to_string();
        self.run(
            account,
            &[
                "pr",
                "merge",
                &number_text,
                "--repo",
                repo,
                "--squash",
                "--delete-branch",
            ],
            None,
        )
        .await
        .map(|_| ())
    }

    /// The open pull requests that Renovate made against `base`.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails or its output does not parse.
    async fn renovate_prs(
        &self,
        account: &Account,
        repo: &str,
        base: &str,
    ) -> Result<Vec<RenovatePr>, String> {
        let base_field = format!("base={base}");
        let lines = self
            .run(
                account,
                &[
                    "api",
                    "--paginate",
                    "--method",
                    "GET",
                    &format!("repos/{repo}/pulls"),
                    "-f",
                    "state=open",
                    "-f",
                    &base_field,
                    "-f",
                    "per_page=100",
                    "--jq",
                    ".[]",
                ],
                None,
            )
            .await?;
        parse_renovate_prs(&lines)
    }

    /// One page of the open pull requests, see [`Forge::search_open_prs`].
    /// The listing is one REST call, a number is one REST call and a title
    /// search is one GraphQL call, because the search API returns no
    /// branches. Each word of a title search is quoted, so a word such as
    /// `repo:other/name` is text and not a qualifier.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails or its output does not parse.
    async fn search_open_prs(
        &self,
        account: &Account,
        repo: &str,
        query: &str,
    ) -> Result<Vec<OpenPr>, String> {
        match PrQuery::parse(query) {
            PrQuery::All => {
                let out = self
                    .run(
                        account,
                        &[
                            "api",
                            "--method",
                            "GET",
                            &format!("repos/{repo}/pulls"),
                            "-f",
                            "state=open",
                            "-f",
                            "sort=updated",
                            "-f",
                            "direction=desc",
                            "-f",
                            &format!("per_page={SEARCH_PAGE}"),
                        ],
                        None,
                    )
                    .await?;
                let pulls: Vec<FullPullJson> = serde_json::from_str(&out)
                    .map_err(|_| "gh api pulls returned unexpected output".to_owned())?;
                Ok(pulls.into_iter().map(FullPullJson::into_open).collect())
            }
            PrQuery::Number(number) => Ok(self
                .pull_request(account, repo, number)
                .await?
                .filter(|pr| pr.state == PrState::Open)
                .map(|pr| OpenPr {
                    number: pr.number,
                    title: pr.title,
                    author: pr.author,
                    head_branch: pr.head_branch,
                    base_branch: pr.base_branch,
                    fork: pr.fork,
                    draft: pr.draft,
                    url: pr.url,
                })
                .into_iter()
                .collect()),
            PrQuery::Title(text) => {
                let words: Vec<String> = text
                    .split_whitespace()
                    .map(|word| format!("\"{}\"", word.replace(['"', '\\'], "")))
                    .collect();
                let search = format!(
                    "repo:{repo} is:pr is:open sort:updated-desc in:title {}",
                    words.join(" ")
                );
                let graphql = "query($q:String!,$n:Int!){search(query:$q,type:ISSUE,first:$n){nodes{... on PullRequest{number title url isDraft isCrossRepository headRefName baseRefName author{login}}}}}";
                let out = self
                    .run(
                        account,
                        &[
                            "api",
                            "graphql",
                            "-f",
                            &format!("query={graphql}"),
                            "-f",
                            &format!("q={search}"),
                            "-F",
                            &format!("n={SEARCH_PAGE}"),
                            "--jq",
                            ".data.search.nodes",
                        ],
                        None,
                    )
                    .await?;
                let nodes: Vec<SearchNode> = serde_json::from_str(&out)
                    .map_err(|_| "gh api graphql returned unexpected output".to_owned())?;
                Ok(nodes
                    .into_iter()
                    .filter_map(|node| {
                        Some(OpenPr {
                            number: node.number?,
                            title: node.title,
                            author: node
                                .author
                                .map_or_else(|| "ghost".to_owned(), |user| user.login),
                            head_branch: node.head_ref_name,
                            base_branch: node.base_ref_name,
                            fork: node.is_cross_repository,
                            draft: node.is_draft,
                            url: node.url,
                        })
                    })
                    .collect())
            }
        }
    }

    /// A pull request in any state, or `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails or its output does not parse.
    async fn pull_request(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<Option<PullRequest>, String> {
        let out = match self
            .run(
                account,
                &["api", &format!("repos/{repo}/pulls/{number}")],
                None,
            )
            .await
        {
            Ok(out) => out,
            Err(err) if is_not_found(&err) => return Ok(None),
            Err(err) => return Err(err),
        };
        let pull: FullPullJson = serde_json::from_str(&out)
            .map_err(|_| "gh api pull request returned unexpected output".to_owned())?;
        Ok(Some(pull.into_pull_request()))
    }

    /// The title, description, head and changed files of a pull request,
    /// without its diff.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails or its output does not parse.
    async fn pr_info(&self, account: &Account, repo: &str, number: u64) -> Result<PrInfo, String> {
        let view = self
            .run(
                account,
                &[
                    "pr",
                    "view",
                    &number.to_string(),
                    "--repo",
                    repo,
                    "--json",
                    "title,body,headRefOid,files",
                ],
                None,
            )
            .await?;
        let view: DetailsJson =
            serde_json::from_str(&view).map_err(|err| format!("gh pr view output: {err}"))?;
        Ok(PrInfo {
            title: view.title,
            body: view.body,
            head: view.head_ref_oid,
            files: view.files.into_iter().map(|file| file.path).collect(),
        })
    }

    /// The head commit of a pull request, when it was made, and when GitHub
    /// recorded the force push that made it the head, if it did.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails, its output does not parse, or
    /// the timeline says nothing about the head.
    async fn head_commit(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<HeadCommit, String> {
        #[derive(Deserialize)]
        struct Oid {
            oid: String,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Commit {
            oid: String,
            committed_date: String,
        }
        #[derive(Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            HeadRefForcePushedEvent {
                #[serde(rename = "createdAt")]
                created_at: String,
                #[serde(rename = "afterCommit")]
                after: Option<Oid>,
            },
            PullRequestCommit {
                commit: Commit,
            },
        }
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<Node>,
        }
        #[derive(Deserialize)]
        struct Pr {
            #[serde(rename = "headRefOid")]
            head: String,
            #[serde(rename = "timelineItems")]
            timeline: Nodes,
        }
        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| format!("{repo} is not owner/name"))?;
        let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid timelineItems(last:100,itemTypes:[HEAD_REF_FORCE_PUSHED_EVENT,PULL_REQUEST_COMMIT]){nodes{__typename ... on HeadRefForcePushedEvent{createdAt afterCommit{oid}} ... on PullRequestCommit{commit{oid committedDate}}}}}}}";
        let out = self
            .run(
                account,
                &[
                    "api",
                    "graphql",
                    "-f",
                    &format!("query={query}"),
                    "-f",
                    &format!("owner={owner}"),
                    "-f",
                    &format!("name={name}"),
                    "-F",
                    &format!("number={number}"),
                    "--jq",
                    ".data.repository.pullRequest",
                ],
                None,
            )
            .await?;
        let pr: Pr = serde_json::from_str(&out)
            .map_err(|_| "gh api graphql returned unexpected output".to_owned())?;
        let mut pushed_at = None;
        let mut committed_at = None;
        for node in &pr.timeline.nodes {
            match node {
                Node::HeadRefForcePushedEvent {
                    created_at,
                    after: Some(after),
                } if after.oid == pr.head => pushed_at = Some(sql_time(created_at)),
                Node::PullRequestCommit { commit } if commit.oid == pr.head => {
                    committed_at = Some(sql_time(&commit.committed_date));
                }
                _ => {}
            }
        }
        Ok(HeadCommit {
            sha: pr.head,
            pushed_at,
            committed_at,
        })
    }

    /// When the pull request merged, as `YYYY-MM-DD HH:MM:SS` in UTC, or
    /// `None` when GitHub shows it unmerged.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails or its output does not parse.
    async fn merged_at(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<Option<String>, String> {
        #[derive(Deserialize)]
        struct View {
            #[serde(rename = "mergedAt")]
            merged_at: Option<String>,
        }
        let out = self
            .run(
                account,
                &[
                    "pr",
                    "view",
                    &number.to_string(),
                    "--repo",
                    repo,
                    "--json",
                    "mergedAt",
                ],
                None,
            )
            .await?;
        let view: View = serde_json::from_str(&out)
            .map_err(|_| "gh pr view returned unexpected output".to_owned())?;
        Ok(view.merged_at.as_deref().map(sql_time))
    }

    /// The title, description, head, changed files and diff of a pull
    /// request.
    ///
    /// # Errors
    ///
    /// Returns gh's message when a call fails or its output does not parse.
    async fn pr_details(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<PrDetails, String> {
        let info = self.pr_info(account, repo, number).await?;
        let diff = self
            .run(
                account,
                &["pr", "diff", &number.to_string(), "--repo", repo],
                None,
            )
            .await?;
        if diff.len() > MAX_DIFF {
            return Err(format!(
                "the diff of pull request {number} is {} bytes, over the limit of {MAX_DIFF}, so it cannot be reviewed in full",
                diff.len()
            ));
        }
        Ok(PrDetails {
            title: info.title,
            body: info.body,
            head: info.head,
            files: info.files,
            diff,
        })
    }

    /// Posts a comment on the pull request.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails.
    async fn comment(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<(), String> {
        let number_text = number.to_string();
        self.run(
            account,
            &[
                "pr",
                "comment",
                &number_text,
                "--repo",
                repo,
                "--body-file",
                "-",
            ],
            Some(body),
        )
        .await
        .map(|_| ())
    }

    /// Whether the account already posted `body` as a plain comment of the
    /// pull request. It reads every page of the comments.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails, or when its output does not
    /// parse.
    async fn has_comment(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<bool, String> {
        let lines = self
            .run(
                account,
                &[
                    "api",
                    "--paginate",
                    &format!("repos/{repo}/issues/{number}/comments?per_page=100"),
                    "--jq",
                    ".[] | {login: .user.login, body: .body}",
                ],
                None,
            )
            .await?;
        for line in lines.lines().filter(|line| !line.trim().is_empty()) {
            let comment: PostedJson =
                serde_json::from_str(line).map_err(|err| format!("comments: {err}"))?;
            if crate::forge::is_posted_comment(
                account,
                comment.login.as_deref().unwrap_or_default(),
                comment.body.as_deref().unwrap_or_default(),
                body,
            ) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Approves commit `head` of the pull request with `body` as the review
    /// text. GitHub refuses the review when `head` is no longer a commit of
    /// the pull request.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails.
    async fn approve(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        head: &str,
        body: &str,
    ) -> Result<(), String> {
        let path = format!("repos/{repo}/pulls/{number}/reviews");
        let commit = format!("commit_id={head}");
        self.run(
            account,
            &[
                "api",
                "--method",
                "POST",
                &path,
                "-f",
                "event=APPROVE",
                "-f",
                &commit,
                "-F",
                "body=@-",
            ],
            Some(body),
        )
        .await
        .map(|_| ())
    }

    /// Ticks Renovate's rebase checkbox in the pull request description,
    /// unless the head is no longer `head`. The head and the description come
    /// from one read, so the write never builds on a newer head's body.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the read or the write fails.
    async fn request_renovate_rebase(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        head: &str,
    ) -> Result<RebaseCheckbox, String> {
        #[derive(Deserialize)]
        struct Body {
            body: Option<String>,
            #[serde(rename = "headRefOid")]
            head: String,
        }
        let number_text = number.to_string();
        let out = self
            .run(
                account,
                &[
                    "pr",
                    "view",
                    &number_text,
                    "--repo",
                    repo,
                    "--json",
                    "body,headRefOid",
                ],
                None,
            )
            .await?;
        let view: Body = serde_json::from_str(&out)
            .map_err(|_| "gh pr view returned unexpected output".to_owned())?;
        if view.head != head {
            return Ok(RebaseCheckbox::HeadMoved);
        }
        let body = view.body.unwrap_or_default();
        let Some(ticked) = tick_rebase_checkbox(&body) else {
            return Ok(RebaseCheckbox::Missing);
        };
        if ticked != body {
            let path = format!("repos/{repo}/pulls/{number}");
            self.run(
                account,
                &["api", "--method", "PATCH", &path, "-F", "body=@-"],
                Some(&ticked),
            )
            .await?;
        }
        Ok(RebaseCheckbox::Ticked)
    }

    /// Squash-merges the pull request and deletes its branch, only while its
    /// head is still `head`. GitHub refuses the merge when someone pushed
    /// since, so a commit nobody checked never lands.
    ///
    /// # Errors
    ///
    /// Returns gh's message, such as when the head moved.
    async fn merge_head(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        head: &str,
    ) -> Result<(), String> {
        let number_text = number.to_string();
        self.run(
            account,
            &[
                "pr",
                "merge",
                &number_text,
                "--repo",
                repo,
                "--squash",
                "--delete-branch",
                "--match-head-commit",
                head,
            ],
            None,
        )
        .await
        .map(|_| ())
    }

    /// The text of `path` at `git_ref`, or `None` when the file or the ref
    /// does not exist.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails for another reason, or when
    /// the path is not a file.
    async fn file_at(
        &self,
        account: &Account,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>, String> {
        let out = match self
            .run(
                account,
                &[
                    "api",
                    "--method",
                    "GET",
                    &format!("repos/{repo}/contents/{path}"),
                    "-f",
                    &format!("ref={git_ref}"),
                ],
                None,
            )
            .await
        {
            Ok(out) => out,
            Err(err) if is_not_found(&err) => return Ok(None),
            Err(err) => return Err(err),
        };
        let content: ContentJson =
            serde_json::from_str(&out).map_err(|err| format!("contents of {path}: {err}"))?;
        if content.kind.as_deref() != Some("file") {
            return Err(format!(
                "{path} is a {} and not a file",
                content.kind.as_deref().unwrap_or("thing of unknown type")
            ));
        }
        match content.encoding.as_deref() {
            Some("base64") => {}
            // GitHub leaves the content empty for a file over 1 MB.
            Some("none") => return self.raw_file(account, repo, path, git_ref).await,
            other => {
                return Err(format!(
                    "contents of {path} have the unknown encoding {}",
                    other.unwrap_or("none given")
                ));
            }
        }
        let packed: String = content.content.split_whitespace().collect();
        let bytes = STANDARD
            .decode(packed)
            .map_err(|err| format!("contents of {path}: {err}"))?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    async fn raw_file(
        &self,
        account: &Account,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>, String> {
        match self
            .run(
                account,
                &[
                    "api",
                    "--method",
                    "GET",
                    "-H",
                    "Accept: application/vnd.github.raw+json",
                    &format!("repos/{repo}/contents/{path}"),
                    "-f",
                    &format!("ref={git_ref}"),
                ],
                None,
            )
            .await
        {
            Ok(text) => Ok(Some(text)),
            Err(err) if is_not_found(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// The patch GitHub reports for `path` between the refs `from` and `to`,
    /// or `None` when the file did not change.
    ///
    /// # Errors
    ///
    /// Returns an error naming the refs when GitHub does not know one of them,
    /// gh's message when the call fails for another reason, when its
    /// output does not parse, or when the comparison lists the most files
    /// GitHub shows and `path` is not among them.
    async fn compare_patch(
        &self,
        account: &Account,
        repo: &str,
        from: &str,
        to: &str,
        path: &str,
    ) -> Result<Option<String>, String> {
        let out = match self
            .api_get(account, &format!("repos/{repo}/compare/{from}...{to}"), &[])
            .await
        {
            Ok(out) => out,
            Err(err) if is_not_found(&err) => {
                return Err(format!(
                    "compare {from}...{to} is not a valid comparison in {repo}"
                ));
            }
            Err(err) => return Err(err),
        };
        let compare: CompareJson =
            serde_json::from_str(&out).map_err(|err| format!("compare {from}...{to}: {err}"))?;
        let listed = compare.files.len();
        match compare.files.into_iter().find(|file| file.filename == path) {
            Some(file) => Ok(file.patch),
            None if listed >= COMPARE_FILE_LIMIT => Err(format!(
                "compare {from}...{to} lists {listed} files, GitHub's limit, so it cannot show whether {path} changed"
            )),
            None => Ok(None),
        }
    }

    /// # Errors
    ///
    /// Returns gh's message when a call fails or its output does not parse.
    async fn status(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        ignore: &[String],
    ) -> Result<PrStatus, String> {
        let number_text = number.to_string();
        let view = self
            .run(
                account,
                &[
                    "pr",
                    "view",
                    &number_text,
                    "--repo",
                    repo,
                    "--json",
                    "state,mergeable,statusCheckRollup,comments,reviews,headRefOid",
                ],
                None,
            )
            .await?;
        let inline = self
            .run(
                account,
                &[
                    "api",
                    "--paginate",
                    &format!("repos/{repo}/pulls/{number}/comments"),
                    "--jq",
                    ".[]",
                ],
                None,
            )
            .await?;
        parse_status(&view, &inline, ignore)
    }

    /// The failed checks with the end of each GitHub Actions job's failed
    /// steps' log, for the agent to read.
    async fn failure_report(
        &self,
        account: &Account,
        repo: &str,
        checks: &[FailedCheck],
    ) -> Result<String, String> {
        let mut report = String::new();
        for check in checks {
            let _ = writeln!(report, "Check {:?} failed.", check.name);
            let job = check.url.as_deref().and_then(actions_job_id);
            match job {
                Some(job) => {
                    match self
                        .run(
                            account,
                            &["run", "view", "--repo", repo, "--job", job, "--log-failed"],
                            None,
                        )
                        .await
                    {
                        Ok(log) => report.push_str(tail(&log, MAX_LOG)),
                        Err(err) if err.contains(crate::forge::REJECTED) => return Err(err),
                        Err(err) => {
                            let _ = writeln!(report, "Could not fetch its log: {err}");
                        }
                    }
                }
                None => {
                    if let Some(url) = &check.url {
                        let _ = writeln!(report, "Details: {url}");
                    }
                }
            }
            report.push('\n');
        }
        Ok(report)
    }

    /// A GET of the API path `path` as `account`. With no token it is a plain
    /// request to the public API, which reads a public repository but is rate
    /// limited. Failures read like `gh`'s, so `HTTP 404` marks a missing path.
    async fn api_get(
        &self,
        account: &Account,
        path: &str,
        args: &[&str],
    ) -> Result<String, String> {
        if account.token.is_some() {
            let mut full = vec!["api", path];
            full.extend_from_slice(args);
            return self.run(account, &full, None).await;
        }
        let url = format!("{}/{path}", self.api_base.trim_end_matches('/'));
        let response = reqwest::Client::new()
            .get(&url)
            .header("User-Agent", "factory")
            .header("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|err| format!("GET {path} without a token failed: {err}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| format!("GET {path} without a token failed: {err}"))?;
        if status.is_success() {
            Ok(body)
        } else {
            let detail: String = body.chars().take(200).collect();
            Err(format!(
                "GET {path} without a token failed: HTTP {}: {}",
                status.as_u16(),
                detail.trim()
            ))
        }
    }

    async fn run(
        &self,
        account: &Account,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<String, String> {
        let mut command = Command::new(&self.gh);
        command
            .args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(token) = &account.token {
            command.env("GH_TOKEN", token);
        }
        let mut child = command
            .spawn()
            .map_err(|err| format!("could not start {}: {err}", self.gh.display()))?;
        let written = match (stdin, child.stdin.take()) {
            (Some(text), Some(mut pipe)) => pipe.write_all(text.as_bytes()).await,
            _ => Ok(()),
        };
        let output = child
            .wait_with_output()
            .await
            .map_err(|err| format!("waiting for gh: {err}"))?;
        if output.status.success() {
            // gh may exit without reading its stdin, so a broken pipe on a
            // successful exit is not an error.
            match written {
                Err(err) if err.kind() != std::io::ErrorKind::BrokenPipe => {
                    Err(format!("writing to gh: {err}"))
                }
                _ => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
            }
        } else {
            Err(command_error(
                &format!(
                    "gh {}",
                    args.iter().take(2).copied().collect::<Vec<_>>().join(" ")
                ),
                &String::from_utf8_lossy(&output.stderr),
                account.token.as_slice(),
            ))
        }
    }
}

#[derive(Deserialize)]
struct ApiUser {
    login: String,
    id: u64,
}

#[derive(Deserialize)]
struct ApiRepoAccess {
    #[serde(default)]
    permissions: ApiPermissions,
}

#[derive(Default, Deserialize)]
struct ApiPermissions {
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    maintain: bool,
    #[serde(default)]
    push: bool,
}

/// The marker Renovate puts on the rebase/retry checkbox line of its pull
/// request description.
const REBASE_MARKER: &str = "<!-- rebase-check -->";

/// What asking Renovate to rebase found in the description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseCheckbox {
    /// The checkbox is ticked, by this call or before it.
    Ticked,
    /// The description has no rebase checkbox.
    Missing,
    /// The pull request has another head than the caller expected, so
    /// nothing was written.
    HeadMoved,
}

/// `body` with Renovate's rebase checkbox ticked: the first
/// `- [ ] <!-- rebase-check -->` becomes `- [x] <!-- rebase-check -->`, the
/// exact text Renovate looks for, and every other byte stays. `None` when
/// the body has no such checkbox, so a stray marker or a checkbox with
/// anything between it and the marker does not count. A box that is already
/// ticked comes back unchanged.
#[must_use]
pub fn tick_rebase_checkbox(body: &str) -> Option<String> {
    let open = format!("- [ ] {REBASE_MARKER}");
    let done = format!("- [x] {REBASE_MARKER}");
    match (body.find(&open), body.find(&done)) {
        (Some(at), ticked) if ticked.is_none_or(|t| at < t) => {
            let mut out = String::with_capacity(body.len());
            out.push_str(&body[..at]);
            out.push_str(&done);
            out.push_str(&body[at + open.len()..]);
            Some(out)
        }
        (_, Some(_)) => Some(body.to_owned()),
        _ => None,
    }
}

impl Forge for GitHub {
    fn access<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
    ) -> BoxFuture<'a, Result<Access, String>> {
        Box::pin(async move {
            let out = match self
                .api_get(
                    account,
                    &format!("repos/{repo}"),
                    &["--hostname", "github.com"],
                )
                .await
            {
                Ok(out) => out,
                Err(err) if err.contains("HTTP 404") => return Ok(Access::NotVisible),
                Err(err) => return Err(err),
            };
            let json: ApiRepoAccess = serde_json::from_str(&out)
                .map_err(|_| "gh api repos returned unexpected output".to_owned())?;
            let p = json.permissions;
            Ok(if p.admin {
                Access::Admin
            } else if p.maintain || p.push {
                Access::Write
            } else {
                Access::Read
            })
        })
    }

    fn user_access<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        login: &'a str,
    ) -> BoxFuture<'a, Result<Option<Access>, String>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Permission {
                permission: String,
            }
            // A bot such as `dependabot[bot]` or a name that is not a login
            // is no collaborator, and it never reaches the API path.
            if login.is_empty()
                || !login
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Ok(Some(Access::Read));
            }
            let out = match self
                .api_get(
                    account,
                    &format!("repos/{repo}/collaborators/{login}/permission"),
                    &["--hostname", "github.com"],
                )
                .await
            {
                Ok(out) => out,
                Err(err) if is_not_found(&err) => return Ok(Some(Access::Read)),
                Err(err) => return Err(err),
            };
            let json: Permission = serde_json::from_str(&out).map_err(|_| {
                "gh api collaborators permission returned unexpected output".to_owned()
            })?;
            // GitHub maps maintain to write and triage to read.
            Ok(Some(match json.permission.as_str() {
                "admin" => Access::Admin,
                "write" => Access::Write,
                _ => Access::Read,
            }))
        })
    }

    fn may_merge<'a>(
        &'a self,
        _account: &'a Account,
        _repo: &'a str,
        _number: u64,
        access: Access,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move { Ok(matches!(access, Access::Write | Access::Admin)) })
    }

    fn lookup_account<'a>(
        &'a self,
        token: &'a str,
    ) -> BoxFuture<'a, Result<ForgeAccount, LookupError>> {
        Box::pin(async move {
            let probe = Account {
                token: Some(token.to_owned()),
                login: String::new(),
                name: String::new(),
                email: String::new(),
            };
            let out = self
                .run(&probe, &["api", "user", "--hostname", "github.com"], None)
                .await
                .map_err(|err| {
                    if err.contains("HTTP 401") || err.contains("HTTP 403") {
                        LookupError::Rejected
                    } else {
                        LookupError::Failed(err.replace(token, "<redacted>"))
                    }
                })?;
            let user: ApiUser = serde_json::from_str(&out).map_err(|_| {
                LookupError::Failed("gh api user returned unexpected output".into())
            })?;
            Ok(ForgeAccount {
                login: user.login,
                id: user.id,
            })
        })
    }

    fn account(&self, token: String, found: &ForgeAccount) -> Account {
        Account {
            token: Some(token),
            login: found.login.clone(),
            name: found.login.clone(),
            email: format!("{}+{}@users.noreply.github.com", found.id, found.login),
        }
    }

    fn clone_url(&self, repo: &str) -> String {
        format!("https://github.com/{repo}.git")
    }

    fn git_auth_header(&self, account: &Account) -> Option<String> {
        let token = account.token.as_ref()?;
        let basic = STANDARD.encode(format!("x-access-token:{token}"));
        Some(format!("Authorization: Basic {basic}"))
    }

    fn compare_range(&self, body: &str, upstream: &str) -> Option<(String, String)> {
        let marker = format!("github.com/{}/compare/", upstream.to_ascii_lowercase());
        let start = body.to_ascii_lowercase().find(&marker)? + marker.len();
        let link: String = body[start..]
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ')' | ']' | '>' | '"' | '\''))
            .collect();
        let (from, to) = link.split_once("...")?;
        (!from.is_empty() && !to.is_empty()).then(|| (from.to_owned(), to.to_owned()))
    }

    fn merged_pr<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        branch: &'a str,
    ) -> BoxFuture<'a, Result<Option<u64>, String>> {
        Box::pin(GitHub::merged_pr(self, account, repo, branch))
    }

    fn open_pr<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        base: &'a str,
        branch: &'a str,
        description: &'a PrDescription,
    ) -> BoxFuture<'a, Result<u64, String>> {
        Box::pin(GitHub::open_pr(
            self,
            account,
            repo,
            base,
            branch,
            description,
        ))
    }

    fn rerun_failed<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<Rerun, String>> {
        Box::pin(GitHub::rerun_failed(self, account, repo, number))
    }

    fn merge<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(GitHub::merge(self, account, repo, number))
    }

    fn renovate_prs<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        base: &'a str,
        _author: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<RenovatePr>, String>> {
        Box::pin(GitHub::renovate_prs(self, account, repo, base))
    }

    fn search_open_prs<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        query: &'a str,
    ) -> BoxFuture<'a, Result<Vec<OpenPr>, String>> {
        Box::pin(GitHub::search_open_prs(self, account, repo, query))
    }

    fn pull_request<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<Option<PullRequest>, String>> {
        Box::pin(GitHub::pull_request(self, account, repo, number))
    }

    fn pr_info<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<PrInfo, String>> {
        Box::pin(GitHub::pr_info(self, account, repo, number))
    }

    fn head_commit<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<HeadCommit, String>> {
        Box::pin(GitHub::head_commit(self, account, repo, number))
    }

    fn merged_at<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(GitHub::merged_at(self, account, repo, number))
    }

    fn pr_details<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<PrDetails, String>> {
        Box::pin(GitHub::pr_details(self, account, repo, number))
    }

    fn request_renovate_rebase<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
    ) -> BoxFuture<'a, Result<RebaseCheckbox, String>> {
        Box::pin(GitHub::request_renovate_rebase(
            self, account, repo, number, head,
        ))
    }

    fn comment<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        body: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(GitHub::comment(self, account, repo, number, body))
    }

    fn has_comment<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        body: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(GitHub::has_comment(self, account, repo, number, body))
    }

    fn approve<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(GitHub::approve(self, account, repo, number, head, body))
    }

    fn merge_head<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(GitHub::merge_head(self, account, repo, number, head))
    }

    fn file_at<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        path: &'a str,
        git_ref: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(GitHub::file_at(self, account, repo, path, git_ref))
    }

    fn compare_patch<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        from: &'a str,
        to: &'a str,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(GitHub::compare_patch(self, account, repo, from, to, path))
    }

    fn status<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        ignore: &'a [String],
    ) -> BoxFuture<'a, Result<PrStatus, String>> {
        Box::pin(GitHub::status(self, account, repo, number, ignore))
    }

    fn failure_report<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        checks: &'a [FailedCheck],
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(GitHub::failure_report(self, account, repo, checks))
    }
}

pub(crate) fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}
