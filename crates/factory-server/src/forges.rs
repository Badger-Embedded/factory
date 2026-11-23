//! The forge selection: one place maps a [`ForgeKind`] to the code that talks
//! to that forge, so a new forge adds a variant and an arm here.

use std::sync::Arc;

use factory_runner::{Forge, ForgeKind, GitLab};

use crate::github::RepoChecker;
use crate::gitlab::GlabRepos;

/// The implementations of each forge kind: the one the jobs use for pull
/// requests and git credentials, and the one the onboarding wizard uses for
/// its repository checks.
#[derive(Clone)]
pub struct Forges {
    github: Arc<dyn Forge>,
    github_checker: Arc<dyn RepoChecker>,
    salsa: Arc<dyn Forge>,
    salsa_checker: Arc<dyn RepoChecker>,
}

impl Forges {
    #[must_use]
    pub fn new(github: Arc<dyn Forge>, github_checker: Arc<dyn RepoChecker>) -> Forges {
        Forges {
            github,
            github_checker,
            salsa: Arc::new(GitLab::default()),
            salsa_checker: Arc::new(GlabRepos {
                gitlab: GitLab::default(),
            }),
        }
    }

    /// The same selection with `salsa` as the forge of Salsa projects.
    #[must_use]
    pub fn with_salsa(mut self, salsa: Arc<dyn Forge>) -> Forges {
        self.salsa = salsa;
        self
    }

    /// The same selection with `checker` as the repository checks of Salsa
    /// projects.
    #[must_use]
    pub fn with_salsa_checker(mut self, checker: Arc<dyn RepoChecker>) -> Forges {
        self.salsa_checker = checker;
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
            ForgeKind::Salsa => Arc::clone(&self.salsa_checker),
        }
    }
}
