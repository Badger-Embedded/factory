//! Picks the account each forge or git operation of a job runs as.

use factory_runner::{Account, ForgeKind};

use crate::projects::Project;
use crate::store::StoreError;

#[derive(Debug, Clone)]
pub struct Resolved {
    pub forge: ForgeKind,
    pub account: Account,
    /// The logins whose pull request comments are not review feedback.
    pub ignore: Vec<String>,
}

/// Resolves the account of an operation when it starts, so a change to the
/// accounts applies from the next operation.
pub struct Accounts {
    factory: Account,
}

impl Accounts {
    #[must_use]
    pub fn new(factory: Account) -> Accounts {
        Accounts { factory }
    }

    /// The account a job of `project` requested by `requested_by` runs as.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::UnknownForge`] when the project's forge is not
    /// one the factory supports.
    pub fn resolve(
        &self,
        project_name: &str,
        project: &Project,
        _requested_by: &str,
    ) -> Result<Resolved, StoreError> {
        let forge = project
            .forge
            .kind()
            .ok_or_else(|| StoreError::UnknownForge(project_name.to_owned()))?;
        Ok(Resolved {
            forge,
            ignore: vec![self.factory.login.clone()],
            account: self.factory.clone(),
        })
    }
}
