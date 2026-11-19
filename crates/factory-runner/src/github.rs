use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;

use factory_core::PrDescription;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const MAX_LOG: usize = 30_000;

/// Talks to GitHub through the `gh` CLI with the factory's token.
#[derive(Debug, Clone)]
pub struct GitHub {
    pub gh: PathBuf,
    pub token: Option<String>,
    /// The factory's own login. Its comments are not review feedback.
    pub bot_login: String,
}

impl Default for GitHub {
    fn default() -> Self {
        GitHub {
            gh: PathBuf::from("gh"),
            token: None,
            bot_login: "moonthebot".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck {
    pub name: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ci {
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
    pub ci: Ci,
    pub comments: Vec<Comment>,
}

/// What the factory tells the job after looking at its pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    Merged,
    Closed,
    CiFailed(Vec<FailedCheck>),
    /// Comments newer than the watermark, and the new watermark.
    Comments {
        text: String,
        seen_until: String,
    },
    Nothing,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewJson {
    state: String,
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

const FAILED_CONCLUSIONS: [&str; 5] = [
    "FAILURE",
    "TIMED_OUT",
    "CANCELLED",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
];

/// Parses `gh pr view --json state,statusCheckRollup,comments,reviews` and
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
    } else {
        Ci::Passed
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
        ci,
        comments,
    })
}

/// Decides what to tell the job. A merge or close comes first, then a CI
/// failure, then comments newer than `seen_until`.
#[must_use]
pub fn observe(status: &PrStatus, seen_until: Option<&str>) -> Observation {
    match status.state {
        PrState::Merged => return Observation::Merged,
        PrState::Closed => return Observation::Closed,
        PrState::Open => {}
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
        return Observation::Nothing;
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
    /// Opens the pull request for `branch`, or updates the title and
    /// description of the open one, and returns its number.
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
                    "state,statusCheckRollup,comments,reviews",
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
