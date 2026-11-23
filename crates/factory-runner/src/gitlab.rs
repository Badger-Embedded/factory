use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use factory_core::PrDescription;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::forge::{
    Access, Account, BoxFuture, Forge, ForgeAccount, ForgeKind, LookupError, REJECTED,
    command_error,
};
use crate::github::{
    Ci, Comment, FailedCheck, HeadCommit, MAX_DIFF, MAX_LOG, PrDetails, PrInfo, PrState, PrStatus,
    RebaseCheckbox, RenovatePr, Rerun, tail, tick_rebase_checkbox,
};

/// GitLab's access levels for a project member.
const DEVELOPER: u32 = 30;
const MAINTAINER: u32 = 40;

const PAGE_SIZE: usize = 100;
/// The most pages a listing reads, so a runaway listing ends.
const MAX_PAGES: usize = 50;

/// Variables of the factory's own environment that would give `glab` a
/// credential or a host the call did not ask for.
const INHERITED: &[&str] = &[
    "GLAB_IS_OAUTH2",
    "OAUTH2_REFRESH_TOKEN",
    "GLAB_SKIP_TLS_VERIFY",
    "SKIP_TLS_VERIFY",
    "JOB_TOKEN",
    "CI_JOB_TOKEN",
    "API_PROTOCOL",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_COUNT",
    "GITLAB_TOKEN",
    "GITLAB_ACCESS_TOKEN",
    "OAUTH_TOKEN",
    "GITLAB_API_HOST",
    "GITLAB_URI",
    "GITLAB_API_PROTOCOL",
];

/// An empty directory for `glab`'s configuration. A login stored in the
/// user's own configuration never takes the place of a missing token, and
/// `glab` sends an unauthenticated request instead.
fn config_dir() -> Result<&'static Path, String> {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    if let Some(dir) = DIR.get() {
        return Ok(dir.path());
    }
    let dir = tempfile::tempdir().map_err(|err| format!("creating the glab config dir: {err}"))?;
    Ok(DIR.get_or_init(|| dir).path())
}

/// Talks to a GitLab host through the `glab` CLI as the account each call
/// names. The host is `salsa.debian.org` for [`ForgeKind::Salsa`].
#[derive(Debug, Clone)]
pub struct GitLab {
    pub glab: PathBuf,
    /// The host the projects live on, such as `salsa.debian.org`.
    pub host: String,
    /// How many times `rerun_failed` reads the merge request while it waits
    /// for GitLab to stop reporting the old failure.
    pub settle_polls: u32,
    pub settle_interval: Duration,
}

impl Default for GitLab {
    fn default() -> Self {
        GitLab {
            glab: PathBuf::from("glab"),
            host: ForgeKind::Salsa.host().to_owned(),
            settle_polls: 10,
            settle_interval: Duration::from_secs(3),
        }
    }
}

#[derive(Deserialize)]
struct UserJson {
    id: u64,
    username: String,
}

#[derive(Default, Deserialize)]
struct ProjectJson {
    #[serde(default)]
    permissions: PermissionsJson,
}

#[derive(Default, Deserialize)]
struct PermissionsJson {
    project_access: Option<LevelJson>,
    group_access: Option<LevelJson>,
}

#[derive(Deserialize)]
struct LevelJson {
    access_level: u32,
}

#[derive(Deserialize)]
struct MrJson {
    iid: u64,
    #[serde(default)]
    title: String,
    description: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    sha: String,
    #[serde(default)]
    has_conflicts: bool,
    detailed_merge_status: Option<String>,
    head_pipeline: Option<PipelineJson>,
    user: Option<MrUserJson>,
    merged_at: Option<String>,
    #[serde(default)]
    web_url: String,
    #[serde(default)]
    target_branch: String,
    project_id: Option<u64>,
    source_project_id: Option<u64>,
}

#[derive(Deserialize)]
struct PipelineJson {
    id: u64,
    /// The project that owns the pipeline. For a merge request from a fork
    /// it is usually the fork.
    project_id: Option<u64>,
    #[serde(default)]
    sha: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    web_url: String,
}

#[derive(Deserialize)]
struct MrUserJson {
    #[serde(default)]
    can_merge: bool,
}

#[derive(Deserialize)]
struct JobJson {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    allow_failure: bool,
    web_url: Option<String>,
}

