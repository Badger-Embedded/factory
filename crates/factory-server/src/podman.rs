use std::fs;
use std::path::{Path, PathBuf};

use factory_core::Event;
use factory_runner::{
    Git, GitHub, Observation, RunReport, SessionRun, WorkerConfig, WorkerRun, observe,
    place_images, remove_container, run, run_checks, show_images,
};

use crate::scheduler::{
    AgentRequest, ChecksRequest, Executor, LogSink, PublishRequest, WatchReport, WatchRequest,
};
use crate::settings::Settings;

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
}

fn failed(reason: String) -> RunReport {
    RunReport {
        event: Event::AgentFailed { reason },
        session_id: None,
        cost_usd: None,
    }
}

impl PodmanExecutor {
    fn container(job_id: i64) -> String {
        format!("factory-job-{job_id}")
    }

    fn job_dir(&self, job_id: i64) -> PathBuf {
        self.data_dir.join("jobs").join(job_id.to_string())
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
        }
        let worker = WorkerRun {
            name,
            session,
            workspace,
            claude_dir,
            expect: request.expect,
            prompt,
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

    async fn publish(&self, request: PublishRequest) -> Result<u64, String> {
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
    }

    async fn watch(&self, request: WatchRequest) -> Result<WatchReport, String> {
        let status = self.github.status(&request.repo, request.number).await?;
        let mut merged_by_factory = false;
        let (event, seen_until) = match observe(&status, request.seen_until.as_deref()) {
            Observation::Ready if request.auto_merge => {
                self.github.merge(&request.repo, request.number).await?;
                merged_by_factory = true;
                (Some(Event::Merged), None)
            }
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
            Observation::Ready | Observation::Nothing => (None, None),
        };
        Ok(WatchReport {
            event,
            seen_until,
            merged_by_factory,
        })
    }

    async fn stop(&self, job_id: i64) {
        let name = Self::container(job_id);
        if let Err(err) = remove_container(&self.base, &name).await {
            tracing::warn!("removing {name}: {err}");
        }
    }
}
