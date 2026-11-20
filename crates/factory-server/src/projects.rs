use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

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

/// One target repository, read from `projects/<name>/project.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// The GitHub repository as `owner/name`.
    pub repo: String,
    /// The branch pull requests target.
    pub base: String,
    /// The command the factory runs with bash in a worker, in the job's
    /// checkout, before it pushes. Without it, the factory pushes unchecked.
    #[serde(default)]
    pub checks: Option<String>,
    /// Defaults for the project's models. Models saved on the settings page
    /// replace them.
    #[serde(default)]
    pub models: StageModels,
    /// The default for auto-merge: squash-merge a pull request once its checks
    /// pass and no review comment waits for the agent. A saved settings row
    /// replaces it. Off by default.
    #[serde(default)]
    pub auto_merge: bool,
    /// The default for the LLM review before merge. A saved settings row
    /// replaces it. Off by default.
    #[serde(default)]
    pub llm_review: bool,
}

/// A handle that resolves project metadata on each `lookup` or `list`, rather
/// than holding a snapshot taken once at startup. A project file edited while
/// the service runs takes effect on the next call.
#[derive(Debug, Clone)]
pub struct Projects(Source);

#[derive(Debug, Clone)]
enum Source {
    /// Reads `<dir>/<name>/project.toml` on each call.
    Dir(PathBuf),
    /// Holds the projects in memory. Used by tests.
    Memory(Arc<BTreeMap<String, Project>>),
}

#[derive(Debug)]
pub enum ProjectsError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

impl fmt::Display for ProjectsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProjectsError::Read { path, source } => write!(f, "{}: {source}", path.display()),
            ProjectsError::Parse { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for ProjectsError {}

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
    /// A handle backed by `<dir>/<name>/project.toml`. It reads the files on
    /// each call, not at construction, so the next lookup sees an edit.
    #[must_use]
    pub fn from_dir(dir: PathBuf) -> Self {
        Projects(Source::Dir(dir))
    }

    /// The project named `name`, or `None` when none is onboarded under it.
    ///
    /// # Errors
    ///
    /// Fails when the backing store cannot be read or a `project.toml` does
    /// not parse.
    pub fn lookup(&self, name: &str) -> Result<Option<Project>, ProjectsError> {
        match &self.0 {
            Source::Memory(map) => Ok(map.get(name).cloned()),
            Source::Dir(dir) => read_project(&dir.join(name).join("project.toml")),
        }
    }

    /// Every project with its name, in name order.
    ///
    /// # Errors
    ///
    /// Fails when the backing store cannot be read or a `project.toml` does
    /// not parse.
    pub fn list(&self) -> Result<Vec<(String, Project)>, ProjectsError> {
        match &self.0 {
            Source::Memory(map) => Ok(map
                .iter()
                .map(|(name, project)| (name.clone(), project.clone()))
                .collect()),
            Source::Dir(dir) => read_dir(dir),
        }
    }

    /// Every project name, in name order.
    ///
    /// # Errors
    ///
    /// As [`Projects::list`].
    pub fn names(&self) -> Result<Vec<String>, ProjectsError> {
        Ok(self.list()?.into_iter().map(|(name, _)| name).collect())
    }
}

/// Reads one `project.toml`. A missing file is `None`, so a subdirectory
/// without one is not a project.
fn read_project(path: &Path) -> Result<Option<Project>, ProjectsError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ProjectsError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let project = toml::from_str(&text).map_err(|source| ProjectsError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(project))
}

fn read_dir(dir: &Path) -> Result<Vec<(String, Project)>, ProjectsError> {
    let read_err = |path: &Path| {
        let path = path.to_path_buf();
        move |source| ProjectsError::Read { path, source }
    };
    let mut projects = BTreeMap::new();
    for entry in fs::read_dir(dir).map_err(read_err(dir))? {
        let entry = entry.map_err(read_err(dir))?;
        if let Some(project) = read_project(&entry.path().join("project.toml"))? {
            projects.insert(entry.file_name().to_string_lossy().into_owned(), project);
        }
    }
    Ok(projects.into_iter().collect())
}
