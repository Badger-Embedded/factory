//! Picks the account each forge or git operation of a job runs as.

use factory_runner::{Account, ForgeKind};

use crate::projects::Project;

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
    #[must_use]
    pub fn resolve(
        &self,
        _project_name: &str,
        _project: &Project,
        _requested_by: &str,
    ) -> Resolved {
        Resolved {
            forge: ForgeKind::GitHub,
            ignore: vec![self.factory.login.clone()],
            account: self.factory.clone(),
        }
    }
}
