//! The interface to a git forge: pull requests, checks and the credentials
//! git needs to clone and push. [`GitHub`](crate::GitHub) and
//! [`GitLab`](crate::GitLab) implement it.
//!
//! Every call takes the [`Account`] it runs as, so the caller picks the
//! account per operation and the forge holds no token of its own.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use factory_core::PrDescription;

use crate::github::{
    FailedCheck, HeadCommit, PrDetails, PrInfo, PrStatus, RebaseCheckbox, RenovatePr, Rerun,
};

/// A boxed future, so the trait stays object safe without a macro crate.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A registered Forgejo instance: the name projects and tokens store, and
/// the address of its server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForgejoServer {
    pub name: String,
    /// `https://host[:port][/sub-path]`, with no trailing slash.
    pub base_url: String,
}

/// The kind of forge a project lives on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ForgeKind {
    GitHub,
    Salsa,
    Forgejo(ForgejoServer),
}

impl ForgeKind {
    /// The name stored for the kind. A Forgejo instance is stored under its
    /// registered name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            ForgeKind::GitHub => "github",
            ForgeKind::Salsa => "salsa",
            ForgeKind::Forgejo(server) => &server.name,
        }
    }

    /// The kinds whose address is fixed in code, in the order the pages list
    /// them. The Forgejo instances follow, from the registry.
    pub const FIXED: [ForgeKind; 2] = [ForgeKind::GitHub, ForgeKind::Salsa];

    /// The name pages show for the kind.
    #[must_use]
    pub fn display_name(&self) -> &str {
        match self {
            ForgeKind::GitHub => "GitHub",
            ForgeKind::Salsa => "Salsa",
            ForgeKind::Forgejo(server) => &server.name,
        }
    }

    /// The host the kind's projects live on.
    #[must_use]
    pub fn host(&self) -> String {
        match self {
            ForgeKind::GitHub => "github.com".to_owned(),
            ForgeKind::Salsa => "salsa.debian.org".to_owned(),
            ForgeKind::Forgejo(server) => reqwest::Url::parse(&server.base_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_default(),
        }
    }

    /// The web page of the repository `repo`.
    #[must_use]
    pub fn web_url(&self, repo: &str) -> String {
        match self {
            ForgeKind::Forgejo(server) => format!("{}/{repo}", server.base_url),
            _ => format!("https://{}/{repo}", self.host()),
        }
    }

    /// The web page of pull request `number` of the repository `repo`.
    #[must_use]
    pub fn pr_url(&self, repo: &str, number: u64) -> String {
        match self {
            ForgeKind::GitHub => format!("{}/pull/{number}", self.web_url(repo)),
            ForgeKind::Salsa => format!("{}/-/merge_requests/{number}", self.web_url(repo)),
            ForgeKind::Forgejo(_) => format!("{}/pulls/{number}", self.web_url(repo)),
        }
    }

    /// Whether `repo` has the form the kind's repositories have: `owner/name`
    /// on GitHub and Forgejo, and the full project path of two or more
    /// segments on Salsa. Each segment is non-empty and holds only letters,
    /// digits, `-`, `_` and `.`, and is not `.` or `..`.
    #[must_use]
    pub fn valid_repo(&self, repo: &str) -> bool {
        let segments: Vec<&str> = repo.split('/').collect();
        let count_ok = match self {
            ForgeKind::GitHub | ForgeKind::Forgejo(_) => segments.len() == 2,
            ForgeKind::Salsa => segments.len() >= 2,
        };
        count_ok
            && segments.iter().all(|segment| {
                !segment.is_empty()
                    && *segment != "."
                    && *segment != ".."
                    && segment
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            })
    }

    /// The kind with a fixed address named `name`, or `None` for any other
    /// name, including a Forgejo instance, which only the registry resolves.
    #[must_use]
    pub fn parse(name: &str) -> Option<ForgeKind> {
        match name {
            "github" => Some(ForgeKind::GitHub),
            "salsa" => Some(ForgeKind::Salsa),
            _ => None,
        }
    }

    /// The kind named `name`, where `instance_url` is the base URL of the
    /// registered Forgejo instance of that name, if there is one.
    #[must_use]
    pub fn resolve(name: &str, instance_url: Option<&str>) -> Option<ForgeKind> {
        ForgeKind::parse(name).or_else(|| {
            instance_url.map(|base_url| {
                ForgeKind::Forgejo(ForgejoServer {
                    name: name.to_owned(),
                    base_url: base_url.to_owned(),
                })
            })
        })
    }
}

/// Starts the error of a write the account may no longer make, read just
/// before the write. The factory returns the job to watching instead of
/// retrying the write.
pub const NOT_PERMITTED: &str = "the account may not make this change: ";

/// Starts the error of a forge or git command that failed because the forge
/// refused the credentials. [`rejected_by_forge`] decides this from the
/// command's own output, where the command runs.
pub const REJECTED: &str = "the forge rejected the credentials: ";

/// Whether the stderr of a forge CLI or git says the forge refused the token.
#[must_use]
pub fn rejected_by_forge(stderr: &str) -> bool {
    [
        "HTTP 401",
        "Bad credentials",
        "returned error: 401",
        "Authentication failed",
        "Invalid username or token",
        "401 Unauthorized",
    ]
    .iter()
    .any(|marker| stderr.contains(marker))
}

/// The error text of a failed command: `stderr` with every secret removed,
/// marked with [`REJECTED`] when the forge refused the credentials.
pub(crate) fn command_error(what: &str, stderr: &str, secrets: &[String]) -> String {
    let mut text = stderr.trim().to_owned();
    let mut secrets: Vec<&String> = secrets.iter().filter(|s| !s.is_empty()).collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for secret in secrets {
        text = text.replace(secret.as_str(), "<redacted>");
    }
    let marker = if rejected_by_forge(&text) {
        REJECTED
    } else {
        ""
    };
    format!("{marker}{what} failed: {text}")
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
    /// Where `repo` is cloned from.
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

    /// Whether `account` may merge pull request `number`. `access` is what
    /// the account may do in `repo`, read just before.
    fn may_merge<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        access: Access,
    ) -> BoxFuture<'a, Result<bool, String>>;

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

    /// The open pull requests that Renovate made against `base`. `author` is
    /// the account Renovate runs as. GitHub has a fixed Renovate account and
    /// ignores it, and a forge without one fails when it is `None`.
    fn renovate_prs<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        base: &'a str,
        author: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<RenovatePr>, String>>;

    /// The title, description, head and files of a pull request.
    fn pr_info<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<PrInfo, String>>;

    /// The head commit of a pull request and when it became the head.
    fn head_commit<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<HeadCommit, String>>;

    /// When the pull request merged, or `None` while it is not merged.
    fn merged_at<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<Option<String>, String>>;

    /// As [`Forge::pr_info`], plus the diff.
    fn pr_details<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<PrDetails, String>>;

    /// Asks Renovate to rebase its pull request by ticking the rebase
    /// checkbox in the description, or reports that there is none or that the
    /// head is no longer `head`.
    fn request_renovate_rebase<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
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

    /// The failed checks with the end of each failed job's log. A log that
    /// cannot be fetched is noted in the report, except when the forge
    /// rejects the credentials, which is an error.
    fn failure_report<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        checks: &'a [FailedCheck],
    ) -> BoxFuture<'a, Result<String, String>>;
}
