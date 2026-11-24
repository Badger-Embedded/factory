use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use factory_core::DEFAULT_AGENT;
use factory_runner::ForgeKind;
use serde::Deserialize;

use crate::store::{Store, StoreError};

/// The model each stage of a job runs with. `None` means no model is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageModels {
    #[serde(default)]
    pub clarify: Option<String>,
    #[serde(default)]
    pub spec: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub implement: Option<String>,
    #[serde(default)]
    pub review: Option<String>,
}

/// The effort level each stage of a job runs with. Every stage always has one,
/// unlike a model: a project that saves no effort runs all five at the default
/// agent's default effort, which [`StageEfforts::default`] fills in. Effort is
/// never read from `project.toml`; SQLite is its only store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageEfforts {
    pub clarify: String,
    pub spec: String,
    pub plan: String,
    pub implement: String,
    pub review: String,
}

impl Default for StageEfforts {
    fn default() -> Self {
        let level = DEFAULT_AGENT.default_effort().to_owned();
        StageEfforts {
            clarify: level.clone(),
            spec: level.clone(),
            plan: level.clone(),
            implement: level.clone(),
            review: level,
        }
    }
}

/// The agent each stage of a job runs on, as a stable agent id. Every stage
/// always has one: a project that saves no agents runs all five on
/// [`DEFAULT_AGENT`], which [`StageAgents::default`] fills in. A resumed session
/// keeps the agent it started on, so this holds the stage's current choice, not
/// the one a session in flight uses. Agents are never read from `project.toml`;
/// SQLite is their only store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageAgents {
    pub clarify: String,
    pub spec: String,
    pub plan: String,
    pub implement: String,
    pub review: String,
}

impl Default for StageAgents {
    fn default() -> Self {
        let id = DEFAULT_AGENT.id().to_owned();
        StageAgents {
            clarify: id.clone(),
            spec: id.clone(),
            plan: id.clone(),
            implement: id.clone(),
            review: id,
        }
    }
}

/// The forge a project's row names. A name no kind carries is kept, so one bad
/// row is an error for that project and not for the list it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectForge {
    Known(ForgeKind),
    Unknown(String),
}

impl ProjectForge {
    /// The forge of the stored kind name `name`.
    #[must_use]
    pub fn from_name(name: &str) -> ProjectForge {
        ForgeKind::parse(name).map_or_else(|| ProjectForge::Unknown(name.to_owned()), Self::Known)
    }

    /// The kind, or `None` for an unknown name.
    #[must_use]
    pub fn kind(&self) -> Option<&ForgeKind> {
        match self {
            ProjectForge::Known(kind) => Some(kind),
            ProjectForge::Unknown(_) => None,
        }
    }

    /// What to show for a project whose forge is unknown, or `None` when it is known.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        match self {
            ProjectForge::Known(_) => None,
            ProjectForge::Unknown(name) => Some(format!(
                "This project is on the forge {name:?}, which this factory does not support, so none of its jobs can run."
            )),
        }
    }
}

impl From<ForgeKind> for ProjectForge {
    fn from(kind: ForgeKind) -> Self {
        ProjectForge::Known(kind)
    }
}

fn github() -> ProjectForge {
    ProjectForge::Known(ForgeKind::GitHub)
}

/// A project as a page shows it: `id` is the identifier that goes into URLs,
/// form fields and the cookie, `shown` is the name a person reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectLabel {
    pub id: String,
    pub shown: String,
}

impl ProjectLabel {
    /// A label that shows the identifier.
    pub fn plain(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            shown: id.clone(),
            id,
        }
    }
}

/// One target repository. The `projects` table holds `forge`, `repo`, `base`
/// and the optional `checks`. A project's models and its auto-merge and LLM-review
/// flags live in `project_models` and `project_settings`, so a project read
/// from the store carries those three fields at their defaults. The `[models]`,
/// `auto_merge` and `llm_review` fields are read only when a `project.toml` is
/// parsed for the one-time import, see [`import_tomls`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// The forge the repository lives on. A `project.toml` has no such field,
    /// so an import reads `github`.
    #[serde(skip, default = "github")]
    pub forge: ProjectForge,
    /// The repository as the forge names it, `owner/name` on GitHub.
    pub repo: String,
    /// The branch pull requests target.
    pub base: String,
    /// The command the factory runs with bash in a worker, in the job's
    /// checkout, before it pushes. Without it, the factory pushes unchecked.
    #[serde(default)]
    pub checks: Option<String>,
    /// Defaults for the project's models, read from a `project.toml` for the
    /// import. A project read from the store carries [`StageModels::default`].
    #[serde(default)]
    pub models: StageModels,
    /// The auto-merge default read from a `project.toml` for the import. A
    /// project read from the store carries `false`.
    #[serde(default)]
    pub auto_merge: bool,
    /// The LLM-review default read from a `project.toml` for the import. A
    /// project read from the store carries `false`.
    #[serde(default)]
    pub llm_review: bool,
}

