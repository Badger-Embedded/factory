use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
}

#[derive(Debug, Clone, Default)]
pub struct Projects(BTreeMap<String, Project>);

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

impl FromIterator<(String, Project)> for Projects {
    fn from_iter<I: IntoIterator<Item = (String, Project)>>(iter: I) -> Self {
        Projects(iter.into_iter().collect())
    }
}

impl Projects {
    /// Reads every `<dir>/<name>/project.toml`. A subdirectory without a
    /// `project.toml` is skipped.
    ///
    /// # Errors
    ///
    /// Fails when `dir` cannot be read or a `project.toml` does not parse.
    pub fn load(dir: &Path) -> Result<Self, ProjectsError> {
        let read_err = |path: &Path| {
            let path = path.to_path_buf();
            move |source| ProjectsError::Read { path, source }
        };
        let mut projects = BTreeMap::new();
        for entry in fs::read_dir(dir).map_err(read_err(dir))? {
            let entry = entry.map_err(read_err(dir))?;
            let path = entry.path().join("project.toml");
            if !path.is_file() {
                continue;
            }
            let text = fs::read_to_string(&path).map_err(read_err(&path))?;
            let project = toml::from_str(&text).map_err(|source| ProjectsError::Parse {
                path: path.clone(),
                source,
            })?;
            projects.insert(entry.file_name().to_string_lossy().into_owned(), project);
        }
        Ok(Projects(projects))
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Project> {
        self.0.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}
