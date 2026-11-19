use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::process::Command;

/// Runs git for the factory. The factory holds the GitHub token, and git
/// gets it through `GIT_CONFIG_*` environment variables, so it shows up
/// neither in the process list nor in a checkout's `.git/config`.
#[derive(Debug, Clone)]
pub struct Git {
    pub git: PathBuf,
    /// Where repositories live, such as `https://github.com`. A repository
    /// `owner/name` is cloned from `<base_url>/owner/name.git`.
    pub base_url: String,
    pub token: Option<String>,
    /// The identity the agent's commits carry.
    pub user_name: String,
    pub user_email: String,
}

impl Default for Git {
    fn default() -> Self {
        Git {
            git: PathBuf::from("git"),
            base_url: "https://github.com".into(),
            token: None,
            user_name: "moonthebot".into(),
            user_email: "289833623+moonthebot@users.noreply.github.com".into(),
        }
    }
}

impl Git {
    #[must_use]
    pub fn url(&self, repo: &str) -> String {
        format!("{}/{repo}.git", self.base_url.trim_end_matches('/'))
    }

    /// Makes `work` a checkout of `repo` ready for an agent run.
    ///
    /// Without a branch, a missing checkout is cloned at `base` and an
    /// existing one is left as it is, so planning runs keep reading the same
    /// commit. With a branch, an existing local branch is checked out with
    /// the agent's commits on it, and a new one starts from the newest
    /// `origin/<base>`.
    ///
    /// # Errors
    ///
    /// Returns git's message when a git command fails.
    pub async fn prepare(
        &self,
        work: &Path,
        repo: &str,
        base: &str,
        branch: Option<&str>,
    ) -> Result<(), String> {
        if !work.join(".git").exists() {
            self.clone_into(work, repo, base).await?;
        }
        let Some(branch) = branch else {
            return Ok(());
        };
        let local = format!("refs/heads/{branch}");
        if self
            .run(work, &["rev-parse", "--verify", "--quiet", &local], false)
            .await
            .is_ok()
        {
            self.run(work, &["checkout", branch], false).await?;
        } else {
            self.run(work, &["fetch", "origin", base], true).await?;
            let start = format!("origin/{base}");
            self.run(work, &["checkout", "-b", branch, &start], false)
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
    pub async fn push(&self, work: &Path, branch: &str) -> Result<(), String> {
        let refspec = format!("{branch}:{branch}");
        self.run(
            work,
            &["push", "--force-with-lease", "origin", &refspec],
            true,
        )
        .await
        .map(|_| ())
    }

    async fn clone_into(&self, work: &Path, repo: &str, base: &str) -> Result<(), String> {
        if work.exists() {
            fs::remove_dir_all(work)
                .map_err(|err| format!("could not remove {}: {err}", work.display()))?;
        }
        let parent = work.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
        let url = self.url(repo);
        let target = work.to_string_lossy();
        self.run(
            parent,
            &["clone", "--branch", base, "--", &url, &target],
            true,
        )
        .await?;
        self.run(work, &["config", "user.name", &self.user_name], false)
            .await?;
        self.run(work, &["config", "user.email", &self.user_email], false)
            .await?;
        Ok(())
    }

    async fn run(&self, dir: &Path, args: &[&str], auth: bool) -> Result<String, String> {
        let mut command = Command::new(&self.git);
        command
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0");
        if auth && let Some(token) = &self.token {
            let basic = STANDARD.encode(format!("x-access-token:{token}"));
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env(
                    "GIT_CONFIG_VALUE_0",
                    format!("Authorization: Basic {basic}"),
                );
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
