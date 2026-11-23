//! Picks the account each forge or git operation of a job runs as.

use std::sync::Arc;

use factory_runner::{Account, ForgeAccount, ForgeKind};

use crate::forges::Forges;
use crate::projects::Project;
use crate::secrets::{ForgeTokenOwner, SecretError, SecretStore};
use crate::store::{FACTORY_REQUESTER, Store, StoreError};

/// Whose token an operation runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// The token stored on the project's settings page.
    Project { project: String },
    /// The personal token of the job's requester.
    Requester { login: String, forge: ForgeKind },
    /// The optional token that reads the packaging upstream.
    Upstream { project: String },
}

impl Owner {
    /// What the job log calls the account.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Owner::Project { .. } => "project account",
            Owner::Requester { .. } => "requester's account",
            Owner::Upstream { .. } => "upstream account",
        }
    }

    /// Whose token it is and the page that replaces it.
    fn token_and_page(&self) -> (String, String) {
        match self {
            Owner::Project { project } => (
                format!("the project token of {project}"),
                format!("/settings/projects/{project}"),
            ),
            Owner::Upstream { project } => (
                format!("the upstream token of {project}"),
                format!("/settings/projects/{project}"),
            ),
            Owner::Requester { login, forge } => (
                format!("the {} token of {login}", forge.display_name()),
                format!("/settings/forges/{}", forge.as_str()),
            ),
        }
    }

    /// The message for an operation the forge refused because of the token.
    #[must_use]
    pub fn rejected(&self) -> String {
        let (token, page) = self.token_and_page();
        format!("The forge rejected {token}. Save a new one on {page}")
    }
}

/// Starts the error of an operation that failed because of the token it ran
/// with, as opposed to a failure that trying again may fix. The scheduler
/// fails the job on it, so the job shows Retry, where any other error waits
/// for the next pass.
const CREDENTIALS: &str = "credentials: ";

/// `reason` as the error of an operation that cannot run with its token.
#[must_use]
pub fn credential_failure(reason: &str) -> String {
    format!("{CREDENTIALS}{reason}")
}

/// The reason in an error made by [`credential_failure`], or `None` for any
/// other error.
#[must_use]
pub fn credential_reason(err: &str) -> Option<&str> {
    err.strip_prefix(CREDENTIALS)
}

/// `result` with a refusal of the credentials, which the forge or git command
/// reported where it ran, turned into a credential failure that names whose
/// token it was. Any other error stays as it is.
///
/// # Errors
///
/// Returns the error of `result`.
pub fn blame<T>(owner: &Owner, result: Result<T, String>) -> Result<T, String> {
    result.map_err(|err| {
        if err.starts_with(factory_runner::REJECTED) {
            credential_failure(&owner.rejected())
        } else {
            err
        }
    })
}

/// The account one forge or git operation runs as.
#[derive(Debug, Clone)]
pub struct Operation {
    pub forge: ForgeKind,
    pub account: Account,
    pub owner: Owner,
    /// The logins whose pull request comments are not review feedback: the
    /// account the operation runs as and the project's own account.
    pub ignore: Vec<String>,
}

/// The account a job runs as at the moment a step starts, and the source the
/// executor asks again before each forge or git operation of the step.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub forge: ForgeKind,
    pub account: Account,
    pub owner: Owner,
    pub ignore: Vec<String>,
    pub source: AccountSource,
}

/// Resolves the account of a job again each time it is asked, so a token or
/// switch changed between two operations of one step applies to the second.
#[derive(Clone)]
pub struct AccountSource(Arc<dyn Fn() -> Result<Operation, ResolveError> + Send + Sync>);

impl AccountSource {
    pub fn new(
        resolve: impl Fn() -> Result<Operation, ResolveError> + Send + Sync + 'static,
    ) -> Self {
        AccountSource(Arc::new(resolve))
    }

    /// A source that always gives `account` on GitHub, for tests.
    #[must_use]
    pub fn fixed(account: Account) -> Self {
        let operation = Operation {
            forge: ForgeKind::GitHub,
            ignore: vec![account.login.clone()],
            account,
            owner: Owner::Project {
                project: String::new(),
            },
        };
        AccountSource::new(move || Ok(operation.clone()))
    }

