use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use factory_core::{Agent, Event};
use factory_runner::{
    BranchSummary, Credential, Git, GitHub, Observation, Rerun, RunReport, SessionRun,
    WorkerConfig, WorkerRun, backend, observe, place_files, place_images, remove_container, run,
    run_checks, show_files, show_images,
};

use crate::registry::AgentRegistry;
use crate::scheduler::{
    AgentRequest, ChecksRequest, Executor, LogSink, PublishRequest, Published, ReadyAction,
    RerunRequest, SessionSink, SummaryRequest, WatchReport, WatchRequest, ready_action,
    watch_only_event,
};
use crate::settings::Settings;
use crate::store::Store;
use crate::usage::PlanUsageCache;

/// Runs agents in podman worker containers.
///
/// Each job keeps its files under `<data_dir>/jobs/<id>/`: `work/` is the
/// checkout and the agent's config directory, named by the registry, holds the
/// job's sessions.
pub struct PodmanExecutor {
    /// The podman binary, secret and agents directory. The image and limits
    /// come from [`Settings`] on each run.
    pub base: WorkerConfig,
    /// Prepares each job's checkout. It holds the GitHub token, which never
    /// reaches the worker.
    pub git: Git,
    /// Opens pull requests and reads their state with the same token.
    pub github: GitHub,
    pub data_dir: PathBuf,
    /// Resolves each run's agent to its credential, worker image and host config
    /// directory. The run reads the credential when it starts, so a token saved
    /// on the settings page applies to the next run with no factory restart.
    /// When the agent resolves no credential, the run fails before any container
    /// starts.
    pub registry: Arc<AgentRegistry>,
    /// Reads each job's attached files, so a fresh-session run lists them in
    /// the prompt with the original names the store holds.
    pub store: Arc<Store>,
    /// Takes each plan usage report a run makes, for the board tile.
    pub plan_usage: Arc<PlanUsageCache>,
}

/// Writes a credential file, such as Codex's `auth.json`, with mode 0600 so
/// only the worker user reads it. A file left by an earlier crashed run is
/// removed first, so the new one is created with the right mode rather than
/// keeping the old one's.
fn write_credential_file(path: &Path, contents: &str) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

