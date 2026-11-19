use std::fs;
use std::path::PathBuf;

use factory_core::Event;
use factory_runner::{Git, RunReport, SessionRun, WorkerConfig, WorkerRun, remove_container, run};

use crate::scheduler::{AgentRequest, Executor, LogSink};
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
}

impl Executor for PodmanExecutor {
    async fn run_agent(
        &self,
        settings: &Settings,
        request: AgentRequest,
        log: LogSink,
    ) -> RunReport {
        let config = WorkerConfig {
            image: settings.worker_image.clone(),
            memory: settings.worker_memory.clone(),
            cpus: settings.worker_cpus.clone(),
            ..self.base.clone()
        };
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
        let worker = WorkerRun {
            name,
            session: SessionRun::for_session(&claude_dir, &request.session_id),
            workspace,
            claude_dir,
            expect: request.expect,
            prompt: request.prompt,
        };
        run(&config, &worker, log).await
    }

    async fn stop(&self, job_id: i64) {
        let name = Self::container(job_id);
        if let Err(err) = remove_container(&self.base, &name).await {
            tracing::warn!("removing {name}: {err}");
        }
    }
}
