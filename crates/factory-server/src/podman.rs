use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use factory_core::Event;
use factory_runner::{
    BranchSummary, Git, GitHub, Observation, Rerun, RunReport, SessionRun, WorkerConfig, WorkerRun,
    observe, place_files, place_images, remove_container, run, run_checks, show_files, show_images,
};

use crate::scheduler::{
    AgentRequest, ChecksRequest, Executor, LogSink, PublishRequest, Published, ReadyAction,
    RerunRequest, SummaryRequest, WatchReport, WatchRequest, ready_action, watch_only_event,
};
use crate::secrets::ClaudeToken;
use crate::settings::Settings;
use crate::store::Store;

/// Runs agents in podman worker containers.
///
/// Each job keeps its files under `<data_dir>/jobs/<id>/`: `work/` is the
/// checkout and `claude/` is the Claude Code config directory that holds
/// the job's sessions.
pub struct PodmanExecutor {
    /// The podman binary, secret and plugin directory. The image and limits
    /// come from [`Settings`] on each run.
    pub base: WorkerConfig,
    /// Prepares each job's checkout. It holds the GitHub token, which never
    /// reaches the worker.
    pub git: Git,
    /// Opens pull requests and reads their state with the same token.
    pub github: GitHub,
    pub data_dir: PathBuf,
    /// Resolves the Claude subscription token for each agent run. The run reads
    /// the current token when it starts, so a token saved on the settings page
    /// applies to the next run with no factory restart. When it resolves none,
    /// the run fails with "no Claude token is set" before any container starts.
    pub token: Arc<ClaudeToken>,
    /// Reads each job's attached files, so a fresh-session run lists them in
    /// the prompt with the original names the store holds.
    pub store: Arc<Store>,
}

fn failed(reason: String) -> RunReport {
    RunReport {
        event: Event::AgentFailed { reason },
        session_id: None,
        cost_usd: None,
        mcp_unavailable: Vec::new(),
    }
}

impl PodmanExecutor {
    fn container(job_id: i64) -> String {
        format!("factory-job-{job_id}")
    }

    fn job_dir(&self, job_id: i64) -> PathBuf {
        self.data_dir.join("jobs").join(job_id.to_string())
    }

    /// Each file saved for a job as its path under `job_dir` and the original
    /// name the store holds, in upload order. A job without files has an empty
    /// list.
    fn job_files(
        &self,
        job_id: i64,
        job_dir: &Path,
    ) -> Result<Vec<(PathBuf, String)>, crate::store::StoreError> {
        let dir = job_dir.join("files");
        Ok(self
            .store
            .files(job_id)?
            .into_iter()
            .map(|file| (dir.join(&file.name), file.original_name))
            .collect())
    }

    fn worker_config(&self, settings: &Settings) -> WorkerConfig {
        WorkerConfig {
            image: settings.worker_image.clone(),
            memory: settings.worker_memory.clone(),
            cpus: settings.worker_cpus.clone(),
            ..self.base.clone()
        }
    }
}

/// The images saved for a job, in name order. A job without images has no
/// `images` directory.
fn job_images(job_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(job_dir.join("images")) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    files.sort();
    files
}

impl Executor for PodmanExecutor {
    async fn run_agent(
        &self,
        settings: &Settings,
        request: AgentRequest,
        log: LogSink,
    ) -> RunReport {
        // Resolve the token before preparing the checkout or starting a
        // container. With none, the run fails here; the agent failure keeps the
        // task as its retry target, so the job page shows Retry and a token
        // saved on the settings page lets Retry run the task.
        let Some(token) = self.token.resolve() else {
            return failed("no Claude token is set".into());
        };
        let config = self.worker_config(settings);
        let job_dir = self.job_dir(request.job_id);
        let workspace = job_dir.join("work");
        let claude_dir = job_dir.join("claude");
        if let Err(err) = fs::create_dir_all(&claude_dir) {
            return failed(format!("could not create {}: {err}", claude_dir.display()));
        }
        if let Err(err) = self
            .git
            .prepare(
                &workspace,
                &request.repo,
                &request.base,
                request.branch.as_deref(),
            )
            .await
        {
            return failed(format!("could not prepare the checkout: {err}"));
        }

        let name = Self::container(request.job_id);
        if let Err(err) = remove_container(&config, &name).await {
            tracing::warn!("removing {name}: {err}");
        }
        let session = SessionRun::for_session(&claude_dir, &request.session_id);
        let mut prompt = request.prompt;
        if matches!(session, SessionRun::Start(_)) {
            match place_images(&workspace, &job_images(&job_dir)) {
                Ok(paths) => prompt = show_images(&prompt, &paths),
                Err(err) => return failed(format!("could not place the images: {err}")),
            }
            let files = match self.job_files(request.job_id, &job_dir) {
                Ok(files) => files,
                Err(err) => return failed(format!("could not read the files: {err}")),
            };
            match place_files(&workspace, &files) {
                Ok(placed) => prompt = show_files(&prompt, &placed),
                Err(err) => return failed(format!("could not place the files: {err}")),
            }
        }
        let worker = WorkerRun {
            name,
            session,
            workspace,
            claude_dir,
            token,
            expect: request.expect,
            prompt,
            model: request.model,
            effort: request.effort,
            mcp: request.mcp,
        };
        run(&config, &worker, log).await
    }

