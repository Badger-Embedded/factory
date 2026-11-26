use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use factory_core::BranchSummary;
use tokio::process::Command;

use crate::forge::{Account, Forge, command_error};

/// Gives the remote for one command that sends a credential, resolved when
/// the command starts.
pub type RemoteSource<'a> = dyn Fn() -> Result<Remote, String> + Send + Sync + 'a;

/// Runs git for the factory.
#[derive(Debug, Clone)]
pub struct Git {
    pub git: PathBuf,
}

impl Default for Git {
    fn default() -> Self {
        Git {
            git: PathBuf::from("git"),
        }
    }
}

/// Why a push failed. A rejection means the remote branch holds commits the
/// checkout does not, so a new `prepare_pr` brings them in before the next push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushError {
    Rejected(String),
    Failed(String),
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushError::Rejected(message) | PushError::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PushError {}

/// Where a checkout comes from and who its commits belong to: the clone URL
/// and authentication header from the forge, and the account's commit
/// identity. The header holds the account's token, and git gets it through
/// `GIT_CONFIG_*` environment variables, so it shows up neither in the
/// process list nor in a checkout's `.git/config`.
#[derive(Clone)]
pub struct Remote {
    pub url: String,
    pub auth_header: Option<String>,
    pub user_name: String,
    pub user_email: String,
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote")
            .field("url", &self.url)
            .field("user_name", &self.user_name)
            .field("user_email", &self.user_email)
            .finish_non_exhaustive()
    }
}

impl Remote {
    /// The remote of `repo` on `forge`, used as `account`.
    #[must_use]
    pub fn new(forge: &dyn Forge, account: &Account, repo: &str) -> Remote {
        Remote {
            url: forge.clone_url(repo),
            auth_header: forge.git_auth_header(account),
            user_name: account.name.clone(),
            user_email: account.email.clone(),
        }
    }
}

impl Git {
    /// Makes `work` a checkout of `repo` ready for an agent run, with the
    /// commit identity of the remote's account. The identity is set on every
    /// call, so a checkout made under another account takes the new one.
    /// `remote` is called before each command that sends a credential, so a
    /// token saved between the clone and the fetch reaches the fetch.
    ///
    /// Without a branch, a missing checkout is cloned at `base` and an
    /// existing one is left as it is, so planning runs keep reading the same
    /// commit. With a branch, an existing local branch is checked out with
    /// the agent's commits on it, and a new one starts from the newest
    /// `origin/<base>`. A rebase a stopped run left unfinished is aborted
    /// first, which returns the branch to its commits before the rebase, so
    /// the next run can start the rebase again.
    ///
    /// # Errors
    ///
    /// Returns git's message when a git command fails.
    pub async fn prepare(
        &self,
        work: &Path,
        remote: &RemoteSource<'_>,
        base: &str,
        branch: Option<&str>,
    ) -> Result<(), String> {
        if !work.join(".git").exists() {
            self.clone_into(work, &remote()?, base).await?;
        }
        let Some(branch) = branch else {
            return self.set_identity(work, &remote()?).await;
        };
        let remote = remote()?;
        self.set_identity(work, &remote).await?;
        self.fetch_base(work, &remote, base).await?;
        if ["rebase-merge", "rebase-apply"]
            .iter()
            .any(|dir| work.join(".git").join(dir).exists())
        {
            self.run(work, &["rebase", "--abort"], None).await?;
        }
        let local = format!("refs/heads/{branch}");
        if self
            .run(work, &["rev-parse", "--verify", "--quiet", &local], None)
            .await
            .is_ok()
        {
            self.run(work, &["checkout", branch], None).await?;
        } else {
            let start = format!("origin/{base}");
            self.run(work, &["checkout", "-b", branch, &start], None)
                .await?;
        }
        Ok(())
    }

