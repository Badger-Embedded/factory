//! The forge selection: one place maps a [`ForgeKind`] to the code that talks
//! to that forge, so a new forge adds a variant and an arm here.

use std::sync::Arc;

use std::future::Future;
use std::pin::Pin;

use factory_runner::{Account, Forge, ForgeKind, GitLab};

use crate::github::{RepoChecker, RepoReport};

/// The repository checks of a forge the wizard cannot onboard yet. It makes
/// no forge call.
struct NoOnboarding(ForgeKind);

impl RepoChecker for NoOnboarding {
    fn check<'a>(
        &'a self,
        _account: &'a Account,
        _repo: &'a str,
        _base: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RepoReport, String>> + Send + 'a>> {
        let name = self.0.display_name();
        Box::pin(async move { Err(format!("onboarding a {name} project is not available yet")) })
    }
}

/// The implementations of each forge kind: the one the jobs use for pull
/// requests and git credentials, and the one the onboarding wizard uses for
/// its repository checks.
#[derive(Clone)]
pub struct Forges {
    github: Arc<dyn Forge>,
    github_checker: Arc<dyn RepoChecker>,
    salsa: Arc<dyn Forge>,
}

impl Forges {
    #[must_use]
    pub fn new(github: Arc<dyn Forge>, github_checker: Arc<dyn RepoChecker>) -> Forges {
        Forges {
            github,
            github_checker,
            salsa: Arc::new(GitLab::default()),
        }
    }

    /// The same selection with `salsa` as the forge of Salsa projects.
    #[must_use]
    pub fn with_salsa(mut self, salsa: Arc<dyn Forge>) -> Forges {
        self.salsa = salsa;
        self
    }

    /// The forge that runs a job's pull request and git operations.
    #[must_use]
    pub fn forge(&self, kind: ForgeKind) -> Arc<dyn Forge> {
        match kind {
            ForgeKind::GitHub => Arc::clone(&self.github),
            ForgeKind::Salsa => Arc::clone(&self.salsa),
        }
    }

    /// The repository checks the onboarding wizard runs.
    #[must_use]
    pub fn checker(&self, kind: ForgeKind) -> Arc<dyn RepoChecker> {
        match kind {
            ForgeKind::GitHub => Arc::clone(&self.github_checker),
            ForgeKind::Salsa => Arc::new(NoOnboarding(kind)),
        }
    }
}
