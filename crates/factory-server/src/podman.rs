use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use factory_core::{Agent, Event, FileText, PackagingContext, PrDiff, RenovateCi};
use factory_runner::{
    Account, BranchSummary, Ci, Credential, ForgeKind, Git, Observation, PrState, RebaseCheckbox,
    Remote, RenovatePr, Rerun, RunReport, SessionRun, WorkerConfig, WorkerRun, backend, observe,
    place_files, place_images, remove_container, run, run_checks, show_files, show_images,
};

use crate::forges::Forges;
use crate::registry::AgentRegistry;
use crate::scheduler::{
    APPROVAL, AgentRequest, ChecksRequest, Executor, LogSink, PackagingRequest, PublishRequest,
    Published, ReadyAction, Rebase, RenovateWatchRequest, RerunRequest, SessionSink,
    SummaryRequest, WatchReport, WatchRequest, ready_action, watch_only_event,
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
    /// Prepares each job's checkout.
    pub git: Git,
    /// The forge of each request, which opens pull requests and reads their
    /// state as the account the request names.
    pub forges: Forges,
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
            && !workspace.join(agent.rules_doc()).exists()
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
        let forge = self.forges.forge(request.forge);
        if let Err(err) = self
            .git
            .prepare(
                &workspace,
                &Remote::new(&*forge, &request.account, &request.repo),
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
        let forge = self.forges.forge(request.forge);
        if let Some(number) = forge
            .merged_pr(&request.account, &request.repo, &request.branch)
            .await?
        {
            return Ok(Published::AlreadyMerged(number));
        }
        let workspace = self.job_dir(request.job_id).join("work");
        let summary = self.git.summarize_branch(&workspace, &request.base).await?;
        if summary.commits.is_empty() {
            return Ok(Published::Empty);
        }
        self.git
            .push(
                &workspace,
                &Remote::new(&*forge, &request.account, &request.repo),
                &request.branch,
            )
            .await?;
        forge
            .open_pr(
                &request.account,
                &request.repo,
                &request.base,
                &request.branch,
                &request.description,
            )
            .await
            .map(Published::Open)
    }

    async fn watch(&self, request: WatchRequest) -> Result<WatchReport, String> {
        let forge = self.forges.forge(request.forge);
        let status = forge
            .status(
                &request.account,
                &request.repo,
                request.number,
                &request.ignore,
            )
            .await?;
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
                    forge
                        .merge(&request.account, &request.repo, request.number)
                        .await?;
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
                let log = forge
                    .failure_report(&request.account, &request.repo, &checks)
                    .await;
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
        self.forges
            .forge(request.forge)
            .rerun_failed(&request.account, &request.repo, request.number)
            .await
    }

    async fn renovate_prs(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        base: &str,
    ) -> Result<Vec<RenovatePr>, String> {
        self.forges
            .forge(forge)
            .renovate_prs(account, repo, base)
            .await
    }

    async fn watch_renovate(&self, request: RenovateWatchRequest) -> Result<Option<Event>, String> {
        let forge = self.forges.forge(request.forge);
        let status = forge
            .status(
                &request.account,
                &request.repo,
                request.number,
                &request.ignore,
            )
            .await?;
        let ci = match status.state {
            PrState::Merged => {
                let event = self
                    .renovate_merged(
                        request.forge,
                        &request.account,
                        &request.repo,
                        request.number,
                        request.manifest_files.as_deref(),
                    )
                    .await?;
                return Ok(Some(event));
            }
            PrState::Closed => return Ok(Some(Event::ClosedUnmerged)),
            PrState::Open => status.ci,
        };
        if request.watch_only && request.known_head.as_deref() == Some(status.head.as_str()) {
            return Ok(None);
        }
        let ci = match ci {
            Ci::NoChecks => RenovateCi::NoChecks,
            Ci::Pending => RenovateCi::Pending,
            Ci::Failed(checks) => RenovateCi::Failed {
                log: forge
                    .failure_report(&request.account, &request.repo, &checks)
                    .await,
            },
            Ci::Passed => {
                let details = if request.verdict_head.as_deref() == Some(status.head.as_str()) {
                    None
                } else {
                    let details = forge
                        .pr_details(&request.account, &request.repo, request.number)
                        .await?;
                    Some(Box::new(PrDiff {
                        diff: details.diff,
                        body: details.body,
                    }))
                };
                RenovateCi::Passed { details }
            }
        };
        Ok(Some(Event::RenovateObserved {
            head: status.head,
            ci,
            limits: request.limits,
            auto_merge: request.auto_merge,
        }))
    }

    async fn comment(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<(), String> {
        self.forges
            .forge(forge)
            .comment(account, repo, number, body)
            .await
    }

    async fn merge_renovate(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        number: u64,
        head: &str,
    ) -> Result<(), String> {
        let forge = self.forges.forge(forge);
        forge.approve(account, repo, number, head, APPROVAL).await?;
        forge.merge_head(account, repo, number, head).await
    }

    async fn request_rebase(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        number: u64,
        head: &str,
    ) -> Result<Rebase, String> {
        Ok(
            match self
                .forges
                .forge(forge)
                .request_renovate_rebase(account, repo, number, head)
                .await?
            {
                RebaseCheckbox::Ticked => Rebase::Requested,
                RebaseCheckbox::Missing => Rebase::NoCheckbox,
                RebaseCheckbox::HeadMoved => Rebase::HeadMoved,
            },
        )
    }

    async fn renovate_head(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<String, String> {
        Ok(self
            .forges
            .forge(forge)
            .pr_info(account, repo, number)
            .await?
            .head)
    }

    async fn renovate_merged(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        number: u64,
        manifest_files: Option<&[String]>,
    ) -> Result<Event, String> {
        let Some(manifest_files) = manifest_files else {
            return Ok(Event::RenovateMerged {
                packaging: false,
                manifest_changed: false,
            });
        };
        let info = self
            .forges
            .forge(forge)
            .pr_info(account, repo, number)
            .await?;
        Ok(Event::RenovateMerged {
            packaging: true,
            manifest_changed: info.files.iter().any(|file| manifest_files.contains(file)),
        })
    }

    async fn gather_packaging(
        &self,
        request: PackagingRequest,
    ) -> Result<PackagingContext, String> {
        let forge = self.forges.forge(request.forge);
        let details = forge
            .pr_details(&request.account, &request.repo, request.number)
            .await?;
        let mut manifests = Vec::new();
        for path in &request.manifest_files {
            let text = forge
                .file_at(&request.account, &request.repo, path, &request.base)
                .await?;
            manifests.push(FileText {
                path: path.clone(),
                text,
            });
        }
        let recorded = (request.verdict_head.as_deref() == Some(details.head.as_str()))
            .then(|| request.from.clone().zip(request.to.clone()))
            .flatten();
        let range = recorded
            .or_else(|| forge.compare_range(&details.body, &request.upstream))
            .or_else(|| manifest_range(&details.diff, &request.manifest_files));
        if range.is_none() && !request.build_config_files.is_empty() {
            return Err(format!(
                "the upstream comparison for pull request #{} is blocked: no recorded verdict, \
                 compare link or manifest version change gives the old and new version",
                request.number
            ));
        }
        let mut upstream = Vec::new();
        if let Some((from, to)) = &range {
            for path in &request.build_config_files {
                let text = forge
                    .compare_patch(&request.account, &request.upstream, from, to, path)
                    .await?;
                upstream.push(FileText {
                    path: path.clone(),
                    text,
                });
            }
        }
        Ok(PackagingContext {
            kind: String::new(),
            upstream_repo: request.upstream,
            pr_title: details.title,
            pr_body: details.body,
            pr_diff: details.diff,
            manifests,
            from: range.as_ref().map(|(from, _)| from.clone()),
            to: range.map(|(_, to)| to),
            upstream,
            existing: Vec::new(),
        })
    }

    async fn stop(&self, job_id: i64) {
        let name = Self::container(job_id);
        if let Err(err) = remove_container(&self.base, &name).await {
            tracing::warn!("removing {name}: {err}");
        }
    }
}

/// Manifest keys that name the upstream source version.
const VERSION_KEYS: [&str; 4] = ["source-tag", "source-commit", "tag", "commit"];

/// The old and new upstream version a pull request diff changes in the
/// manifest files. A replacement block is a run of removed and added lines
/// that context lines, hunk headers and file boundaries end. Within a block,
/// each version key must have exactly one removed and one added line, and
/// only one key in one block may change. `None` otherwise, so a diff that
/// does not say which old value became which new one gives no range.
fn manifest_range(diff: &str, manifest_files: &[String]) -> Option<(String, String)> {
    let mut found = None;
    let mut removed: Vec<(String, String)> = Vec::new();
    let mut added: Vec<(String, String)> = Vec::new();
    let mut in_manifest = false;
    for line in diff.lines().map(Some).chain([None]) {
        let change = line.filter(|line| {
            in_manifest
                && (line.starts_with('-') || line.starts_with('+'))
                && !line.starts_with("---")
                && !line.starts_with("+++")
        });
        if let Some(line) = change {
            if let Some(old) = line.strip_prefix('-') {
                removed.extend(version_field(old));
            } else if let Some(new) = line.strip_prefix('+') {
                added.extend(version_field(new));
            }
            continue;
        }
        if !file_range(&removed, &added, &mut found) {
            return None;
        }
        removed.clear();
        added.clear();
        if let Some(header) = line.and_then(|line| line.strip_prefix("diff --git ")) {
            in_manifest = manifest_files
                .iter()
                .any(|path| header.ends_with(&format!(" b/{path}")));
        }
    }
    found
}

/// Records the version change of one block in `found`. Returns false when
/// the block's changes are ambiguous or a second change appears.
fn file_range(
    removed: &[(String, String)],
    added: &[(String, String)],
    found: &mut Option<(String, String)>,
) -> bool {
    for key in VERSION_KEYS {
        let from: Vec<_> = removed.iter().filter(|(k, _)| k == key).collect();
        let to: Vec<_> = added.iter().filter(|(k, _)| k == key).collect();
        match (from.as_slice(), to.as_slice()) {
            ([], []) => {}
            ([(_, from)], [(_, to)]) if from != to && found.is_none() => {
                *found = Some((from.clone(), to.clone()));
            }
            _ => return false,
        }
    }
    true
}

fn version_field(line: &str) -> Option<(String, String)> {
    let (key, value) = line.split_once(':')?;
    let key = key.trim();
    let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
    (VERSION_KEYS.contains(&key) && !value.is_empty()).then(|| (key.to_owned(), value.to_owned()))
}