    /// Like `prepare` with a branch, for a branch that holds an existing pull
    /// request. `head_ref` is the forge's ref of the pull request's head, which
    /// also exists for a fork's pull request. It is fetched into
    /// `refs/factory/pr-<number>/head`, a namespace no branch name can reach. A new local branch starts there. An
    /// existing one keeps its commits: it fast-forwards to a head that moved,
    /// or takes the new head in with a merge commit. A merge that conflicts is
    /// aborted and returned as an error. A merge of the base that an earlier
    /// run left conflicted stays as it is when its branch is checked out, it
    /// already holds the pull request's head and it merges the fetched base,
    /// so the agent can finish it. Otherwise it is aborted and the branch
    /// synchronized, and the caller merges the base again. `origin/<base>` is fetched too, for diffs
    /// against the pull request's own base.
    ///
    /// # Errors
    ///
    /// Returns git's message when a git command fails.
    pub async fn prepare_pr(
        &self,
        work: &Path,
        remote_source: &RemoteSource<'_>,
        base: &str,
        head_ref: &str,
        number: u64,
        branch: &str,
    ) -> Result<(), String> {
        if !work.join(".git").exists() {
            self.clone_into(work, &remote_source()?, base).await?;
        }
        let remote = remote_source()?;
        self.set_identity(work, &remote).await?;
        self.fetch_base(work, &remote, base).await?;
        let tracking = format!("refs/factory/pr-{number}/head");
        let remote = remote_source()?;
        self.fetch_ref(work, &remote, head_ref, &tracking).await?;
        if ["rebase-merge", "rebase-apply"]
            .iter()
            .any(|dir| work.join(".git").join(dir).exists())
        {
            self.run(work, &["rebase", "--abort"], None).await?;
        }
        let mut leave_conflict = false;
        if work.join(".git/MERGE_HEAD").exists() {
            let current = self.run(work, &["branch", "--show-current"], None).await?;
            let base_ref = format!("refs/remotes/origin/{base}");
            let merging = self
                .run(work, &["rev-parse", "MERGE_HEAD"], None)
                .await
                .map(|out| out.trim().to_owned());
            let base_tip = self
                .run(work, &["rev-parse", &base_ref], None)
                .await
                .map(|out| out.trim().to_owned());
            let head_tip = self
                .run(work, &["rev-parse", &tracking], None)
                .await
                .map(|out| out.trim().to_owned());
            let head_seen = self
                .run(
                    work,
                    &["merge-base", "--is-ancestor", &tracking, "HEAD"],
                    None,
                )
                .await
                .is_ok();
            let base_merge = head_seen && merging == base_tip;
            let head_merge = merging == head_tip;
            if current.trim() == branch && merging.is_ok() && (base_merge || head_merge) {
                return Ok(());
            }
            // An older head's merge is redone against the latest head, and a
            // conflict stays in the checkout for the agent.
            leave_conflict = current.trim() == branch
                && match &merging {
                    Ok(old) => self
                        .run(work, &["merge-base", "--is-ancestor", old, &tracking], None)
                        .await
                        .is_ok(),
                    Err(_) => false,
                };
            self.run(work, &["merge", "--abort"], None).await?;
        }
        let local = format!("refs/heads/{branch}");
        if self
            .run(work, &["rev-parse", "--verify", "--quiet", &local], None)
            .await
            .is_err()
        {
            return self
                .run(work, &["checkout", "-b", branch, &tracking], None)
                .await
                .map(|_| ());
        }
        self.run(work, &["checkout", branch], None).await?;
        if self
            .run(
                work,
                &["merge-base", "--is-ancestor", &tracking, "HEAD"],
                None,
            )
            .await
            .is_ok()
        {
            return Ok(());
        }
        let fast_forward = self
            .run(work, &["merge", "--ff-only", &tracking], None)
            .await;
        if fast_forward.is_ok() {
            return Ok(());
        }
        if leave_conflict {
            return self
                .merge_leaving_conflicts(work, &tracking)
                .await
                .map(|_| ());
        }
        self.merge_or_abort(work, &tracking).await
    }