#[derive(Deserialize)]
struct NoteJson {
    #[serde(default)]
    body: String,
    #[serde(default)]
    system: bool,
    author: Option<NoteAuthor>,
    created_at: String,
    position: Option<PositionJson>,
}

#[derive(Deserialize)]
struct NoteAuthor {
    username: String,
    #[serde(default)]
    bot: bool,
}

#[derive(Deserialize)]
struct PositionJson {
    new_path: Option<String>,
    new_line: Option<u64>,
    old_path: Option<String>,
    old_line: Option<u64>,
}

// Mirrors GitLab's diff entry, which has these flags.
#[allow(clippy::struct_excessive_bools)]
#[derive(Deserialize)]
struct DiffJson {
    #[serde(default)]
    old_path: String,
    #[serde(default)]
    new_path: String,
    #[serde(default)]
    diff: String,
    #[serde(default)]
    new_file: bool,
    #[serde(default)]
    deleted_file: bool,
    #[serde(default)]
    too_large: bool,
    #[serde(default)]
    collapsed: bool,
}

#[derive(Deserialize)]
struct CompareJson {
    #[serde(default)]
    diffs: Vec<DiffJson>,
    #[serde(default)]
    compare_timeout: bool,
}

#[derive(Deserialize)]
struct DiffVersionJson {
    #[serde(default)]
    head_commit_sha: String,
    #[serde(default)]
    state: String,
    diffs: Option<Vec<DiffJson>>,
    real_size: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct VersionJson {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    head_commit_sha: String,
    created_at: String,
}

#[derive(Deserialize)]
struct CommitJson {
    committed_date: Option<String>,
}

/// GitLab's `2026-01-02T03:04:05.678Z` as `2026-01-02 03:04:05`.
fn sql_time(gitlab: &str) -> String {
    gitlab
        .chars()
        .take(19)
        .collect::<String>()
        .replace('T', " ")
}

/// `value` as one URL path segment, so a project path or a file path with
/// slashes names one resource.
#[must_use]
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[must_use]
pub fn project(repo: &str) -> String {
    format!("projects/{}", encode(repo))
}

#[must_use]
pub fn is_not_found(err: &str) -> bool {
    err.contains("HTTP 404")
}

/// Whether a GitLab user is a project or group access token's bot user.
fn is_token_bot(username: &str) -> bool {
    ["project_", "group_"].iter().any(|prefix| {
        username
            .strip_prefix(prefix)
            .and_then(|rest| rest.split_once("_bot"))
            .is_some_and(|(id, _)| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// The job ID in a GitLab job URL, such as
/// `https://salsa.debian.org/team/pkg/-/jobs/42`.
#[must_use]
pub fn gitlab_job_id(url: &str) -> Option<&str> {
    let (_, job) = url.split_once("/-/jobs/")?;
    let job = job.split(['/', '?', '#']).next()?;
    (!job.is_empty() && job.bytes().all(|b| b.is_ascii_digit())).then_some(job)
}

/// The checks of a pipeline's jobs. A job that may fail does not fail the
/// merge request.
/// The pipeline's own status decides a pass: a blocking manual job leaves
/// the pipeline `manual`, and a pipeline of only skipped jobs is `skipped`.
fn ci_of_jobs(jobs: Vec<JobJson>, pipeline_status: &str) -> Ci {
    let any = !jobs.is_empty();
    let mut failed = Vec::new();
    let mut pending = false;
    for job in jobs {
        match job.status.as_str() {
            "failed" | "canceled" if !job.allow_failure => failed.push(FailedCheck {
                name: job.name,
                url: job.web_url,
            }),
            "created"
            | "waiting_for_resource"
            | "preparing"
            | "pending"
            | "running"
            | "scheduled"
            | "canceling" => pending = true,
            _ => {}
        }
    }
    if !failed.is_empty() {
        Ci::Failed(failed)
    } else if pending {
        Ci::Pending
    } else if !any || matches!(pipeline_status, "skipped" | "failed" | "canceled") {
        Ci::NoChecks
    } else if pipeline_status == "success" {
        Ci::Passed
    } else {
        Ci::Pending
    }
}

/// The unified diff of one changed file, with the header `git apply` and
/// reviewers expect. GitLab sends only the hunks.
fn unified(diff: &DiffJson) -> String {
    let old = if diff.new_file {
        "/dev/null".to_owned()
    } else {
        format!("a/{}", diff.old_path)
    };
    let new = if diff.deleted_file {
        "/dev/null".to_owned()
    } else {
        format!("b/{}", diff.new_path)
    };
    let mut text = format!(
        "diff --git a/{} b/{}\n--- {old}\n+++ {new}\n{}",
        diff.old_path, diff.new_path, diff.diff
    );
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

impl GitLab {
    /// Runs `glab api` and returns what it printed. `fields` are `-f` and
    /// `-F` arguments: the flag, the key and the value.
    async fn api(
        &self,
        account: &Account,
        method: &str,
        endpoint: &str,
        fields: &[(&str, &str, &str)],
        stdin: Option<&str>,
    ) -> Result<String, String> {
        // The child runs in another directory, so a relative path to the
        // binary is resolved here.
        let glab = if self.glab.components().count() > 1 {
            std::path::absolute(&self.glab).unwrap_or_else(|_| self.glab.clone())
        } else {
            self.glab.clone()
        };
        let mut command = Command::new(glab);
        command.args(["api", "--method", method, endpoint]);
        for (flag, key, value) in fields {
            command.arg(flag).arg(format!("{key}={value}"));
        }
        command.args(["--hostname", &self.host]);
        // Outside any checkout, so no repository-local glab config applies.
        command.current_dir(config_dir()?);
        for name in INHERITED {
            command.env_remove(name);
        }
        command
            .env("GITLAB_HOST", &self.host)
            .env("GLAB_CONFIG_DIR", config_dir()?)
            .env("GLAB_ENABLE_CI_AUTOLOGIN", "false")
            .env("GLAB_API_PROTOCOL", "https")
            .env("GITLAB_API_PROTOCOL", "https")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GLAB_CHECK_UPDATE", "false")
            .env("GLAB_SEND_TELEMETRY", "false")
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
            command.env("GITLAB_TOKEN", token);
        }
        let mut child = command
            .spawn()
            .map_err(|err| format!("could not start {}: {err}", self.glab.display()))?;
        let written = match (stdin, child.stdin.take()) {
            (Some(text), Some(mut pipe)) => pipe.write_all(text.as_bytes()).await,
            _ => Ok(()),
        };
        let output = child
            .wait_with_output()
            .await
            .map_err(|err| format!("waiting for glab: {err}"))?;
        if output.status.success() {
            // glab may exit without reading its stdin, so a broken pipe on a
            // successful exit is not an error.
            match written {
                Err(err) if err.kind() != std::io::ErrorKind::BrokenPipe => {
                    Err(format!("writing to glab: {err}"))
                }
                _ => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
            }
        } else {
            Err(command_error(
                &format!("glab api {method} {endpoint}"),
                &String::from_utf8_lossy(&output.stderr),
                account.token.as_slice(),
            ))
        }
    }

    /// A GET of `endpoint` on the host's API as `account`, for reads that
    /// have no [`Forge`] operation, such as the onboarding checks. A missing
    /// resource is an error that [`is_not_found`] recognizes.
    ///
    /// # Errors
    ///
    /// Returns the redacted error of a failed `glab` call.
    pub async fn api_get(&self, account: &Account, endpoint: &str) -> Result<String, String> {
        self.get(account, endpoint, &[]).await
    }

    async fn get(
        &self,
        account: &Account,
        endpoint: &str,
        query: &[(&str, &str)],
    ) -> Result<String, String> {
        let fields: Vec<(&str, &str, &str)> = query
            .iter()
            .map(|(key, value)| ("-f", *key, *value))
            .collect();
        self.api(account, "GET", endpoint, &fields, None).await
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        account: &Account,
        endpoint: &str,
        query: &[(&str, &str)],
    ) -> Result<T, String> {
        let out = self.get(account, endpoint, query).await?;
        serde_json::from_str(&out)
            .map_err(|_| format!("glab api {endpoint} returned unexpected output"))
    }

    /// Every item of a paginated listing.
    async fn get_all<T: DeserializeOwned>(
        &self,
        account: &Account,
        endpoint: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<T>, String> {
        let per_page = PAGE_SIZE.to_string();
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let page = page.to_string();
            let mut paged = query.to_vec();
            paged.push(("per_page", &per_page));
            paged.push(("page", &page));
            let items: Vec<T> = self.get_json(account, endpoint, &paged).await?;
            let count = items.len();
            all.extend(items);
            if count < PAGE_SIZE {
                break;
            }
        }
        Ok(all)
    }

    async fn mr(&self, account: &Account, repo: &str, number: u64) -> Result<MrJson, String> {
        self.get_json(
            account,
            &format!("{}/merge_requests/{number}", project(repo)),
            &[],
        )
        .await
    }

    /// The project path prefix of a pipeline's endpoints.
    fn pipeline_base(repo: &str, pipeline: &PipelineJson) -> String {
        pipeline
            .project_id
            .map_or_else(|| project(repo), |id| format!("projects/{id}"))
    }

    async fn jobs(
        &self,
        account: &Account,
        base: &str,
        pipeline: u64,
    ) -> Result<Vec<JobJson>, String> {
        self.get_all(account, &format!("{base}/pipelines/{pipeline}/jobs"), &[])
            .await
    }

    /// The pipeline on the merge request's head, or `None` when it has none
    /// yet. After a push, `head_pipeline` still names the old head's pipeline
    /// until GitLab creates the new one.
    fn head_pipeline(mr: &MrJson) -> Option<&PipelineJson> {
        mr.head_pipeline
            .as_ref()
            .filter(|pipeline| pipeline.sha.is_empty() || pipeline.sha == mr.sha)
    }

    async fn first_mr(
        &self,
        account: &Account,
        repo: &str,
        branch: &str,
        state: &str,
        base: Option<&str>,
    ) -> Result<Option<u64>, String> {
        // The list holds merge requests from forks too, so match the source
        // project and not only the branch name.
        let list: Vec<MrJson> = self
            .get_all(
                account,
                &format!("{}/merge_requests", project(repo)),
                &[("source_branch", branch), ("state", state)],
            )
            .await?;
        Ok(list
            .iter()
            .find(|mr| {
                mr.project_id.is_some()
                    && mr.source_project_id == mr.project_id
                    && base.is_none_or(|base| mr.target_branch == base)
            })
            .map(|mr| mr.iid))
    }

    async fn open_pr(
        &self,
        account: &Account,
        repo: &str,
        base: &str,
        branch: &str,
        description: &PrDescription,
    ) -> Result<u64, String> {
        let existing = self
            .first_mr(account, repo, branch, "opened", Some(base))
            .await?;
        let list = format!("{}/merge_requests", project(repo));
        if let Some(number) = existing {
            let was_failed = match self.status(account, repo, number, &[]).await {
                Ok(status) => matches!(status.ci, Ci::Failed(_)),
                Err(err) => {
                    tracing::error!("reading the checks of merge request {number}: {err}");
                    false
                }
            };
            self.api(
                account,
                "PUT",
                &format!("{list}/{number}"),
                &[
                    ("-f", "title", &description.title),
                    ("-F", "description", "@-"),
                ],
                Some(&description.body),
            )
            .await?;
            if was_failed {
                match self.rerun_failed(account, repo, number).await {
                    Ok(Rerun::Started(_)) => {}
                    Ok(Rerun::NothingToRerun) => {
                        tracing::error!("merge request {number} has no failed job to rerun");
                    }
                    Err(err) => tracing::error!("rerunning merge request {number}: {err}"),
                }
            }
            return Ok(number);
        }
        let out = self
            .api(
                account,
                "POST",
                &list,
                &[
                    ("-f", "source_branch", branch),
                    ("-f", "target_branch", base),
                    ("-f", "title", &description.title),
                    ("-F", "description", "@-"),
                    ("-F", "remove_source_branch", "true"),
                    ("-F", "squash", "true"),
                ],
                Some(&description.body),
            )
            .await?;
        serde_json::from_str::<MrJson>(&out)
            .map(|mr| mr.iid)
            .map_err(|_| "glab api merge request create printed no merge request".to_owned())
    }

    /// Retries the failed jobs of the pipeline on the merge request's head.
    /// GitLab keeps reporting the old failure for a moment after a retry, so
    /// this reads the merge request until no check is failed, up to
    /// `settle_polls` times.
    async fn rerun_failed(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<Rerun, String> {
        let mr = self.mr(account, repo, number).await?;
        let Some(pipeline) = Self::head_pipeline(&mr) else {
            return Ok(Rerun::NothingToRerun);
        };
        let failed: Vec<u64> = self
            .jobs(account, &Self::pipeline_base(repo, pipeline), pipeline.id)
            .await?
            .into_iter()
            .filter(|job| matches!(job.status.as_str(), "failed" | "canceled"))
            .map(|job| job.id)
            .collect();
        if failed.is_empty() {
            return Ok(Rerun::NothingToRerun);
        }
        self.api(
            account,
            "POST",
            &format!(
                "{}/pipelines/{}/retry",
                Self::pipeline_base(repo, pipeline),
                pipeline.id
            ),
            &[],
            None,
        )
        .await?;
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

    async fn status(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        ignore: &[String],
    ) -> Result<PrStatus, String> {
        let mr = self.mr(account, repo, number).await?;
        let state = match mr.state.as_str() {
            "merged" => PrState::Merged,
            "closed" => PrState::Closed,
            _ => PrState::Open,
        };
        let conflicting =
            mr.has_conflicts || mr.detailed_merge_status.as_deref() == Some("conflict");
        if state != PrState::Open {
            return Ok(PrStatus {
                state,
                conflicting,
                ci: Ci::NoChecks,
                comments: Vec::new(),
                head: mr.sha,
            });
        }
        let ci = match Self::head_pipeline(&mr) {
            None => Ci::NoChecks,
            Some(pipeline) => match ci_of_jobs(
                self.jobs(account, &Self::pipeline_base(repo, pipeline), pipeline.id)
                    .await?,
                &pipeline.status,
            ) {
                // A pipeline that failed before it made a job, such as for an
                // invalid configuration, has no job to name.
                Ci::NoChecks if matches!(pipeline.status.as_str(), "failed" | "canceled") => {
                    Ci::Failed(vec![FailedCheck {
                        name: "pipeline".into(),
                        url: (!pipeline.web_url.is_empty()).then(|| pipeline.web_url.clone()),
                    }])
                }
                ci => ci,
            },
        };
        let notes: Vec<NoteJson> = self
            .get_all(
                account,
                &format!("{}/merge_requests/{number}/notes", project(repo)),
                &[("sort", "asc"), ("order_by", "created_at")],
            )
            .await?;
        let mut comments = Vec::new();
        for note in notes {
            let Some(author) = note.author else { continue };
            if note.system
                || author.bot
                || is_token_bot(&author.username)
                || ignore.contains(&author.username)
            {
                continue;
            }
            let location = note.position.and_then(|position| {
                // A note on a removed line has both paths and only `old_line`.
                let (path, line) = match (position.new_line, position.old_line) {
                    (Some(line), _) => (position.new_path.or(position.old_path)?, Some(line)),
                    (None, Some(line)) => (position.old_path.or(position.new_path)?, Some(line)),
                    (None, None) => (position.new_path.or(position.old_path)?, None),
                };
                Some(match line {
                    Some(line) => format!("{path}:{line}"),
                    None => path,
                })
            });
            comments.push(Comment {
                author: author.username,
                body: note.body,
                at: note.created_at,
                location,
            });
        }
        comments.sort_by(|a, b| a.at.cmp(&b.at));
        Ok(PrStatus {
            state,
            conflicting,
            ci,
            comments,
            head: mr.sha,
        })
    }

    async fn changes(
        &self,
        account: &Account,
        repo: &str,
        mr: &MrJson,
    ) -> Result<Vec<DiffJson>, String> {
        let number = mr.iid;
        // The diff of the version whose head is the head under review, so a
        // push between two reads cannot substitute another diff.
        let versions: Vec<VersionJson> = self
            .get_all(
                account,
                &format!("{}/merge_requests/{number}/versions", project(repo)),
                &[],
            )
            .await?;
        let Some(version) = versions
            .iter()
            .filter(|version| version.head_commit_sha == mr.sha)
            .max_by_key(|version| version.id)
        else {
            return Err(format!(
                "merge request {number} has no diff version for its head {}, so it cannot be reviewed yet",
                mr.sha
            ));
        };
        let detail: DiffVersionJson = self
            .get_json(
                account,
                &format!(
                    "{}/merge_requests/{number}/versions/{}",
                    project(repo),
                    version.id
                ),
                &[],
            )
            .await?;
        if detail.state != "collected" || detail.head_commit_sha != mr.sha {
            return Err(format!(
                "the diff of merge request {number} is {:?}, not collected for its head, so it cannot be reviewed in full",
                detail.state
            ));
        }
        let real_size = match &detail.real_size {
            Some(serde_json::Value::String(text)) => text.parse::<usize>().ok(),
            Some(serde_json::Value::Number(n)) => n.as_u64().and_then(|n| usize::try_from(n).ok()),
            _ => None,
        };
        match detail.diffs {
            Some(diffs) if Some(diffs.len()) == real_size => Ok(diffs),
            _ => Err(format!(
                "the diff of merge request {number} is missing files, so it cannot be reviewed in full"
            )),
        }
    }

    async fn pr_info(&self, account: &Account, repo: &str, number: u64) -> Result<PrInfo, String> {
        let mr = self.mr(account, repo, number).await?;
        let files = self
            .changes(account, repo, &mr)
            .await?
            .into_iter()
            .map(|diff| diff.new_path)
            .collect();
        Ok(PrInfo {
            title: mr.title,
            body: mr.description.unwrap_or_default(),
            head: mr.sha,
            files,
        })
    }

    async fn pr_details(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<PrDetails, String> {
        let mr = self.mr(account, repo, number).await?;
        let changes = self.changes(account, repo, &mr).await?;
        let mut diff = String::new();
        for change in &changes {
            if change.too_large || change.collapsed {
                return Err(format!(
                    "the diff of {} in merge request {number} is too large for GitLab to send, so it cannot be reviewed in full",
                    change.new_path
                ));
            }
            diff.push_str(&unified(change));
        }
        if diff.len() > MAX_DIFF {
            return Err(format!(
                "the diff of merge request {number} is {} bytes, over the limit of {MAX_DIFF}, so it cannot be reviewed in full",
                diff.len()
            ));
        }
        Ok(PrDetails {
            title: mr.title,
            body: mr.description.unwrap_or_default(),
            head: mr.sha,
            files: changes.into_iter().map(|change| change.new_path).collect(),
            diff,
        })
    }

    async fn head_commit(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<HeadCommit, String> {
        let mr = self.mr(account, repo, number).await?;
        let versions: Vec<VersionJson> = self
            .get_all(
                account,
                &format!("{}/merge_requests/{number}/versions", project(repo)),
                &[],
            )
            .await?;
        // Each push to the source branch makes a version, so the oldest
        // version with this head dates when it became the head.
        let pushed_at = versions
            .iter()
            .filter(|version| version.head_commit_sha == mr.sha)
            .map(|version| sql_time(&version.created_at))
            .min();
        let committed_at = self
            .get_json::<CommitJson>(
                account,
                &format!("{}/repository/commits/{}", project(repo), encode(&mr.sha)),
                &[],
            )
            .await
            .ok()
            .and_then(|commit| commit.committed_date)
            .filter(|at| at.ends_with('Z') || at.ends_with("+00:00"))
            .map(|at| sql_time(&at));
        Ok(HeadCommit {
            sha: mr.sha,
            pushed_at,
            committed_at,
        })
    }

    async fn request_renovate_rebase(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        head: &str,
    ) -> Result<RebaseCheckbox, String> {
        let mr = self.mr(account, repo, number).await?;
        if mr.sha != head {
            return Ok(RebaseCheckbox::HeadMoved);
        }
        let body = mr.description.unwrap_or_default();
        let Some(ticked) = tick_rebase_checkbox(&body) else {
            return Ok(RebaseCheckbox::Missing);
        };
        if ticked != body {
            self.api(
                account,
                "PUT",
                &format!("{}/merge_requests/{number}", project(repo)),
                &[("-F", "description", "@-")],
                Some(&ticked),
            )
            .await?;
        }
        Ok(RebaseCheckbox::Ticked)
    }

    async fn merge_request(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        head: Option<&str>,
    ) -> Result<(), String> {
        let mut fields = vec![
            ("-F", "squash", "true"),
            ("-F", "should_remove_source_branch", "true"),
        ];
        if let Some(head) = head {
            fields.push(("-f", "sha", head));
        }
        self.api(
            account,
            "PUT",
            &format!("{}/merge_requests/{number}/merge", project(repo)),
            &fields,
            None,
        )
        .await
        .map(|_| ())
    }

    async fn comment(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<(), String> {
        self.api(
            account,
            "POST",
            &format!("{}/merge_requests/{number}/notes", project(repo)),
            &[("-F", "body", "@-")],
            Some(body),
        )
        .await
        .map(|_| ())
    }

    async fn approve(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        head: &str,
        body: &str,
    ) -> Result<(), String> {
        self.api(
            account,
            "POST",
            &format!("{}/merge_requests/{number}/approve", project(repo)),
            &[("-f", "sha", head)],
            None,
        )
        .await?;
        if body.trim().is_empty() {
            return Ok(());
        }
        self.comment(account, repo, number, body).await
    }

    async fn file_at(
        &self,
        account: &Account,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>, String> {
        match self
            .get(
                account,
                &format!("{}/repository/files/{}/raw", project(repo), encode(path)),
                &[("ref", git_ref)],
            )
            .await
        {
            Ok(text) => Ok(Some(text)),
            Err(err) if is_not_found(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn compare_patch(
        &self,
        account: &Account,
        repo: &str,
        from: &str,
        to: &str,
        path: &str,
    ) -> Result<Option<String>, String> {
        let compare: CompareJson = match self
            .get_json(
                account,
                &format!("{}/repository/compare", project(repo)),
                &[("from", from), ("to", to)],
            )
            .await
        {
            Ok(compare) => compare,
            Err(err) if is_not_found(&err) => {
                return Err(format!(
                    "compare {from}...{to} is not a valid comparison in {repo}"
                ));
            }
            Err(err) => return Err(err),
        };
        if compare.compare_timeout {
            return Err(format!(
                "compare {from}...{to} timed out, so it cannot show whether {path} changed"
            ));
        }
        let Some(diff) = compare.diffs.into_iter().find(|diff| diff.new_path == path) else {
            return Ok(None);
        };
        if diff.too_large || diff.collapsed {
            return Err(format!(
                "the patch of {path} in compare {from}...{to} is too large for GitLab to send"
            ));
        }
        Ok((!diff.diff.is_empty()).then_some(diff.diff))
    }

    /// The project path in a job URL on this host.
    fn job_project(&self, url: &str) -> Option<String> {
        let rest = url
            .strip_prefix("https://")?
            .strip_prefix(self.host.as_str())?
            .strip_prefix('/')?;
        let (path, _) = rest.split_once("/-/jobs/")?;
        (!path.is_empty()).then(|| path.to_owned())
    }

    async fn failure_report(
        &self,
        account: &Account,
        repo: &str,
        checks: &[FailedCheck],
    ) -> Result<String, String> {
        let mut report = String::new();
        for check in checks {
            let _ = writeln!(report, "Check {:?} failed.", check.name);
            match check.url.as_deref().and_then(gitlab_job_id) {
                Some(job) => {
                    // A fork's pipeline runs in the fork, so read the trace
                    // from the project the job's URL names.
                    let owner = check
                        .url
                        .as_deref()
                        .and_then(|url| self.job_project(url))
                        .map_or_else(|| project(repo), |path| project(&path));
                    match self
                        .get(account, &format!("{owner}/jobs/{job}/trace"), &[])
                        .await
                    {
                        Ok(log) => report.push_str(tail(&log, MAX_LOG)),
                        Err(err) if err.contains(REJECTED) => return Err(err),
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
}

impl Forge for GitLab {
    fn clone_url(&self, repo: &str) -> String {
        format!("https://{}/{repo}.git", self.host)
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
            let out = self.get(&probe, "user", &[]).await.map_err(|err| {
                if err.contains("HTTP 401") || err.contains("HTTP 403") {
                    LookupError::Rejected
                } else {
                    LookupError::Failed(err.replace(token, "<redacted>"))
                }
            })?;
            let user: UserJson = serde_json::from_str(&out).map_err(|_| {
                LookupError::Failed("glab api user returned unexpected output".into())
            })?;
            Ok(ForgeAccount {
                login: user.username,
                id: user.id,
            })
        })
    }

    fn access<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
    ) -> BoxFuture<'a, Result<Access, String>> {
        Box::pin(async move {
            let json: ProjectJson = match self.get_json(account, &project(repo), &[]).await {
                Ok(json) => json,
                Err(err) if is_not_found(&err) => return Ok(Access::NotVisible),
                Err(err) => return Err(err),
            };
            // The higher of the direct and the inherited membership. A
            // public project the account is no member of reads as Read.
            let level = [
                json.permissions.project_access,
                json.permissions.group_access,
            ]
            .into_iter()
            .flatten()
            .map(|level| level.access_level)
            .max()
            .unwrap_or(0);
            Ok(if level >= MAINTAINER {
                Access::Admin
            } else if level >= DEVELOPER {
                Access::Write
            } else {
                Access::Read
            })
        })
    }

    fn may_merge<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        access: Access,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            if matches!(access, Access::NotVisible | Access::Read) {
                return Ok(false);
            }
            // A protected base branch can limit merging to Maintainers, so
            // the merge request's own answer decides.
            let mr = self.mr(account, repo, number).await?;
            Ok(mr.user.is_some_and(|user| user.can_merge))
        })
    }

    fn account(&self, token: String, found: &ForgeAccount) -> Account {
        Account {
            token: Some(token),
            login: found.login.clone(),
            name: found.login.clone(),
            email: format!("{}-{}@users.noreply.{}", found.id, found.login, self.host),
        }
    }

    fn git_auth_header(&self, account: &Account) -> Option<String> {
        let token = account.token.as_ref()?;
        let basic = STANDARD.encode(format!("oauth2:{token}"));
        Some(format!("Authorization: Basic {basic}"))
    }

    fn compare_range(&self, body: &str, upstream: &str) -> Option<(String, String)> {
        let marker = format!(
            "{}/{}/-/compare/",
            self.host.to_ascii_lowercase(),
            upstream.to_ascii_lowercase()
        );
        let start = body.to_ascii_lowercase().find(&marker)? + marker.len();
        let link: String = body[start..]
            .chars()
            .take_while(|c| {
                !c.is_whitespace() && !matches!(c, ')' | ']' | '>' | '"' | '\'' | '?' | '#')
            })
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
        Box::pin(self.first_mr(account, repo, branch, "merged", None))
    }

    fn open_pr<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        base: &'a str,
        branch: &'a str,
        description: &'a PrDescription,
    ) -> BoxFuture<'a, Result<u64, String>> {
        Box::pin(GitLab::open_pr(
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
        Box::pin(GitLab::rerun_failed(self, account, repo, number))
    }

    fn merge<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.merge_request(account, repo, number, None))
    }

    fn renovate_prs<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        base: &'a str,
        author: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<RenovatePr>, String>> {
        Box::pin(async move {
            let author = author.ok_or_else(|| {
                format!(
                    "{} has no fixed Renovate account, so the project needs one",
                    self.host
                )
            })?;
            let list: Vec<MrJson> = self
                .get_all(
                    account,
                    &format!("{}/merge_requests", project(repo)),
                    &[
                        ("state", "opened"),
                        ("target_branch", base),
                        ("author_username", author),
                    ],
                )
                .await?;
            Ok(list
                .into_iter()
                .map(|mr| RenovatePr {
                    number: mr.iid,
                    title: mr.title,
                    url: mr.web_url,
                    head: mr.sha,
                })
                .collect())
        })
    }

    fn pr_info<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<PrInfo, String>> {
        Box::pin(GitLab::pr_info(self, account, repo, number))
    }

    fn head_commit<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<HeadCommit, String>> {
        Box::pin(GitLab::head_commit(self, account, repo, number))
    }

    fn merged_at<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            let mr = self.mr(account, repo, number).await?;
            Ok(mr.merged_at.as_deref().map(sql_time))
        })
    }

    fn pr_details<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<PrDetails, String>> {
        Box::pin(GitLab::pr_details(self, account, repo, number))
    }

    fn request_renovate_rebase<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
    ) -> BoxFuture<'a, Result<RebaseCheckbox, String>> {
        Box::pin(GitLab::request_renovate_rebase(
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
        Box::pin(GitLab::comment(self, account, repo, number, body))
    }

    fn approve<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(GitLab::approve(self, account, repo, number, head, body))
    }

    fn merge_head<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.merge_request(account, repo, number, Some(head)))
    }

    fn file_at<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        path: &'a str,
        git_ref: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(GitLab::file_at(self, account, repo, path, git_ref))
    }

    fn compare_patch<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        from: &'a str,
        to: &'a str,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(GitLab::compare_patch(self, account, repo, from, to, path))
    }

    fn status<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        ignore: &'a [String],
    ) -> BoxFuture<'a, Result<PrStatus, String>> {
        Box::pin(GitLab::status(self, account, repo, number, ignore))
    }

    fn failure_report<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        checks: &'a [FailedCheck],
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(GitLab::failure_report(self, account, repo, checks))
    }
}
