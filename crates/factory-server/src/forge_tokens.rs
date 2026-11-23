//! Saving and clearing a forge token with the account it belongs to. The
//! token sits in the secret store and the account login and id sit in plain
//! tables, so a page can say whose token is stored without decrypting it.

use std::time::Duration;

use factory_runner::{Access, Forge, ForgeAccount, ForgeKind, LookupError};

use crate::secrets::{ForgeTokenOwner, SecretError, SecretStore};
use crate::store::{ProjectGuard, Store, StoreError};

/// Why a token was not saved. No variant carries the token.
#[derive(Debug)]
pub enum SaveError {
    /// The forge refused the token.
    Rejected,
    /// The account cannot see the repository, so the token was not stored.
    NotVisible(ForgeAccount),
    /// A project was removed, or the login stopped being a collaborator,
    /// while the save ran.
    Gone,
    /// The account lookup could not run.
    Lookup(String),
    Secrets(SecretError),
    Store(StoreError),
}

impl From<SecretError> for SaveError {
    fn from(err: SecretError) -> Self {
        SaveError::Secrets(err)
    }
}

impl From<StoreError> for SaveError {
    fn from(err: StoreError) -> Self {
        SaveError::Store(err)
    }
}

/// Looks the account up as `token`, then stores the token and the account for
/// `owner` in one transaction. A token the forge rejects is not stored. With the secret store
/// unavailable nothing is looked up or stored.
///
/// # Errors
///
/// Fails when the store is unavailable, when the forge rejects the token or
/// the lookup cannot run, or on a storage error.
pub async fn save(
    secrets: &dyn SecretStore,
    forge: &dyn Forge,
    owner: ForgeTokenOwner<'_>,
    token: &str,
) -> Result<ForgeAccount, SaveError> {
    if !secrets.available() {
        return Err(SecretError::Unavailable.into());
    }
    let account = forge.lookup_account(token).await.map_err(|err| match err {
        LookupError::Rejected => SaveError::Rejected,
        LookupError::Failed(message) => SaveError::Lookup(message),
    })?;
    secrets.put_forge_token(owner, token, &account)?;
    Ok(account)
}

/// As [`save`], and writes only while the project and collaborator in
/// `guard` still exist, so a lookup that outlives its project stores nothing.
///
/// # Errors
///
/// As [`save`], and [`SaveError::Gone`] when the guard no longer holds.
pub async fn save_guarded(
    secrets: &dyn SecretStore,
    forge: &dyn Forge,
    owner: ForgeTokenOwner<'_>,
    token: &str,
    guard: &ProjectGuard<'_>,
) -> Result<ForgeAccount, SaveError> {
    if !secrets.available() {
        return Err(SecretError::Unavailable.into());
    }
    let account = forge.lookup_account(token).await.map_err(|err| match err {
        LookupError::Rejected => SaveError::Rejected,
        LookupError::Failed(message) => SaveError::Lookup(message),
    })?;
    if !secrets.put_project_forge_token(owner, token, &account, guard)? {
        return Err(SaveError::Gone);
    }
    Ok(account)
}

/// What a token saved for a repository belongs to and may do there.
pub struct RepoSave {
    pub account: ForgeAccount,
    /// The account's access to the repository, or `None` when the check could
    /// not run.
    pub access: Option<Access>,
}

/// As [`save`], and reads the account's access to `repo` first. A token whose
/// account cannot see `repo` is not stored. A check that cannot run does not
/// stop the save. The write happens only while the project and collaborator in
/// `guard` still exist.
///
/// # Errors
///
/// As [`save`], [`SaveError::NotVisible`] when the repository is not visible
/// to the account, and [`SaveError::Gone`] when the guard no longer holds.
pub async fn save_for_repo(
    secrets: &dyn SecretStore,
    forge: &dyn Forge,
    owner: ForgeTokenOwner<'_>,
    token: &str,
    repo: &str,
    guard: &ProjectGuard<'_>,
) -> Result<RepoSave, SaveError> {
    if !secrets.available() {
        return Err(SecretError::Unavailable.into());
    }
    let account = forge.lookup_account(token).await.map_err(|err| match err {
        LookupError::Rejected => SaveError::Rejected,
        LookupError::Failed(message) => SaveError::Lookup(message),
    })?;
    let acting = forge.account(token.to_owned(), &account);
    let access = forge.access(&acting, repo).await.ok();
    if access == Some(Access::NotVisible) {
        return Err(SaveError::NotVisible(account));
    }
    if !secrets.put_project_forge_token(owner, token, &account, guard)? {
        return Err(SaveError::Gone);
    }
    Ok(RepoSave { account, access })
}