    /// Merges `origin/<base>` into the checked-out branch with a merge commit,
    /// never a rebase. On a conflict the merge stays unfinished in the
    /// checkout, for the agent to resolve, and `Ok(false)` comes back.
    ///
    /// # Errors
    ///
    /// Returns git's message when the merge fails for another reason.
    pub async fn merge_base(&self, work: &Path, base: &str) -> Result<bool, String> {
        self.merge_leaving_conflicts(work, &format!("origin/{base}"))
            .await
    }

    /// Fetches the pull request's head again and merges it into the
    /// checked-out branch with a merge commit, for a push the remote refused
    /// because someone pushed first. On a conflict the merge stays unfinished
    /// in the checkout, for the agent to resolve, and `Ok(false)` comes back.
    ///
    /// # Errors
    ///
    /// Returns git's message when the fetch fails or the merge fails for
    /// another reason.
    pub async fn merge_pr_head(
        &self,
        work: &Path,
        remote_source: &RemoteSource<'_>,
        head_ref: &str,
        number: u64,
    ) -> Result<bool, String> {
        let tracking = format!("refs/factory/pr-{number}/head");
        let remote = remote_source()?;
        self.fetch_ref(work, &remote, head_ref, &tracking).await?;
        self.merge_leaving_conflicts(work, &tracking).await
    }

    /// The commit the checkout is at.
    ///
    /// # Errors
    ///
    /// Returns git's message when it fails.
    pub async fn head(&self, work: &Path) -> Result<String, String> {
        let out = self.run(work, &["rev-parse", "HEAD"], None).await?;
        Ok(out.trim().to_owned())
    }

    async fn merge_leaving_conflicts(&self, work: &Path, target: &str) -> Result<bool, String> {
        match self.run(work, &["merge", "--no-edit", target], None).await {
            Ok(_) => Ok(true),
            Err(err) => {
                let conflicted = self
                    .run(work, &["diff", "--name-only", "--diff-filter=U"], None)
                    .await
                    .is_ok_and(|out| !out.trim().is_empty());
                if conflicted { Ok(false) } else { Err(err) }
            }
        }
    }

    /// Pushes `branch` to `branch_on_remote` without force. The remote refuses
    /// a push that would drop a commit someone else pushed, and that comes
    /// back as `PushError::Rejected`. `pushed` runs once the push succeeds.
    ///
    /// # Errors
    ///
    /// Returns `PushError::Rejected` when the remote branch moved, and
    /// `PushError::Failed` with git's message for any other failure.
    pub async fn push_fast_forward(
        &self,
        work: &Path,
        remote: &RemoteSource<'_>,
        branch: &str,
        branch_on_remote: &str,
        pushed: &(dyn Fn() + Send + Sync),
    ) -> Result<(), PushError> {
        let transfer = tempfile::tempdir().map_err(|err| {
            PushError::Failed(format!("could not create a transfer repository: {err}"))
        })?;
        let git_dir = self
            .init_transfer(transfer.path())
            .await
            .map_err(PushError::Failed)?;
        let local = format!("refs/heads/{branch}");
        let from_checkout = work.to_string_lossy();
        self.run(
            transfer.path(),
            &[
                &git_dir,
                "fetch",
                "--no-tags",
                &from_checkout,
                &format!("+{local}:{local}"),
            ],
            None,
        )
        .await
        .map_err(PushError::Failed)?;
        let pushing = remote().map_err(PushError::Failed)?;
        let refspec = format!("{local}:refs/heads/{branch_on_remote}");
        if let Err(err) = self
            .run(
                transfer.path(),
                &[&git_dir, "push", &pushing.url, &refspec],
                Some(&pushing),
            )
            .await
        {
            // A push that lands between git reading the refs and the server
            // updating one is reported as a generic remote failure, so look at
            // the remote branch before calling this one anything else.
            let moved = err.contains("[rejected]")
                || err.contains("failed to update ref")
                    && self
                        .remote_branch_moved(
                            transfer.path(),
                            &git_dir,
                            remote,
                            branch_on_remote,
                            &local,
                        )
                        .await;
            return Err(if moved {
                PushError::Rejected(format!(
                    "the pull request's branch has new commits, so the push was rejected: {err}"
                ))
            } else {
                PushError::Failed(err)
            });
        }
        pushed();
        let tracking = format!("refs/remotes/origin/{branch_on_remote}");
        let tip = self
            .run(work, &["rev-parse", &local], None)
            .await
            .map_err(PushError::Failed)?;
        self.run(work, &["update-ref", &tracking, tip.trim()], None)
            .await
            .map(|_| ())
            .map_err(PushError::Failed)
    }

