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
        let range = format!("origin/{base}..HEAD");
        let log = self
            .run(work, &["log", "--format=%h %s%x1f%B%x1e", &range], None)
            .await?;
        Ok(log
            .split('\u{1e}')
            .filter_map(|entry| {
                let (head, body) = entry.trim().split_once('\u{1f}')?;
                body.lines().any(credits_ai).then(|| head.trim().to_owned())
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