/// A handle that resolves project metadata from the store on each `lookup` or
/// `list`, rather than holding a snapshot taken once at startup. A project
/// onboarded while the service runs is visible on the next call, with no
/// restart.
#[derive(Clone)]
pub struct Projects(Source);

#[derive(Clone)]
enum Source {
    /// Reads the `projects` table on each call.
    Db(Arc<Store>),
    /// Holds the projects in memory. Used by tests.
    Memory(Arc<BTreeMap<String, Project>>),
}

impl fmt::Debug for Projects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Source::Db(_) => f.debug_tuple("Projects").field(&"db").finish(),
            Source::Memory(map) => f.debug_tuple("Projects").field(map).finish(),
        }
    }
}

impl Default for Projects {
    fn default() -> Self {
        Projects(Source::Memory(Arc::new(BTreeMap::new())))
    }
}

impl FromIterator<(String, Project)> for Projects {
    fn from_iter<I: IntoIterator<Item = (String, Project)>>(iter: I) -> Self {
        Projects(Source::Memory(Arc::new(iter.into_iter().collect())))
    }
}

impl Projects {
    /// A handle backed by the `projects` table of `store`. It reads the table
    /// on each call, not at construction, so the next lookup sees a project
    /// onboarded in the meantime.
    #[must_use]
    pub fn from_store(store: Arc<Store>) -> Self {
        Projects(Source::Db(store))
    }

    /// The project named `name`, or `None` when none is onboarded under it.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be read.
    pub fn lookup(&self, name: &str) -> Result<Option<Project>, StoreError> {
        match &self.0 {
            Source::Memory(map) => Ok(map.get(name).cloned()),
            Source::Db(store) => store.project(name),
        }
    }

    /// Every project with its name, in name order.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be read.
    pub fn list(&self) -> Result<Vec<(String, Project)>, StoreError> {
        match &self.0 {
            Source::Memory(map) => Ok(map
                .iter()
                .map(|(name, project)| (name.clone(), project.clone()))
                .collect()),
            Source::Db(store) => store.list_projects(),
        }
    }

    /// Every project name, in name order.
    ///
    /// # Errors
    ///
    /// As [`Projects::list`].
    pub fn names(&self) -> Result<Vec<String>, StoreError> {
        Ok(self.list()?.into_iter().map(|(name, _)| name).collect())
    }
}

/// A fault while importing `project.toml` files into the store.
#[derive(Debug)]
pub enum ImportError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    Store(StoreError),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::Read { path, source } => write!(f, "{}: {source}", path.display()),
            ImportError::Parse { path, source } => write!(f, "{}: {source}", path.display()),
            ImportError::Store(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<StoreError> for ImportError {
    fn from(err: StoreError) -> Self {
        ImportError::Store(err)
    }
}

/// Imports each `<dir>/<name>/project.toml` into `store`, once. It is the
/// one-time move from `project.toml` files to the `projects` table.
///
/// The import is a no-op when `store` already has a project, so it runs only
/// on the first start of a database with no projects. A rerun therefore does
/// not duplicate rows or overwrite a later edit. A missing `dir` is also a
/// no-op, so a fresh install with no files to import starts clean.
///
/// Each file's `repo`, `base` and `checks` go into the `projects` table. Its
/// `[models]` go into `project_models` only when they are set, and its
/// `auto_merge` or `llm_review` into `project_settings` only when one is on.
/// A file that sets neither writes no settings row, so
/// [`Store::migrate_global_auto_merge`] still applies the old global
/// auto-merge to an imported project exactly as it did to the file.
///
/// After every file is imported, the files are deleted.
///
/// # Errors
///
/// Fails when `dir` cannot be read, a `project.toml` does not parse, the store
/// rejects a project, or a file cannot be deleted.
pub fn import_tomls(store: &Store, dir: &Path) -> Result<(), ImportError> {
    if !store.list_projects()?.is_empty() {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(ImportError::Read {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let mut found = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|source| ImportError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path().join("project.toml");
        if let Some(project) = read_project(&path)? {
            found.insert(
                entry.file_name().to_string_lossy().into_owned(),
                (path, project),
            );
        }
    }
    for (name, (_, project)) in &found {
        store.create_project(
            name,
            &ForgeKind::GitHub,
            &project.repo,
            &project.base,
            project.checks.as_deref(),
            None,
        )?;
        if project.models != StageModels::default() {
            store.save_project_models(name, &project.models)?;
        }
        if project.auto_merge || project.llm_review {
            store.save_project_settings(name, project.auto_merge, project.llm_review)?;
        }
    }
    for (path, _) in found.values() {
        fs::remove_file(path).map_err(|source| ImportError::Read {
            path: path.clone(),
            source,
        })?;
    }
    Ok(())
}

/// Reads one `project.toml`. A missing file is `None`, so a subdirectory
/// without one is not a project.
fn read_project(path: &Path) -> Result<Option<Project>, ImportError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ImportError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let project = toml::from_str(&text).map_err(|source| ImportError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(project))
}