    /// Pushes `branch` to the remote's URL. The branch belongs to the
    /// factory, so a rewritten branch replaces the old one, unless someone
    /// else pushed to it since the last fetch.
    ///
    /// The worker writes the checkout's configuration, so the token never
    /// reaches a command that reads it. The branch is copied into a
    /// repository the factory made, without a credential, and pushed from
    /// there to the URL the forge gave. `remote` is called after the copy,
    /// right before the push, and `pushed` runs once the push succeeds.
    ///
    /// # Errors
    ///
    /// Returns git's message when the push fails.
    pub async fn push(
        &self,
        work: &Path,
        remote: &RemoteSource<'_>,
        branch: &str,
        pushed: &(dyn Fn() + Send + Sync),
    ) -> Result<(), String> {
        let tracking = format!("refs/remotes/origin/{branch}");
        let seen = self
            .run(work, &["rev-parse", "--verify", "--quiet", &tracking], None)
            .await
            .map(|out| out.trim().to_owned())
            .unwrap_or_default();
        let transfer = tempfile::tempdir()
            .map_err(|err| format!("could not create a transfer repository: {err}"))?;
        let git_dir = self.init_transfer(transfer.path()).await?;
        let local = format!("refs/heads/{branch}");
        let refspec = format!("+{local}:{local}");
        let from_checkout = work.to_string_lossy();
        self.run(
            transfer.path(),
            &[&git_dir, "fetch", "--no-tags", &from_checkout, &refspec],
            None,
        )
        .await?;
        let lease = format!("--force-with-lease={local}:{seen}");
        let refspec = format!("{local}:{local}");
        let remote = remote()?;
        self.run(
            transfer.path(),
            &[&git_dir, "push", &lease, &remote.url, &refspec],
            Some(&remote),
        )
        .await?;
        pushed();
        let tip = self.run(work, &["rev-parse", &local], None).await?;
        self.run(work, &["update-ref", &tracking, tip.trim()], None)
            .await
            .map(|_| ())
    }

    /// The commits on the checked-out branch, after `origin/<base>`, whose
    /// message credits an AI tool, as `<short sha> <subject>`.
    ///
    /// # Errors
    ///
    /// Returns git's message when `git log` fails.
    pub async fn attributed_commits(&self, work: &Path, base: &str) -> Result<Vec<String>, String> {
        self.attributed_commits_after(work, base, None).await
    }

    /// Like `attributed_commits`, without the commits that pull request
    /// `existing` already holds. Those are someone else's to reword, and a
    /// reword would rewrite history the factory must keep.
    ///
    /// # Errors
    ///
    /// Returns git's message when `git log` fails.
    pub async fn attributed_commits_after(
        &self,
        work: &Path,
        base: &str,
        existing: Option<u64>,
    ) -> Result<Vec<String>, String> {
        let range = format!("origin/{base}..HEAD");
        let held = existing.map(|number| format!("^refs/factory/pr-{number}/head"));
        let mut args = vec!["log", "--format=%h %s%x1f%B%x1e", &range];
        args.extend(held.as_deref());
        let log = self.run(work, &args, None).await?;
        Ok(log
            .split('\u{1e}')
            .filter_map(|entry| {
                let (short, body) = entry.trim().split_once('\u{1f}')?;
                body.lines()
                    .any(credits_ai)
                    .then(|| short.trim().to_owned())
            })
            .collect())
    }