    /// The account for the operation that starts now.
    ///
    /// # Errors
    ///
    /// Returns a credential failure when no token can be used, and the store's
    /// message when the store fails.
    pub fn resolve(&self) -> Result<Operation, String> {
        (self.0)().map_err(|err| match err {
            ResolveError::Store(err) => err.to_string(),
            ResolveError::Account(reason) => credential_failure(&reason),
        })
    }
}

impl std::fmt::Debug for AccountSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccountSource")
    }
}

impl PartialEq for AccountSource {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for AccountSource {}

/// Why an operation has no account.
#[derive(Debug)]
pub enum ResolveError {
    Store(StoreError),
    /// The account is missing or unreadable. The message says where to fix
    /// it and never holds a token.
    Account(String),
}

impl From<StoreError> for ResolveError {
    fn from(err: StoreError) -> Self {
        ResolveError::Store(err)
    }
}

/// Which credential reads a packaging upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamCredential {
    /// The upstream token the project stores, with the account it belongs to.
    Upstream(ForgeAccount),
    /// The project token, for an upstream on the project's own forge.
    Project(ForgeAccount),
    /// No token: the read is unauthenticated.
    None,
}

/// The account that reads a packaging upstream and which credential it is.
#[derive(Debug, Clone)]
pub struct UpstreamRead {
    pub account: Account,
    pub owner: Owner,
    pub credential: UpstreamCredential,
}

/// Resolves the account of an operation when it starts, so a change to a
/// token or to the "Use my account" switch applies from the next operation.
#[derive(Clone)]
pub struct Accounts {
    store: Arc<Store>,
    secrets: Arc<dyn SecretStore>,
    forges: Forges,
}

impl Accounts {
    #[must_use]
    pub fn new(store: Arc<Store>, secrets: Arc<dyn SecretStore>, forges: Forges) -> Accounts {
        Accounts {
            store,
            secrets,
            forges,
        }
    }

    /// The account a job of the project `project_name` requested by
    /// `requested_by` runs as: the requester's own token when the requester is
    /// a collaborator with "Use my account" on and a token for the project's
    /// forge, the project token otherwise. A job of the factory itself always
    /// runs as the project token.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::UnknownForge`] when the project's forge is not
    /// one the factory supports, and with [`ResolveError::Account`] when the
    /// project token is missing or a token cannot be read.
    pub fn resolve(
        &self,
        project_name: &str,
        project: &Project,
        requested_by: &str,
    ) -> Result<Resolved, ResolveError> {
        let operation = self.operation(project_name, project, requested_by)?;
        Ok(Resolved {
            forge: operation.forge,
            account: operation.account,
            owner: operation.owner,
            ignore: operation.ignore,
            source: self.source(project_name, project, requested_by),
        })
    }

    /// A source that resolves the account of the same job again on each ask.
    #[must_use]
    pub fn source(
        &self,
        project_name: &str,
        project: &Project,
        requested_by: &str,
    ) -> AccountSource {
        let accounts = self.clone();
        let (name, project, requested_by) = (
            project_name.to_owned(),
            project.clone(),
            requested_by.to_owned(),
        );
        AccountSource::new(move || accounts.operation(&name, &project, &requested_by))
    }

    /// A source that picks the credential for reading the upstream of
    /// `project_name` on `upstream` each time it is asked.
    #[must_use]
    pub fn upstream_source(
        &self,
        project_name: &str,
        project: &Project,
        upstream: ForgeKind,
    ) -> AccountSource {
        let accounts = self.clone();
        let (name, project) = (project_name.to_owned(), project.clone());
        AccountSource::new(move || {
            let read = accounts.upstream(&name, &project, upstream)?;
            Ok(Operation {
                forge: upstream,
                account: read.account,
                owner: read.owner,
                ignore: Vec::new(),
            })
        })
    }

