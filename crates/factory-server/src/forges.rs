//! The forge selection: one place maps a [`ForgeKind`] to the code that talks
//! to that forge. GitHub and Salsa are built once. A Forgejo forge is built
//! from the instance in the kind, so an instance registered while the service
//! runs needs no restart.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use factory_runner::{Account, Forge, ForgeKind, Forgejo, ForgejoServer, GitLab, forgejo_client};

use crate::github::{RepoChecker, RepoReport};
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
    forgejo_roots: Vec<reqwest::Certificate>,
    forgejo: Arc<Mutex<HashMap<ForgejoServer, Arc<Forgejo>>>>,
}

/// The repository checks of a Forgejo instance, which the onboarding wizard
/// does not run yet.
struct ForgejoChecks;

impl RepoChecker for ForgejoChecks {
    fn check<'a>(
        &'a self,
        _account: &'a Account,
        _repo: &'a str,
        _base: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RepoReport, String>> + Send + 'a>> {
        Box::pin(async { Err("Onboarding a Forgejo repository is not available yet.".to_owned()) })
    }
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
            forgejo_roots: Vec::new(),
            forgejo: Arc::default(),
        }
    }

    /// The same selection trusting `roots` in addition to the system roots
    /// when it talks to a Forgejo server. Tests use it to reach a local
    /// server.
    ///
    /// # Errors
    ///
    /// Returns a message when no HTTP client can be built with `roots`.
    pub fn with_forgejo_roots(
        mut self,
        roots: Vec<reqwest::Certificate>,
    ) -> Result<Forges, String> {
        forgejo_client(&roots)?;
        self.forgejo_roots = roots;
        Ok(self)
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
    pub fn forge(&self, kind: &ForgeKind) -> Arc<dyn Forge> {
        match kind {
            ForgeKind::GitHub => Arc::clone(&self.github),
            ForgeKind::Salsa => Arc::clone(&self.salsa),
            ForgeKind::Forgejo(server) => self.forgejo_forge(server),
        }
    }

    fn forgejo_forge(&self, server: &ForgejoServer) -> Arc<dyn Forge> {
        let mut cache = self.forgejo.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(forge) = cache.get(server) {
            return Arc::clone(forge) as Arc<dyn Forge>;
        }
        let forge = Arc::new(
            Forgejo::new(&server.base_url, &self.forgejo_roots)
                .expect("the roots were checked when they were set"),
        );
        cache.insert(server.clone(), Arc::clone(&forge));
        forge
    }

    /// The repository checks the onboarding wizard runs.
    #[must_use]
    pub fn checker(&self, kind: &ForgeKind) -> Arc<dyn RepoChecker> {
        match kind {
            ForgeKind::GitHub => Arc::clone(&self.github_checker),
            ForgeKind::Salsa => Arc::clone(&self.salsa_checker),
            ForgeKind::Forgejo(_) => Arc::new(ForgejoChecks),
        }
    }
}
