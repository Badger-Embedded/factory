//! The interface to a git forge: pull requests, checks and the credentials
//! git needs to clone and push. [`GitHub`](crate::GitHub) is its only
//! implementation.
//!
//! Every call takes the [`Account`] it runs as, so the caller picks the
//! account per operation and the forge holds no token of its own.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use factory_core::PrDescription;

use crate::github::{FailedCheck, PrDetails, PrInfo, PrStatus, RebaseCheckbox, RenovatePr, Rerun};

/// A boxed future, so the trait stays object safe without a macro crate.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The kind of forge a project lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForgeKind {
    GitHub,
}

impl ForgeKind {
    /// The name stored for the kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "github",
        }
    }

    /// Every kind, in the order the pages list them.
    pub const ALL: [ForgeKind; 1] = [ForgeKind::GitHub];

    /// The name pages show for the kind.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "GitHub",
        }
    }

    /// The host the kind's projects live on.
    #[must_use]
    pub fn host(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "github.com",
        }
    }

    /// The web page of the repository `repo`.
    #[must_use]
    pub fn web_url(self, repo: &str) -> String {
        format!("https://{}/{repo}", self.host())
    }

    /// The web page of pull request `number` of the repository `repo`.
    #[must_use]
    pub fn pr_url(self, repo: &str, number: u64) -> String {
        match self {
            ForgeKind::GitHub => format!("{}/pull/{number}", self.web_url(repo)),
        }
    }

    /// The kind named `name`, or `None` for an unknown name.
    #[must_use]
    pub fn parse(name: &str) -> Option<ForgeKind> {
        match name {
            "github" => Some(ForgeKind::GitHub),
            _ => None,
        }
    }
}

/// The forge account an operation runs as: its token and the identity its
/// commits carry.
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    pub token: Option<String>,
    pub login: String,
    /// The commit author name.
    pub name: String,
    /// The commit author email.
    pub email: String,
}

impl Default for Account {
    fn default() -> Self {
        Account {
            token: None,
            login: "moonthebot".into(),
            name: "moonthebot".into(),
            email: "289833623+moonthebot@users.noreply.github.com".into(),
        }
    }
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("login", &self.login)
            .field("name", &self.name)
            .field("email", &self.email)
            .finish()
    }
}

/// The account a token belongs to, as the forge reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeAccount {
    pub login: String,
    pub id: u64,
}

/// Why an account lookup gave no account. Neither variant carries the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupError {
    /// The forge refused the token: HTTP 401 or 403.
    Rejected,
    /// The lookup could not run or its answer was not understood.
    Failed(String),
}

/// What an account may do in a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The repository does not exist or the account cannot see it.
    NotVisible,
    Read,
    Write,
    Admin,
}

/// What the factory asks of a forge. Errors are the forge CLI's message.
pub trait Forge: Send + Sync {
    /// Where `repo` (`owner/name`) is cloned from.
    fn clone_url(&self, repo: &str) -> String;

    /// The account `token` belongs to, read by calling the forge as that
    /// token.
    fn lookup_account<'a>(
        &'a self,
        token: &'a str,
    ) -> BoxFuture<'a, Result<ForgeAccount, LookupError>>;

    /// What `account` may do in `repo`.
    fn access<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
    ) -> BoxFuture<'a, Result<Access, String>>;

    /// The account that runs as `token`, whose commits carry `found`'s
    /// login and noreply address.
    fn account(&self, token: String, found: &ForgeAccount) -> Account;

    /// The HTTP header git sends to authenticate as `account`, if it has a
    /// token.
    fn git_auth_header(&self, account: &Account) -> Option<String>;

    /// The refs of the first link in a pull request description that
    /// compares two refs of `upstream`, which is how Renovate names the
    /// upstream range of an update.
    fn compare_range(&self, body: &str, upstream: &str) -> Option<(String, String)>;

    /// The number of the merged pull request for `branch`, if one exists.
    fn merged_pr<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        branch: &'a str,
    ) -> BoxFuture<'a, Result<Option<u64>, String>>;

    /// Opens the pull request for `branch`, or updates the title and
    /// description of the open one, and returns its number.
    fn open_pr<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        base: &'a str,
        branch: &'a str,
        description: &'a PrDescription,
    ) -> BoxFuture<'a, Result<u64, String>>;

    /// Reruns the failed CI runs on the pull request's head.
    fn rerun_failed<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<Rerun, String>>;

    /// Squash-merges the pull request and deletes its branch.
    fn merge<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<(), String>>;

    /// The open pull requests that Renovate made against `base`.
    fn renovate_prs<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        base: &'a str,
    ) -> BoxFuture<'a, Result<Vec<RenovatePr>, String>>;

    /// The title, description, head and files of a pull request.
    fn pr_info<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<PrInfo, String>>;

    /// As [`Forge::pr_info`], plus the diff.
    fn pr_details<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<PrDetails, String>>;

    /// Asks Renovate to rebase its pull request by ticking the rebase
    /// checkbox in the description, or reports that there is none.
    fn request_renovate_rebase<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<RebaseCheckbox, String>>;

    /// Posts a comment on the pull request.
    fn comment<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        body: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;

    /// Approves the pull request at `head`.
    fn approve<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;

    /// Squash-merges the pull request only if its head is still `head`.
    fn merge_head<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;

    /// The text of `path` at `git_ref`, or `None` when it does not exist.
    fn file_at<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        path: &'a str,
        git_ref: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>>;

    /// The patch of `path` between two refs, or `None` when it is unchanged.
    fn compare_patch<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        from: &'a str,
        to: &'a str,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>>;

    /// The pull request's state, checks and review comments. Comments from
    /// the logins in `ignore` are left out, such as the account the job runs
    /// as.
    fn status<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        ignore: &'a [String],
    ) -> BoxFuture<'a, Result<PrStatus, String>>;

    /// The failed checks with the end of each failed job's log.
    fn failure_report<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        checks: &'a [FailedCheck],
    ) -> BoxFuture<'a, String>;
}