    /// The commits on the checked-out branch after `origin/<base>`, newest
    /// first, and `git diff --stat` of the branch against `origin/<base>`.
    ///
    /// # Errors
    ///
    /// Returns git's message when `git log` or `git diff` fails.
    pub async fn summarize_branch(&self, work: &Path, base: &str) -> Result<BranchSummary, String> {
        let range = format!("origin/{base}..HEAD");
        let log = self
            .run(work, &["log", "--format=%h %s", &range], None)
            .await?;
        let merge_base = format!("origin/{base}...HEAD");
        let diff_stat = self
            .run(work, &["diff", "--stat", &merge_base], None)
            .await?;
        Ok(BranchSummary {
            commits: log.lines().map(str::to_owned).collect(),
            diff_stat: diff_stat.trim_end().to_owned(),
        })
    }

    async fn clone_into(&self, work: &Path, remote: &Remote, base: &str) -> Result<(), String> {
        if work.exists() {
            fs::remove_dir_all(work)
                .map_err(|err| format!("could not remove {}: {err}", work.display()))?;
        }
        let parent = work.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
        let url = &remote.url;
        let target = work.to_string_lossy();
        self.run(
            parent,
            &["clone", "--branch", base, "--", url, &target],
            Some(remote),
        )
        .await?;
        Ok(())
    }

    async fn set_identity(&self, work: &Path, remote: &Remote) -> Result<(), String> {
        self.run(work, &["config", "user.name", &remote.user_name], None)
            .await?;
        self.run(work, &["config", "user.email", &remote.user_email], None)
            .await?;
        Ok(())
    }

    /// Fetches `base` into the checkout's `origin/<base>`. The credential
    /// goes to a repository the factory made, and the checkout then fetches
    /// from that repository without one.
    async fn fetch_base(&self, work: &Path, remote: &Remote, base: &str) -> Result<(), String> {
        let transfer = tempfile::tempdir()
            .map_err(|err| format!("could not create a transfer repository: {err}"))?;
        let git_dir = self.init_transfer(transfer.path()).await?;
        let local = format!("refs/heads/{base}");
        self.run(
            transfer.path(),
            &[
                &git_dir,
                "fetch",
                "--no-tags",
                &remote.url,
                &format!("+{local}:{local}"),
            ],
            Some(remote),
        )
        .await?;
        let tracking = format!("+{local}:refs/remotes/origin/{base}");
        let source = transfer.path().to_string_lossy();
        self.run(work, &["fetch", "--no-tags", &source, &tracking], None)
            .await
            .map(|_| ())
    }

    /// Fetches `source` into the checkout's `target` through a transfer
    /// repository, like `fetch_base`.
    async fn fetch_ref(
        &self,
        work: &Path,
        remote: &Remote,
        source: &str,
        target: &str,
    ) -> Result<(), String> {
        let transfer = tempfile::tempdir()
            .map_err(|err| format!("could not create a transfer repository: {err}"))?;
        let git_dir = self.init_transfer(transfer.path()).await?;
        self.run(
            transfer.path(),
            &[
                &git_dir,
                "fetch",
                "--no-tags",
                &remote.url,
                &format!("+{source}:refs/fetched"),
            ],
            Some(remote),
        )
        .await?;
        let path = transfer.path().to_string_lossy();
        self.run(
            work,
            &[
                "fetch",
                "--no-tags",
                &path,
                &format!("+refs/fetched:{target}"),
            ],
            None,
        )
        .await
        .map(|_| ())
    }

