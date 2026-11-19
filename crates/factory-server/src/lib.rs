//! The factory's dashboard and job store.

mod pages;
mod podman;
mod projects;
mod scheduler;
mod settings;
mod store;
mod web;

pub use podman::PodmanExecutor;
pub use projects::{Project, Projects, ProjectsError};
pub use scheduler::{AgentRequest, Executor, LogSink, Scheduler, branch_name};
pub use settings::Settings;
pub use store::{JobRow, LogEntry, Store, StoreError};
pub use web::{USER_HEADER, app};
