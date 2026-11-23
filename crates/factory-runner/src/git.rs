use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use factory_core::BranchSummary;
use tokio::process::Command;

use crate::forge::{Account, Forge};

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
    /// Makes `work` a checkout of `repo` ready for an agent run.
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
        remote: &Remote,
        base: &str,
        branch: Option<&str>,
    ) -> Result<(), String> {
        if !work.join(".git").exists() {
            self.clone_into(work, remote, base).await?;
        }
        let Some(branch) = branch else {
            return Ok(());
        };
        self.run(
            work,
            &["fetch", "origin", base],
            remote.auth_header.as_deref(),
        )
        .await?;
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

    /// Pushes `branch` to `origin`. The branch belongs to the factory, so a
    /// rewritten branch replaces the old one, unless someone else pushed to
    /// it since the last fetch.
    ///
    /// # Errors
    ///
    /// Returns git's message when the push fails.
    pub async fn push(&self, work: &Path, remote: &Remote, branch: &str) -> Result<(), String> {
        let refspec = format!("{branch}:{branch}");
        self.run(
            work,
            &["push", "--force-with-lease", "origin", &refspec],
            remote.auth_header.as_deref(),
        )
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
            remote.auth_header.as_deref(),
        )
        .await?;
        self.run(work, &["config", "user.name", &remote.user_name], None)
            .await?;
        self.run(work, &["config", "user.email", &remote.user_email], None)
            .await?;
        Ok(())
    }

    async fn run(&self, dir: &Path, args: &[&str], auth: Option<&str>) -> Result<String, String> {
        let mut command = Command::new(&self.git);
        command
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0");
        if let Some(header) = auth {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", header);
        }
        let output = command
            .output()
            .await
            .map_err(|err| format!("could not start {}: {err}", self.git.display()))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

/// Whether a commit message line credits an AI tool: a co-author trailer
/// for Claude or Anthropic, or Claude Code's "Generated with" footer.
fn credits_ai(line: &str) -> bool {
    let line = line.trim().to_ascii_lowercase();
    (line.starts_with("co-authored-by:") && (line.contains("claude") || line.contains("anthropic")))
        || line.contains("generated with [claude")
        || line.contains("generated with claude")
}