    /// Whether the remote's `branch_on_remote` holds a commit that `local`
    /// does not. A remote that cannot be read counts as not moved.
    async fn remote_branch_moved(
        &self,
        transfer: &Path,
        git_dir: &str,
        remote: &RemoteSource<'_>,
        branch_on_remote: &str,
        local: &str,
    ) -> bool {
        let Ok(remote) = remote() else {
            return false;
        };
        let fetched = self
            .run(
                transfer,
                &[
                    git_dir,
                    "fetch",
                    "--no-tags",
                    &remote.url,
                    &format!("+refs/heads/{branch_on_remote}:refs/remote-head"),
                ],
                Some(&remote),
            )
            .await;
        fetched.is_ok()
            && self
                .run(
                    transfer,
                    &[
                        git_dir,
                        "merge-base",
                        "--is-ancestor",
                        "refs/remote-head",
                        local,
                    ],
                    None,
                )
                .await
                .is_err()
    }

    async fn merge_or_abort(&self, work: &Path, target: &str) -> Result<(), String> {
        match self.run(work, &["merge", "--no-edit", target], None).await {
            Ok(_) => Ok(()),
            Err(err) => {
                let _ = self.run(work, &["merge", "--abort"], None).await;
                Err(err)
            }
        }
    }

    /// Makes a bare repository in `dir`, with no hooks or configuration of
    /// the worker's, and returns the `--git-dir` argument that selects it.
    async fn init_transfer(&self, dir: &Path) -> Result<String, String> {
        let target = dir.to_string_lossy();
        self.run(dir, &["init", "--bare", "--quiet", &target], None)
            .await?;
        Ok(format!("--git-dir={target}"))
    }

    async fn run(
        &self,
        dir: &Path,
        args: &[&str],
        authenticate: Option<&Remote>,
    ) -> Result<String, String> {
        let mut command = Command::new(&self.git);
        command.args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ]);
        if authenticate.is_some() {
            command.args([
                "-c",
                "credential.helper=",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
            ]);
        }
        command
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0");
        let mut secrets = Vec::new();
        if let Some(remote) = authenticate
            && let Some(header) = &remote.auth_header
        {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", header_key(&remote.url))
                .env("GIT_CONFIG_VALUE_0", header);
            secrets = header_secrets(header);
        }
        let output = command
            .output()
            .await
            .map_err(|err| format!("could not start {}: {err}", self.git.display()))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(command_error(
                &format!(
                    "git {}",
                    args.iter()
                        .find(|arg| !arg.starts_with("--"))
                        .unwrap_or(&"")
                ),
                &String::from_utf8_lossy(&output.stderr),
                &secrets,
            ))
        }
    }
}

fn header_key(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) if scheme.starts_with("http") => {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("http.{scheme}://{host}/.extraHeader")
        }
        _ => "http.extraHeader".to_owned(),
    }
}

/// Every form of the credential in a header, for removing it from output: the
/// header, its value, and the value decoded from base64 with its token part.
fn header_secrets(header: &str) -> Vec<String> {
    let mut secrets = vec![header.to_owned()];
    if let Some(value) = header.split_whitespace().last() {
        secrets.push(value.to_owned());
        if let Ok(bytes) = STANDARD.decode(value)
            && let Ok(decoded) = String::from_utf8(bytes)
        {
            if let Some((_, token)) = decoded.split_once(':') {
                secrets.push(token.to_owned());
            }
            secrets.push(decoded);
        }
    }
    secrets
}

/// Whether a commit message line credits an AI tool: a co-author trailer
/// for Claude or Anthropic, or Claude Code's "Generated with" footer.
fn credits_ai(line: &str) -> bool {
    let line = line.trim().to_ascii_lowercase();
    (line.starts_with("co-authored-by:") && (line.contains("claude") || line.contains("anthropic")))
        || line.contains("generated with [claude")
        || line.contains("generated with claude")
}