fn failed(reason: String) -> RunReport {
    RunReport {
        event: Event::AgentFailed { reason },
        session_id: None,
        cost_usd: None,
        mcp_unavailable: Vec::new(),
        credential: None,
        token_usage: None,
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

    /// The prompt for a fresh-session run: the task prompt with the job's
    /// images and files placed in the checkout and listed, and a one-time
    /// warning for a Codex session with no `AGENTS.md`. A resumed session skips
    /// all of this, because it has already seen them.
    fn start_session_prompt(
        &self,
        request: &AgentRequest,
        agent: Agent,
        workspace: &Path,
        job_dir: &Path,
    ) -> Result<String, String> {
        let mut prompt = request.prompt.clone();
        let paths = place_images(workspace, &job_images(job_dir))
            .map_err(|err| format!("could not place the images: {err}"))?;
        prompt = show_images(&prompt, &paths);
        let files = self
            .job_files(request.job_id, job_dir)
            .map_err(|err| format!("could not read the files: {err}"))?;
        let placed = place_files(workspace, &files)
            .map_err(|err| format!("could not place the files: {err}"))?;
        prompt = show_files(&prompt, &placed);
        self.warn_missing_agents_md(agent, workspace, request.job_id);
        Ok(prompt)
    }

    /// Warns the job once that a fresh Codex session in a repository with no
    /// `AGENTS.md` at its root runs without the repository's instructions. The
    /// run goes on; only Codex reads `AGENTS.md`, so it is a no-op for Claude.
    fn warn_missing_agents_md(&self, agent: Agent, workspace: &Path, job_id: i64) {
        if agent == Agent::Codex
            && !workspace.join("AGENTS.md").exists()
            && let Err(err) = self.store.append_factory_log(
                job_id,
                "This repository has no AGENTS.md, so Codex runs without its instructions.",
            )
        {
            tracing::error!("job {job_id}: {err}");
        }
    }

    /// Serialises a Codex run while the stored login is due for a refresh and
    /// writes the login into the config directory at mode 0600. Returns the
    /// login written, so the caller carries it on the run, and the run guard,
    /// which the caller holds until the refreshed login is stored.
    async fn place_codex_login<'a>(
        &'a self,
        path: &Path,
        token: String,
    ) -> Result<(String, crate::CodexRun<'a>), String> {
        let run = self
            .registry
            .codex_login()
            .enter_run(token, SystemTime::now())
            .await;
        write_credential_file(path, &run.auth)
            .map_err(|err| format!("could not write the Codex login: {err}"))?;
        Ok((run.auth.clone(), run))
    }

    /// Keeps a login Codex refreshed mid-run, then deletes the file from the
    /// config directory so it stays in a job directory only while a run is in
    /// flight. The store holds the run's gate guard, so a waiting run reads the
    /// refreshed login.
    async fn keep_refreshed_login(&self, path: &Path, credential: Option<&str>) {
        if let Some(refreshed) = credential {
            self.registry.codex_login().store_refreshed(refreshed).await;
        }
        if let Err(err) = fs::remove_file(path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("removing {}: {err}", path.display());
        }
    }

    fn worker_config(&self, settings: &Settings, image: String) -> WorkerConfig {
        WorkerConfig {
            image,
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
        mut session: SessionSink,
    ) -> RunReport {
        let agent = request.agent;
        let entry = self.registry.entry(agent);
        // Resolve the credential before preparing the checkout or starting a
        // container. With none, the run fails here; the agent failure keeps the
        // task as its retry target, so the job page shows Retry and a token
        // saved on the settings page lets Retry run the task.
        let mut token = match entry.credential() {
            Ok(token) => token,
            Err(reason) => return failed(reason),
        };
        let backend = backend(agent).expect("the run's agent has a worker backend");
        let image = entry
            .image(settings)
            .expect("the run's agent has a worker image");
        let config = self.worker_config(settings, image);
        let job_dir = self.job_dir(request.job_id);
        let workspace = job_dir.join("work");
        let config_dir = job_dir.join(
            entry
                .host_config_dir()
                .expect("the run's agent has a host config dir"),
        );
        if let Err(err) = fs::create_dir_all(&config_dir) {
            return failed(format!("could not create {}: {err}", config_dir.display()));
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
        // Claude resumes by the factory UUID; Codex resumes by the thread id it
        // reported on an earlier run, when its rollout file is still there.
        let resume_id = request
            .agent_session
            .as_deref()
            .unwrap_or(&request.session_id);
        let session_run = backend.session_for(&config_dir, resume_id);
        let prompt = if matches!(session_run, SessionRun::Start(_)) {
            match self.start_session_prompt(&request, agent, &workspace, &job_dir) {
                Ok(prompt) => prompt,
                Err(reason) => return failed(reason),
            }
        } else {
            request.prompt
        };
        // A file credential, such as Codex's auth.json, is written into the
        // config directory before the run and read back after. While the stored
        // login is due for a refresh, at most one Codex run is in flight, and a
        // run that waited writes the freshest stored login, so two runs cannot
        // rotate the same refresh token at once.
        let credential_file = match backend.credential() {
            Credential::File(name) => Some(config_dir.join(name)),
            Credential::Env(_) => None,
        };
        let codex_run = match &credential_file {
            Some(path) => match self.place_codex_login(path, token).await {
                Ok((auth, run)) => {
                    token = auth;
                    Some(run)
                }
                Err(reason) => return failed(reason),
            },
            None => None,
        };
        let worker = WorkerRun {
            name,
            agent,
            session: session_run,
            workspace,
            config_dir,
            token,
            expect: request.expect,
            prompt,
            model: request.model,
            effort: request.effort,
            mcp: request.mcp,
        };
        let report = run(
            &config,
            &worker,
            log,
            |usage| {
                self.plan_usage.record_now(usage);
            },
            |id: &str| session(id.to_owned()),
        )
        .await;
        if let Some(path) = &credential_file {
            self.keep_refreshed_login(path, report.credential.as_deref())
                .await;
        }
        drop(codex_run);
        report
    }

    async fn run_checks(
        &self,
        settings: &Settings,
        request: ChecksRequest,
    ) -> Result<Event, String> {
        let config = self.worker_config(settings, settings.worker_image.clone());
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
                ReadyAction::Review => (
                    Some(Event::StartReview {
                        head: status.head.clone(),
                    }),
                    None,
                ),
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