/// The account whose token is stored for `owner`, or `None` when none is.
///
/// # Errors
///
/// Fails on a storage error.
pub fn stored(
    store: &Store,
    owner: ForgeTokenOwner<'_>,
) -> Result<Option<ForgeAccount>, StoreError> {
    store.forge_account(owner)
}

/// Removes the token and the account of `owner`. It works with the secret
/// store unavailable.
///
/// # Errors
///
/// Fails on a storage error.
pub fn clear(secrets: &dyn SecretStore, owner: ForgeTokenOwner<'_>) -> Result<(), SaveError> {
    secrets.delete_forge_token(owner)?;
    Ok(())
}

/// How long the startup copy waits for the account lookup.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);

/// What [`copy_gh_token`] did, by project name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CopyReport {
    /// Projects that got the token, with the account it belongs to.
    pub copied: Vec<(String, ForgeAccount)>,
    /// Projects whose copy failed. They are tried at the next start.
    pub failed: Vec<String>,
}

/// The one-time move of `GH_TOKEN` into the project token of each `github`
/// project that has none and was not onboarded through the wizard. The copy is
/// recorded per project, so a token a collaborator clears is not refilled. A
/// project that already holds a token is only recorded. With the secret store
/// unavailable it logs one warning and copies nothing. The account lookup runs
/// once, for at most `timeout`, because every copy uses the same token. A
/// lookup that fails or times out leaves every eligible project for the next
/// start.
///
/// The log names projects and account logins, never the token.
pub async fn copy_gh_token(
    store: &Store,
    secrets: &dyn SecretStore,
    forge: &dyn Forge,
    token: &str,
    timeout: Duration,
) -> CopyReport {
    let mut report = CopyReport::default();
    if !secrets.available() {
        tracing::warn!("the secret store is unavailable, GH_TOKEN is not copied into projects");
        return report;
    }
    let projects = match store.list_projects() {
        Ok(projects) => projects,
        Err(err) => {
            tracing::warn!("GH_TOKEN is not copied, the projects cannot be listed: {err}");
            return report;
        }
    };
    let mut looked_up: Option<Result<ForgeAccount, String>> = None;
    for (name, project) in projects {
        if project.forge.kind() != Some(ForgeKind::GitHub) {
            continue;
        }
        let owner = ForgeTokenOwner::Project {
            project: &name,
            forge: ForgeKind::GitHub,
        };
        match store.gh_token_copy_done(&name) {
            Ok(false) => {}
            Ok(true) => continue,
            Err(err) => {
                tracing::warn!("GH_TOKEN is not copied into project {name}: {err}");
                continue;
            }
        }
        match secrets.forge_token(owner) {
            Ok(None) => {}
            Ok(Some(_)) => {
                record(store, &name);
                continue;
            }
            Err(err) => {
                tracing::warn!("GH_TOKEN is not copied into project {name}: {err}");
                continue;
            }
        }
        let found = match &looked_up {
            Some(found) => found,
            None => &*looked_up.insert(lookup(forge, token, timeout).await),
        };
        let account = match found {
            Ok(account) => account,
            Err(why) => {
                tracing::warn!(
                    "GH_TOKEN is not copied into project {name}, the account lookup failed: {why}"
                );
                report.failed.push(name);
                continue;
            }
        };
        if let Err(err) = secrets.put_forge_token(owner, token, account) {
            tracing::warn!("GH_TOKEN is not copied into project {name}: {err}");
            report.failed.push(name);
            continue;
        }
        tracing::info!(
            "copied GH_TOKEN into the token of project {name}, account {}",
            account.login
        );
        report.copied.push((name, account.clone()));
    }
    report
}

async fn lookup(forge: &dyn Forge, token: &str, timeout: Duration) -> Result<ForgeAccount, String> {
    match tokio::time::timeout(timeout, forge.lookup_account(token)).await {
        Ok(Ok(account)) => Ok(account),
        Ok(Err(LookupError::Rejected)) => Err("GitHub rejected the token".to_owned()),
        Ok(Err(LookupError::Failed(message))) => Err(message.replace(token, "<redacted>")),
        Err(_) => Err(format!("no answer within {}s", timeout.as_secs_f32())),
    }
}

fn record(store: &Store, project: &str) {
    if let Err(err) = store.record_gh_token_copy(project) {
        tracing::warn!("could not record the GH_TOKEN copy for project {project}: {err}");
    }
}
