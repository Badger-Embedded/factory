use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use factory_core::PrDescription;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const MAX_LOG: usize = 30_000;
/// The largest diff [`GitHub::pr_details`] returns. A larger one is an error,
/// because a review of a cut diff could approve changes it never saw.
const MAX_DIFF: usize = 200_000;
/// The most changed files GitHub's compare API lists.
const COMPARE_FILE_LIMIT: usize = 300;

/// Talks to GitHub through the `gh` CLI with the factory's token.
#[derive(Debug, Clone)]
pub struct GitHub {
    pub gh: PathBuf,
    pub token: Option<String>,
    /// The factory's own login. Its comments are not review feedback.
    pub bot_login: String,
    /// How many times [`GitHub::rerun_failed`] reads the pull request while
    /// it waits for GitHub to stop reporting the old failure.
    pub settle_polls: u32,
    pub settle_interval: Duration,
}

impl Default for GitHub {
    fn default() -> Self {
        GitHub {
            gh: PathBuf::from("gh"),
            token: None,
            bot_login: "moonthebot".into(),
            settle_polls: 10,
            settle_interval: Duration::from_secs(3),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// What [`GitHub::rerun_failed`] did.
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

/// A pull request without its diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrInfo {
    pub title: String,
    pub body: String,
    /// The SHA of the pull request's head commit.
    pub head: String,
    pub files: Vec<String>,
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
/// `bot_login` or by GitHub apps are left out.
///
/// # Errors
///
/// Fails when either JSON does not parse.
pub fn parse_status(view: &str, inline: &str, bot_login: &str) -> Result<PrStatus, String> {
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
        (login != bot_login && !login.ends_with("[bot]")).then_some(login)
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
    pub async fn merged_pr(&self, repo: &str, branch: &str) -> Result<Option<u64>, String> {
        let merged = self
            .run(
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
    pub async fn open_pr(
        &self,
        repo: &str,
        base: &str,
        branch: &str,
        description: &PrDescription,
    ) -> Result<u64, String> {
        let existing = self
            .run(
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
            let was_failed = match self.status(repo, number).await {
                Ok(status) => matches!(status.ci, Ci::Failed(_)),
                Err(err) => {
                    tracing::error!("reading the checks of pull request {number}: {err}");
                    false
                }
            };
            self.run(
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
                match self.rerun_failed(repo, number).await {
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
    pub async fn rerun_failed(&self, repo: &str, number: u64) -> Result<Rerun, String> {
        let number_text = number.to_string();
        let head = self
            .run(
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
                &["run", "rerun", &id.to_string(), "--repo", repo, "--failed"],
                None,
            )
            .await?;
        }
        for _ in 0..self.settle_polls {
            if !matches!(self.status(repo, number).await?.ci, Ci::Failed(_)) {
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
    pub async fn merge(&self, repo: &str, number: u64) -> Result<(), String> {
        let number_text = number.to_string();
        self.run(
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
    pub async fn renovate_prs(&self, repo: &str, base: &str) -> Result<Vec<RenovatePr>, String> {
        let base_field = format!("base={base}");
        let lines = self
            .run(
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

    /// The title, description, head and changed files of a pull request,
    /// without its diff.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails or its output does not parse.
    pub async fn pr_info(&self, repo: &str, number: u64) -> Result<PrInfo, String> {
        let view = self
            .run(
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

    /// The title, description, head, changed files and diff of a pull
    /// request.
    ///
    /// # Errors
    ///
    /// Returns gh's message when a call fails or its output does not parse.
    pub async fn pr_details(&self, repo: &str, number: u64) -> Result<PrDetails, String> {
        let info = self.pr_info(repo, number).await?;
        let diff = self
            .run(&["pr", "diff", &number.to_string(), "--repo", repo], None)
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
    pub async fn comment(&self, repo: &str, number: u64, body: &str) -> Result<(), String> {
        let number_text = number.to_string();
        self.run(
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

    /// Approves commit `head` of the pull request with `body` as the review
    /// text. GitHub refuses the review when `head` is no longer a commit of
    /// the pull request.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails.
    pub async fn approve(
        &self,
        repo: &str,
        number: u64,
        head: &str,
        body: &str,
    ) -> Result<(), String> {
        let path = format!("repos/{repo}/pulls/{number}/reviews");
        let commit = format!("commit_id={head}");
        self.run(
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

    /// Squash-merges the pull request and deletes its branch, only while its
    /// head is still `head`. GitHub refuses the merge when someone pushed
    /// since, so a commit nobody checked never lands.
    ///
    /// # Errors
    ///
    /// Returns gh's message, such as when the head moved.
    pub async fn merge_head(&self, repo: &str, number: u64, head: &str) -> Result<(), String> {
        let number_text = number.to_string();
        self.run(
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
    pub async fn file_at(
        &self,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>, String> {
        let out = match self
            .run(
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
            Some("none") => return self.raw_file(repo, path, git_ref).await,
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
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>, String> {
        match self
            .run(
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
    /// or `None` when a ref is unknown or the file did not change.
    ///
    /// # Errors
    ///
    /// Returns gh's message when the call fails for another reason, when its
    /// output does not parse, or when the comparison lists the most files
    /// GitHub shows and `path` is not among them.
    pub async fn compare_patch(
        &self,
        repo: &str,
        from: &str,
        to: &str,
        path: &str,
    ) -> Result<Option<String>, String> {
        let out = match self
            .run(
                &["api", &format!("repos/{repo}/compare/{from}...{to}")],
                None,
            )
            .await
        {
            Ok(out) => out,
            Err(err) if is_not_found(&err) => return Ok(None),
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
    pub async fn status(&self, repo: &str, number: u64) -> Result<PrStatus, String> {
        let number_text = number.to_string();
        let view = self
            .run(
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
        parse_status(&view, &inline, &self.bot_login)
    }

    /// The failed checks with the end of each GitHub Actions job's failed
    /// steps' log, for the agent to read.
    pub async fn failure_report(&self, repo: &str, checks: &[FailedCheck]) -> String {
        let mut report = String::new();
        for check in checks {
            let _ = writeln!(report, "Check {:?} failed.", check.name);
            let job = check.url.as_deref().and_then(actions_job_id);
            match job {
                Some(job) => {
                    match self
                        .run(
                            &["run", "view", "--repo", repo, "--job", job, "--log-failed"],
                            None,
                        )
                        .await
                    {
                        Ok(log) => report.push_str(tail(&log, MAX_LOG)),
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
        report
    }

    async fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
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
            .stderr(Stdio::piped());
        if let Some(token) = &self.token {
            command.env("GH_TOKEN", token);
        }
        let mut child = command
            .spawn()
            .map_err(|err| format!("could not start {}: {err}", self.gh.display()))?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(text.as_bytes())
                .await
                .map_err(|err| format!("writing to gh: {err}"))?;
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|err| format!("waiting for gh: {err}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "gh {} failed: {}",
                args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}
