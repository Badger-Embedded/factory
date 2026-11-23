//! Saving and clearing a forge token with the account it belongs to. The
//! token sits in the secret store and the account login and id sit in plain
//! tables, so a page can say whose token is stored without decrypting it.

use factory_runner::{Forge, ForgeAccount, LookupError};

use crate::secrets::{ForgeTokenOwner, SecretError, SecretStore};
use crate::store::{Store, StoreError};

/// Why a token was not saved. No variant carries the token.
#[derive(Debug)]
pub enum SaveError {
    /// The forge refused the token.
    Rejected,
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