    async fn run_checks(
        &self,
        settings: &Settings,
        request: ChecksRequest,
    ) -> Result<Event, String> {
        let config = self.worker_config(settings);
        let name = Self::container(request.job_id);
        if let Err(err) = remove_container(&config, &name).await {
            tracing::warn!("removing {name}: {err}");
        }
        let workspace = self.job_dir(request.job_id).join("work");
        let attributed = self
            .git
            .attributed_commits(&workspace, &request.base)
            .await?;
        if !attributed.is_empty() {
            return Ok(Event::ChecksFailed {
                report: format!(
                    "These commits credit an AI tool in their message, and GitHub would \
                     copy that into the squash commit on {}:\n\n{}\n\nReword them without \
                     the Co-Authored-By trailer or the \"Generated with\" line, for example \
                     with git rebase -i or git commit --amend.",
                    request.base,
                    attributed.join("\n")
                ),
            });
        }
        run_checks(&config, &name, &workspace, &request.command).await
    }

    async fn summarize(&self, request: SummaryRequest) -> Result<BranchSummary, String> {
        let workspace = self.job_dir(request.job_id).join("work");
        self.git.summarize_branch(&workspace, &request.base).await
    }

    async fn publish(&self, request: PublishRequest) -> Result<Published, String> {
        if let Some(number) = self
            .github
            .merged_pr(&request.repo, &request.branch)
            .await?
        {
            return Ok(Published::AlreadyMerged(number));
        }
        let workspace = self.job_dir(request.job_id).join("work");
        self.git.push(&workspace, &request.branch).await?;
        self.github
            .open_pr(
                &request.repo,
                &request.base,
                &request.branch,
                &request.description,
            )
            .await
            .map(Published::Open)
    }

    async fn watch(&self, request: WatchRequest) -> Result<WatchReport, String> {
        let status = self.github.status(&request.repo, request.number).await?;
        let observation = observe(&status, request.seen_until.as_deref());
        // The review stopped and waits for the user. Report only a merge or
        // close by someone else; take no other action and leave the watermark
        // where it is, so comments posted meanwhile are picked up once the job
        // goes back to watching normally.
        if request.watch_only {
            return Ok(WatchReport {
                event: watch_only_event(&observation),
                seen_until: None,
                merged_by_factory: false,
            });
        }
        let mut merged_by_factory = false;
        let (event, seen_until) = match observation {
            Observation::Ready => match ready_action(
                request.llm_review,
                request.review_approved,
                request.auto_merge,
            ) {
                ReadyAction::Review => (Some(Event::StartReview), None),
                ReadyAction::Merge => {
                    self.github.merge(&request.repo, request.number).await?;
                    merged_by_factory = true;
                    (Some(Event::Merged), None)
                }
                ReadyAction::Wait => (None, None),
            },
            Observation::Merged => (Some(Event::Merged), None),
            Observation::Closed => (Some(Event::ClosedUnmerged), None),
            Observation::Conflicted => (
                Some(Event::Conflicted {
                    base: request.base.clone(),
                }),
                None,
            ),
            Observation::CiFailed(checks) => {
                let log = self.github.failure_report(&request.repo, &checks).await;
                (Some(Event::CiFailed { log }), None)
            }
            Observation::Comments { text, seen_until } => (
                Some(Event::ReviewComments { comments: text }),
                Some(seen_until),
            ),
            Observation::Nothing => (None, None),
        };
        Ok(WatchReport {
            event,
            seen_until,
            merged_by_factory,
        })
    }

    async fn rerun(&self, request: RerunRequest) -> Result<Rerun, String> {
        self.github
            .rerun_failed(&request.repo, request.number)
            .await
    }

    async fn stop(&self, job_id: i64) {
        let name = Self::container(job_id);
        if let Err(err) = remove_container(&self.base, &name).await {
            tracing::warn!("removing {name}: {err}");
        }
    }
}