    /// The credential that reads the upstream on `upstream`: the upstream
    /// token when one is stored, the project token when the upstream is on the
    /// project's own forge, and no token otherwise, because most upstreams are
    /// public. A token is read from the secret store only when its account is
    /// recorded, so a public read works with the store unavailable.
    ///
    /// # Errors
    ///
    /// Fails with [`ResolveError::Account`] when a stored token cannot be read.
    pub fn upstream(
        &self,
        project_name: &str,
        project: &Project,
        upstream: ForgeKind,
    ) -> Result<UpstreamRead, ResolveError> {
        let forge = self.forges.forge(upstream);
        let own = ForgeTokenOwner::Upstream {
            project: project_name,
            forge: upstream,
        };
        if self.store.forge_account(own)?.is_some() {
            let whose = Owner::Upstream {
                project: project_name.to_owned(),
            };
            if let Some((token, found)) = self.read(own, &whose)? {
                return Ok(UpstreamRead {
                    account: forge.account(token, &found),
                    owner: whose,
                    credential: UpstreamCredential::Upstream(found),
                });
            }
        }
        let kind = project
            .forge
            .kind()
            .ok_or_else(|| StoreError::UnknownForge(project_name.to_owned()))?;
        if kind == upstream {
            let owner = ForgeTokenOwner::Project {
                project: project_name,
                forge: kind,
            };
            if self.store.forge_account(owner)?.is_some() {
                let whose = Owner::Project {
                    project: project_name.to_owned(),
                };
                if let Some((token, found)) = self.read(owner, &whose)? {
                    return Ok(UpstreamRead {
                        account: forge.account(token, &found),
                        owner: whose,
                        credential: UpstreamCredential::Project(found),
                    });
                }
            }
        }
        Ok(UpstreamRead {
            account: Account {
                token: None,
                login: String::new(),
                name: String::new(),
                email: String::new(),
            },
            owner: Owner::Upstream {
                project: project_name.to_owned(),
            },
            credential: UpstreamCredential::None,
        })
    }

    fn operation(
        &self,
        project_name: &str,
        project: &Project,
        requested_by: &str,
    ) -> Result<Operation, ResolveError> {
        let kind = project
            .forge
            .kind()
            .ok_or_else(|| StoreError::UnknownForge(project_name.to_owned()))?;
        let forge = self.forges.forge(kind);
        let project_owner = ForgeTokenOwner::Project {
            project: project_name,
            forge: kind,
        };
        let project_login = self
            .store
            .forge_account(project_owner)?
            .map(|account| account.login);

        let requester = Owner::Requester {
            login: requested_by.to_owned(),
            forge: kind,
        };
        let personal = if requested_by != FACTORY_REQUESTER
            && self.store.is_collaborator(project_name, requested_by)?
            && self.store.use_my_account(project_name, requested_by)?
        {
            let stored = ForgeTokenOwner::User {
                login: requested_by,
                forge: kind,
            };
            self.read(stored, &requester)?
        } else {
            None
        };
        let (token, found, owner) = if let Some((token, found)) = personal {
            (token, found, requester)
        } else {
            let owner = Owner::Project {
                project: project_name.to_owned(),
            };
            let Some((token, found)) = self.read(project_owner, &owner)? else {
                return Err(ResolveError::Account(format!(
                    "Project {project_name} has no project token. Save one on /settings/projects/{project_name}"
                )));
            };
            (token, found, owner)
        };
        let account = forge.account(token, &found);
        let mut ignore = vec![account.login.clone()];
        if let Some(login) = project_login
            && !ignore.contains(&login)
        {
            ignore.push(login);
        }
        Ok(Operation {
            forge: kind,
            account,
            owner,
            ignore,
        })
    }

    fn read(
        &self,
        owner: ForgeTokenOwner<'_>,
        whose: &Owner,
    ) -> Result<Option<(String, factory_runner::ForgeAccount)>, ResolveError> {
        match self.secrets.forge_token_with_account(owner) {
            Ok(found) => Ok(found),
            Err(SecretError::Store(err)) => Err(ResolveError::Store(err)),
            Err(SecretError::Unavailable) => Err(ResolveError::Account(
                "No forge token can be read, because the secret store is unavailable".into(),
            )),
            Err(err) => {
                let (token, page) = whose.token_and_page();
                Err(ResolveError::Account(format!(
                    "Reading {token} failed: {err}. Save it again on {page}"
                )))
            }
        }
    }
}
