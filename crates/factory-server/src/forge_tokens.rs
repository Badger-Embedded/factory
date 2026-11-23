//! Saving and clearing a forge token with the account it belongs to. The
//! token sits in the secret store and the account login and id sit in plain
//! tables, so a page can say whose token is stored without decrypting it.

use factory_runner::{Access, Forge, ForgeAccount, LookupError};

use crate::secrets::{ForgeTokenOwner, SecretError, SecretStore};
use crate::store::{Store, StoreError};

/// Why a token was not saved. No variant carries the token.
#[derive(Debug)]
pub enum SaveError {
    /// The forge refused the token.
    Rejected,
    /// The account cannot see the repository, so the token was not stored.
    NotVisible(ForgeAccount),
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

/// What a token saved for a repository belongs to and may do there.
pub struct RepoSave {
    pub account: ForgeAccount,
    /// The account's access to the repository, or `None` when the check could
    /// not run.
    pub access: Option<Access>,
}

/// As [`save`], and reads the account's access to `repo` first. A token whose
/// account cannot see `repo` is not stored. A check that cannot run does not
/// stop the save.
///
/// # Errors
///
/// As [`save`], and [`SaveError::NotVisible`] when the repository is not
/// visible to the account.
pub async fn save_for_repo(
    secrets: &dyn SecretStore,
    forge: &dyn Forge,
    owner: ForgeTokenOwner<'_>,
    token: &str,
    repo: &str,
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
    secrets.put_forge_token(owner, token, &account)?;
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
